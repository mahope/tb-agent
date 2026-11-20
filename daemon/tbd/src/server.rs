//! HTTP/WebSocket front end of the daemon.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use tb_protocol::connection::default_path;
use tb_protocol::{
    ConnectionInfo, Frame, Hello, MAX_FRAME_BYTES, PROTOCOL_VERSION, Role, WS_PATH, Welcome, code,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::auth;
use crate::hub::{Hub, Out, Tx};

const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct Config {
    /// Address to listen on. Must be a loopback address; port 0 picks a free port.
    pub bind: SocketAddr,
    /// Where to write host, port and token. Defaults to the platform location.
    pub connection_file: Option<PathBuf>,
    /// Default time an operation may take before the client gets a `timeout` error.
    pub request_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            connection_file: None,
            request_timeout: Duration::from_secs(120),
        }
    }
}

pub struct RunningServer {
    pub info: ConnectionInfo,
    pub connection_file: PathBuf,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl RunningServer {
    /// Stop accepting connections, remove the connection file and wait for the server to end.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        self.wait().await;
    }

    /// Resolves when the server stops on its own.
    pub async fn wait(&mut self) {
        if let Some(task) = self.task.as_mut() {
            let _ = task.await;
            self.task = None;
        }
    }
}

pub async fn start(config: Config) -> Result<RunningServer> {
    if !config.bind.ip().is_loopback() {
        bail!("refusing to listen on {}: the daemon only accepts loopback addresses", config.bind);
    }
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("cannot bind {}", config.bind))?;
    let local = listener.local_addr()?;
    let token = auth::new_token();
    let info = ConnectionInfo {
        protocol: PROTOCOL_VERSION,
        host: local.ip().to_string(),
        port: local.port(),
        token: token.clone(),
        pid: std::process::id(),
    };
    let connection_file = config.connection_file.unwrap_or_else(default_path);
    info.write(&connection_file)?;

    let hub = Arc::new(Hub::new(token, config.request_timeout));
    let app = Router::new()
        .route(WS_PATH, get(ws_handler))
        .with_state(hub);
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let file = connection_file.clone();
    let own_token = info.token.clone();
    let task = tokio::spawn(async move {
        let served = axum::serve(listener, app).with_graceful_shutdown(async {
            let _ = shutdown_rx.await;
        });
        if let Err(err) = served.await {
            tracing::error!("server stopped: {err}");
        }
        // Only remove the file if it is still ours.
        if ConnectionInfo::read(&file).is_ok_and(|c| c.token == own_token) {
            let _ = std::fs::remove_file(&file);
        }
    });
    Ok(RunningServer {
        info,
        connection_file,
        shutdown: Some(shutdown_tx),
        task: Some(task),
    })
}

async fn ws_handler(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !auth::origin_allowed(&headers) {
        return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }
    let presented = auth::check(&headers, &hub.token);
    if !presented.valid {
        return (StatusCode::UNAUTHORIZED, "missing or wrong token").into_response();
    }
    let upgrade = match presented.subprotocol {
        Some(sub) => upgrade.protocols([sub]),
        None => upgrade,
    };
    upgrade
        .max_message_size(MAX_FRAME_BYTES)
        .max_frame_size(MAX_FRAME_BYTES)
        .on_upgrade(move |socket| handle_socket(hub, socket))
}

/// Next protocol frame, or `None` when the peer closed. Errors for malformed input.
async fn read_frame(
    stream: &mut futures_util::stream::SplitStream<WebSocket>,
) -> Option<Result<Frame, String>> {
    loop {
        match stream.next().await? {
            Ok(Message::Text(text)) => {
                return Some(Frame::parse(text.as_str()).map_err(|e| format!("invalid frame: {e}")));
            }
            Ok(Message::Binary(_)) => return Some(Err("binary frames are not allowed".into())),
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(Message::Ping(_) | Message::Pong(_)) => {}
        }
    }
}

async fn handle_socket(hub: Arc<Hub>, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx): (Tx, _) = mpsc::unbounded_channel();

    let writer = tokio::spawn(async move {
        while let Some(out) = rx.recv().await {
            match out {
                Out::Frame(frame) => {
                    if sink.send(Message::Text(frame.to_text().into())).await.is_err() {
                        break;
                    }
                }
                Out::Close(code, reason) => {
                    let _ = sink
                        .send(Message::Close(Some(CloseFrame { code, reason: reason.into() })))
                        .await;
                    break;
                }
            }
        }
    });

    let hello = match tokio::time::timeout(HELLO_TIMEOUT, read_frame(&mut stream)).await {
        Ok(Some(Ok(Frame::Hello(hello)))) => hello,
        _ => {
            let _ = tx.send(Out::Close(1002, "expected hello"));
            let _ = writer.await;
            return;
        }
    };
    if let Err(reason) = accept_hello(&hub, &tx, &hello) {
        let _ = tx.send(Out::Close(1008, reason));
        let _ = writer.await;
        return;
    }

    let conn_id = hub.new_conn_id();
    match hello.role {
        Role::Extension => {
            let caps = hello.capabilities.clone().unwrap_or_default();
            hub.attach_extension(conn_id, tx.clone(), caps);
            let welcome = welcome(&hub);
            let _ = tx.send(Out::Frame(Frame::Welcome(welcome)));
            tracing::info!("extension connected");
            while let Some(frame) = read_frame(&mut stream).await {
                match frame {
                    Ok(Frame::Ping) => {
                        let _ = tx.send(Out::Frame(Frame::Pong));
                    }
                    Ok(frame) => hub.extension_frame(conn_id, frame),
                    Err(message) => {
                        tracing::warn!("extension sent a bad frame: {message}");
                    }
                }
            }
            hub.detach_extension(conn_id);
            tracing::info!("extension disconnected");
        }
        Role::Client => {
            let _ = tx.send(Out::Frame(Frame::Welcome(welcome(&hub))));
            while let Some(frame) = read_frame(&mut stream).await {
                match frame {
                    Ok(Frame::Request { id, op, params, timeout_ms }) => {
                        hub.client_request(conn_id, &tx, id, &op, params, timeout_ms);
                    }
                    Ok(Frame::Cancel { id }) => hub.client_cancel(conn_id, &id),
                    Ok(Frame::Ping) => {
                        let _ = tx.send(Out::Frame(Frame::Pong));
                    }
                    Ok(_) => {}
                    Err(message) => {
                        let _ = tx.send(Out::Frame(Frame::err("", code::PROTOCOL, message)));
                    }
                }
            }
            hub.detach_client(conn_id);
        }
    }
    drop(tx);
    let _ = writer.await;
}

fn welcome(hub: &Hub) -> Welcome {
    let extension = hub.extension_caps();
    Welcome {
        protocol: PROTOCOL_VERSION,
        daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
        extension_connected: extension.is_some(),
        extension,
    }
}

fn accept_hello(_hub: &Hub, _tx: &Tx, hello: &Hello) -> Result<(), &'static str> {
    if hello.protocol != PROTOCOL_VERSION {
        return Err("unsupported protocol version");
    }
    if hello.role == Role::Extension
        && !hello
            .capabilities
            .as_ref()
            .is_some_and(|c| c.has_module("core"))
    {
        return Err("extension must announce the core module");
    }
    Ok(())
}
