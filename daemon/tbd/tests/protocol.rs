//! Contract tests: daemon plus a fake extension, no Thunderbird needed.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tb_client::{Client, ClientError};
use tb_protocol::{ConnectionInfo, Frame, Hello, PROTOCOL_VERSION, Role, code};
use tb_testkit::{
    BIG_BODY_BYTES, BIG_MESSAGE, FakeConfig, FakeExtension, HANG_MESSAGE, INBOX, TestDaemon,
};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, ORIGIN, SEC_WEBSOCKET_PROTOCOL};

async fn client(info: &ConnectionInfo) -> Client {
    Client::connect(info, "test")
        .await
        .expect("client connects")
}

fn remote_code(err: ClientError) -> String {
    match err {
        ClientError::Remote(body) => body.code,
        other => panic!("expected a remote error, got {other:?}"),
    }
}

/// Handshake status of a raw WebSocket attempt: `Ok` when upgraded, else the HTTP status.
async fn raw_attempt(
    info: &ConnectionInfo,
    header: Option<(&'static str, String)>,
    origin: Option<&str>,
) -> Result<(), u16> {
    let mut request = info.ws_url().into_client_request().unwrap();
    match header {
        Some(("bearer", token)) => {
            request
                .headers_mut()
                .insert(AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
        }
        Some(("subprotocol", token)) => {
            request.headers_mut().insert(
                SEC_WEBSOCKET_PROTOCOL,
                format!("tb.token.{token}").parse().unwrap(),
            );
        }
        _ => {}
    }
    if let Some(origin) = origin {
        request
            .headers_mut()
            .insert(ORIGIN, origin.parse().unwrap());
    }
    match tokio_tungstenite::connect_async(request).await {
        Ok(_) => Ok(()),
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            Err(response.status().as_u16())
        }
        Err(other) => panic!("unexpected error: {other}"),
    }
}

#[tokio::test]
async fn wrong_or_missing_token_is_rejected() {
    let daemon = TestDaemon::start().await.unwrap();
    let info = daemon.info();
    assert_eq!(raw_attempt(info, None, None).await, Err(401));
    assert_eq!(
        raw_attempt(info, Some(("bearer", "0".repeat(64))), None).await,
        Err(401)
    );
    assert_eq!(
        raw_attempt(info, Some(("subprotocol", "0".repeat(64))), None).await,
        Err(401)
    );
    assert_eq!(
        raw_attempt(info, Some(("bearer", String::new())), None).await,
        Err(401)
    );
    // The right token works in both forms.
    assert_eq!(
        raw_attempt(info, Some(("bearer", info.token.clone())), None).await,
        Ok(())
    );
    assert_eq!(
        raw_attempt(info, Some(("subprotocol", info.token.clone())), None).await,
        Ok(())
    );
}

#[tokio::test]
async fn web_page_origin_is_rejected_even_with_the_token() {
    let daemon = TestDaemon::start().await.unwrap();
    let info = daemon.info();
    let token = info.token.clone();
    assert_eq!(
        raw_attempt(
            info,
            Some(("bearer", token.clone())),
            Some("https://evil.example")
        )
        .await,
        Err(403)
    );
    assert_eq!(
        raw_attempt(info, Some(("bearer", token)), Some("moz-extension://1234")).await,
        Ok(())
    );
}

#[tokio::test]
async fn token_changes_with_every_daemon_start() {
    let a = TestDaemon::start().await.unwrap();
    let b = TestDaemon::start().await.unwrap();
    assert_ne!(a.info().token, b.info().token);
    assert_eq!(
        ConnectionInfo::read(&a.connection_file()).unwrap(),
        *a.info()
    );
}

#[tokio::test]
async fn daemon_refuses_non_loopback_bind() {
    let result = tbd::start(tbd::Config {
        bind: "0.0.0.0:0".parse().unwrap(),
        ..tbd::Config::default()
    })
    .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn requests_reach_the_extension_and_answers_come_back() {
    let (daemon, ext) = TestDaemon::with_extension(FakeConfig::default())
        .await
        .unwrap();
    let c = client(daemon.info()).await;
    assert!(c.welcome().extension_connected);

    let accounts = c.request("listAccounts", Value::Null, None).await.unwrap();
    assert_eq!(accounts.as_array().unwrap().len(), 2);

    let found = c
        .request("searchMessages", json!({"query": "report"}), None)
        .await
        .unwrap();
    assert_eq!(found[0]["id"], "m1@example.com");

    let msg = c
        .request(
            "getMessage",
            json!({"messageId": "m1@example.com", "folderPath": INBOX}),
            None,
        )
        .await
        .unwrap();
    assert!(msg["body"].as_str().unwrap().contains("Quarterly report"));

    let calls = ext.calls();
    assert_eq!(
        calls.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(),
        ["listAccounts", "searchMessages", "getMessage"]
    );
}

#[tokio::test]
async fn errors_from_the_extension_keep_their_code() {
    let (daemon, _ext) = TestDaemon::with_extension(FakeConfig::default())
        .await
        .unwrap();
    let c = client(daemon.info()).await;
    let err = c
        .request(
            "getMessage",
            json!({"messageId": "nope", "folderPath": INBOX}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(remote_code(err), "not_found");
}

#[tokio::test]
async fn invalid_requests_are_rejected_by_the_daemon() {
    let (daemon, ext) = TestDaemon::with_extension(FakeConfig::default())
        .await
        .unwrap();
    let c = client(daemon.info()).await;
    assert_eq!(
        remote_code(c.request("noSuchOp", Value::Null, None).await.unwrap_err()),
        code::UNKNOWN_OP
    );
    assert_eq!(
        remote_code(
            c.request("searchMessages", json!({}), None)
                .await
                .unwrap_err()
        ),
        code::BAD_REQUEST
    );
    assert_eq!(
        remote_code(
            c.request("searchMessages", json!({"query": 3}), None)
                .await
                .unwrap_err()
        ),
        code::BAD_REQUEST
    );
    assert_eq!(
        remote_code(
            c.request("listAccounts", json!([1]), None)
                .await
                .unwrap_err()
        ),
        code::BAD_REQUEST
    );
    assert!(
        ext.calls().is_empty(),
        "invalid requests must not reach the extension"
    );
}

#[tokio::test]
async fn without_an_extension_requests_fail_clearly_but_status_works() {
    let daemon = TestDaemon::start().await.unwrap();
    let c = client(daemon.info()).await;
    assert!(!c.welcome().extension_connected);
    assert_eq!(
        remote_code(
            c.request("listAccounts", Value::Null, None)
                .await
                .unwrap_err()
        ),
        code::EXTENSION_UNAVAILABLE
    );
    let status = c.request("status", Value::Null, None).await.unwrap();
    assert_eq!(status["extensionConnected"], false);
    assert_eq!(status["protocol"], PROTOCOL_VERSION);
}

#[tokio::test]
async fn operations_need_the_module_the_extension_announced() {
    let (daemon, _ext) = TestDaemon::with_extension(FakeConfig::default())
        .await
        .unwrap();
    let c = client(daemon.info()).await;
    let err = c
        .request("searchAttachments", json!({"nameContains": "pdf"}), None)
        .await
        .unwrap_err();
    assert_eq!(remote_code(err), code::UNSUPPORTED_OP);

    let status = c.request("status", Value::Null, None).await.unwrap();
    assert_eq!(status["extension"]["modules"], json!(["core"]));
    assert_eq!(status["extension"]["thunderbird"], "154.0");
}

#[tokio::test]
async fn large_results_are_streamed_in_chunks_and_reassembled() {
    let (daemon, _ext) = TestDaemon::with_extension(FakeConfig::default())
        .await
        .unwrap();
    let c = client(daemon.info()).await;
    let msg = c
        .request(
            "getMessage",
            json!({"messageId": BIG_MESSAGE, "folderPath": INBOX}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(msg["body"].as_str().unwrap().len(), BIG_BODY_BYTES);

    // On the wire: chunk frames in order, then a response with streamed:true and no result.
    let mut request = daemon.info().ws_url().into_client_request().unwrap();
    request.headers_mut().insert(
        AUTHORIZATION,
        format!("Bearer {}", daemon.info().token).parse().unwrap(),
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let hello = Frame::Hello(Hello {
        protocol: PROTOCOL_VERSION,
        role: Role::Client,
        name: "raw".into(),
        capabilities: None,
    });
    ws.send(Message::text(hello.to_text())).await.unwrap();
    let _welcome = ws.next().await.unwrap().unwrap();
    let request = Frame::Request {
        id: "x".into(),
        op: "getMessage".into(),
        params: json!({"messageId": BIG_MESSAGE, "folderPath": INBOX}),
        timeout_ms: None,
    };
    ws.send(Message::text(request.to_text())).await.unwrap();
    let mut seqs = Vec::new();
    loop {
        let Message::Text(text) = ws.next().await.unwrap().unwrap() else {
            continue;
        };
        match Frame::parse(text.as_str()).unwrap() {
            Frame::Chunk { id, seq, .. } => {
                assert_eq!(id, "x", "the daemon must map ids back to the client's ids");
                seqs.push(seq);
            }
            Frame::Response {
                id,
                ok,
                result,
                streamed,
                ..
            } => {
                assert!(ok && streamed && result.is_none());
                assert_eq!(id, "x");
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(seqs.len() >= 6);
    assert!(seqs.iter().enumerate().all(|(i, s)| *s as usize == i));
}

#[tokio::test]
async fn concurrent_requests_are_matched_to_the_right_callers() {
    let (daemon, _ext) = TestDaemon::with_extension(FakeConfig::default())
        .await
        .unwrap();
    let c = std::sync::Arc::new(client(daemon.info()).await);
    let mut tasks = Vec::new();
    for query in ["report", "lunch", "invoice", "newsletter"] {
        let c = c.clone();
        tasks.push(tokio::spawn(async move {
            let r = c
                .request("searchMessages", json!({"query": query}), None)
                .await
                .unwrap();
            (query, r[0]["subject"].as_str().unwrap().to_lowercase())
        }));
    }
    for t in tasks {
        let (query, subject) = t.await.unwrap();
        assert!(subject.contains(query), "{query} got {subject}");
    }
}

#[tokio::test]
async fn unanswered_requests_time_out() {
    let (daemon, _ext) = TestDaemon::with_extension(FakeConfig::default())
        .await
        .unwrap();
    let c = client(daemon.info()).await;
    let err = c
        .request(
            "getMessage",
            json!({"messageId": HANG_MESSAGE, "folderPath": INBOX}),
            Some(Duration::from_millis(200)),
        )
        .await
        .unwrap_err();
    assert_eq!(remote_code(err), code::TIMEOUT);
}

#[tokio::test]
async fn pending_requests_fail_when_the_extension_disconnects() {
    let (daemon, ext) = TestDaemon::with_extension(FakeConfig::default())
        .await
        .unwrap();
    let c = std::sync::Arc::new(client(daemon.info()).await);
    let pending = {
        let c = c.clone();
        tokio::spawn(async move {
            c.request(
                "getMessage",
                json!({"messageId": HANG_MESSAGE, "folderPath": INBOX}),
                None,
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    ext.disconnect();
    let err = pending.await.unwrap().unwrap_err();
    assert_eq!(remote_code(err), code::EXTENSION_DISCONNECTED);

    tokio::time::sleep(Duration::from_millis(100)).await;
    let err = c
        .request("listAccounts", Value::Null, None)
        .await
        .unwrap_err();
    assert_eq!(remote_code(err), code::EXTENSION_UNAVAILABLE);
}

#[tokio::test]
async fn a_new_extension_connection_replaces_the_old_one() {
    let (daemon, first) = TestDaemon::with_extension(FakeConfig::default())
        .await
        .unwrap();
    let second = FakeExtension::connect(daemon.extension_info(), FakeConfig::default())
        .await
        .unwrap();
    let c = client(daemon.info()).await;
    c.request("listAccounts", Value::Null, None).await.unwrap();
    assert!(first.calls().is_empty());
    assert_eq!(second.calls().len(), 1);
}

#[tokio::test]
async fn a_client_can_cancel_a_request() {
    let (daemon, _ext) = TestDaemon::with_extension(FakeConfig::default())
        .await
        .unwrap();
    let mut request = daemon.info().ws_url().into_client_request().unwrap();
    request.headers_mut().insert(
        AUTHORIZATION,
        format!("Bearer {}", daemon.info().token).parse().unwrap(),
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let hello = Frame::Hello(Hello {
        protocol: PROTOCOL_VERSION,
        role: Role::Client,
        name: "raw".into(),
        capabilities: None,
    });
    ws.send(Message::text(hello.to_text())).await.unwrap();
    let _welcome = ws.next().await.unwrap().unwrap();
    let request = Frame::Request {
        id: "7".into(),
        op: "getMessage".into(),
        params: json!({"messageId": HANG_MESSAGE, "folderPath": INBOX}),
        timeout_ms: None,
    };
    ws.send(Message::text(request.to_text())).await.unwrap();
    ws.send(Message::text(Frame::Cancel { id: "7".into() }.to_text()))
        .await
        .unwrap();
    let Message::Text(text) = ws.next().await.unwrap().unwrap() else {
        panic!("not text")
    };
    let Frame::Response { id, ok, error, .. } = Frame::parse(text.as_str()).unwrap() else {
        panic!("not a response")
    };
    assert_eq!(id, "7");
    assert!(!ok);
    assert_eq!(error.unwrap().code, code::CANCELLED);
}

#[tokio::test]
async fn version_one_hello_is_closed() {
    let daemon = TestDaemon::start().await.unwrap();
    let mut request = daemon.info().ws_url().into_client_request().unwrap();
    request.headers_mut().insert(
        AUTHORIZATION,
        format!("Bearer {}", daemon.info().token).parse().unwrap(),
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let hello = Frame::Hello(Hello {
        protocol: 1,
        role: Role::Client,
        name: "old".into(),
        capabilities: None,
    });
    ws.send(Message::text(hello.to_text())).await.unwrap();
    match ws.next().await {
        Some(Ok(Message::Close(Some(frame)))) => assert_eq!(u16::from(frame.code), 1008),
        other => panic!("expected close, got {other:?}"),
    }
}

#[tokio::test]
async fn connection_file_is_removed_on_shutdown() {
    let mut daemon = TestDaemon::start().await.unwrap();
    let path = daemon.connection_file();
    assert!(path.exists());
    daemon.server.take().unwrap().shutdown().await;
    assert!(!path.exists());
}
