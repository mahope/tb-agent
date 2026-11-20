use serde_json::{Value, json};
use tb_client::Client;
use tb_testkit::{FakeConfig, FakeExtension, INBOX, PERSONAL_INBOX, TestDaemon};

#[tokio::test]
async fn headers_default_hides_body_and_send_needs_permission() {
    let daemon = TestDaemon::with_policy(Some(
        r#"
[accounts.account1]
access = "full"
permissions = ["read"]
"#,
    ))
    .await
    .unwrap();
    let extension = FakeExtension::connect(daemon.extension_info(), FakeConfig::default())
        .await
        .unwrap();
    let client = Client::connect(daemon.info(), "policy test").await.unwrap();
    let header = client
        .request(
            "getMessage",
            json!({
                "messageId": "m3@example.com",
                "folderPath": PERSONAL_INBOX,
                "rawSource": true,
                "saveAttachments": true
            }),
            None,
        )
        .await
        .unwrap();
    assert_eq!(header["subject"], "Invoice 2026-11");
    for field in ["body", "attachments", "rawSource", "recipients"] {
        assert!(header.get(field).is_none());
    }
    let calls = extension.calls();
    assert_eq!(calls[0].0, "getMessageHeaders");
    assert!(calls[0].1.get("saveAttachments").is_none());

    let err = client
        .request(
            "sendMail",
            json!({
                "from": "id1", "to": "a@example.com", "subject": "test", "body": "test"
            }),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.to_body().code, "permission_denied");
    assert_eq!(extension.calls().len(), 1);

    let full = client
        .request(
            "getMessage",
            json!({
                "messageId": "m1@example.com", "folderPath": INBOX
            }),
            None,
        )
        .await
        .unwrap();
    assert!(full["body"].is_string());
    let _ = client.request("status", Value::Null, None).await.unwrap();
}

#[tokio::test]
async fn off_accounts_cannot_be_queried_or_used_as_destinations() {
    let daemon = TestDaemon::with_policy(Some(
        r#"
[accounts.account1]
access = "full"
permissions = ["read", "move", "send"]
[accounts.account2]
access = "off"
permissions = ["read", "move", "send"]
"#,
    ))
    .await
    .unwrap();
    let extension = FakeExtension::connect(daemon.extension_info(), FakeConfig::default())
        .await
        .unwrap();
    let client = Client::connect(daemon.info(), "policy test").await.unwrap();
    for (op, params) in [
        (
            "getMessage",
            json!({"messageId": "m3@example.com", "folderPath": PERSONAL_INBOX,
            "accountId": "account1", "_scope": {"accounts": ["account2"]}}),
        ),
        (
            "updateMessage",
            json!({"messageId": "m1@example.com", "folderPath": INBOX, "moveTo": PERSONAL_INBOX}),
        ),
        (
            "sendMail",
            json!({"from": "me@home.example.net", "to": "a@example.com", "subject": "test", "body": "test"}),
        ),
    ] {
        let err = client.request(op, params, None).await.unwrap_err();
        assert_eq!(err.to_body().code, "permission_denied");
    }
    assert!(extension.calls().is_empty());
    let result = client
        .request(
            "searchMessages",
            json!({
                "query": "", "countOnly": true, "_scope": {"accounts": ["account2"]}
            }),
            None,
        )
        .await
        .unwrap();
    assert_eq!(result["count"], 3);
    assert_eq!(
        extension.calls()[0].1["_scope"]["accounts"],
        json!(["account1"])
    );
}

#[tokio::test]
async fn streamed_mixed_reads_project_only_restricted_messages() {
    let daemon = TestDaemon::with_policy(Some(
        r#"
[accounts.account1]
access = "full"
permissions = ["read"]
"#,
    ))
    .await
    .unwrap();
    let extension = FakeExtension::connect(
        daemon.extension_info(),
        FakeConfig {
            stream_threshold: 25,
            chunk_size: 20,
            ..FakeConfig::default()
        },
    )
    .await
    .unwrap();
    let client = Client::connect(daemon.info(), "policy test").await.unwrap();
    let results = client.request("getMessages", json!({"messages": [
        {"messageId": "m1@example.com", "folderPath": INBOX},
        {"messageId": "m3@example.com", "folderPath": PERSONAL_INBOX, "saveAttachments": true}
    ]}), None).await.unwrap();
    assert!(results[0]["body"].is_string());
    assert_eq!(
        results[1],
        json!({
            "id": "m3@example.com", "author": "Billing <billing@example.org>",
            "subject": "Invoice 2026-11", "folderPath": PERSONAL_INBOX,
            "date": "2026-11-16T08:15:00.000Z", "read": false, "flagged": false, "tags": []
        })
    );
    assert!(
        extension.calls()[0].1["messages"][1]
            .get("saveAttachments")
            .is_none()
    );
}

#[tokio::test]
async fn client_token_cannot_register_an_extension() {
    let daemon = TestDaemon::start().await.unwrap();
    assert!(
        FakeExtension::connect(daemon.info(), FakeConfig::default())
            .await
            .is_err()
    );
}
