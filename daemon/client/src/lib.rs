//! Client for the tbd WebSocket protocol. Used by the `tb` CLI and the MCP server.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tb_protocol::connection::{ConnectionError, default_path};
use tb_protocol::{
    ConnectionInfo, ErrorBody, Frame, Hello, MAX_STREAM_BYTES, PROTOCOL_VERSION, Role, Welcome,
};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// No connection file, or the daemon is not listening.
    #[error("cannot reach the tb daemon: {0}")]
    Connect(String),
    /// The daemon or the extension answered with an error.
    #[error("{}: {}", .0.code, .0.message)]
    Remote(ErrorBody),
    #[error("the connection to the daemon was closed")]
    Closed,
    #[error("protocol error: {0}")]
    Protocol(String),
}

impl ClientError {
    /// Structured form for JSON error output.
    pub fn to_body(&self) -> ErrorBody {
        match self {
            ClientError::Remote(body) => body.clone(),
            ClientError::Connect(m) => ErrorBody::new("daemon_unavailable", m.clone()),
            ClientError::Closed => ErrorBody::new("connection_closed", self.to_string()),
            ClientError::Protocol(m) => ErrorBody::new("protocol_error", m.clone()),
        }
    }
}

impl From<ConnectionError> for ClientError {
    fn from(err: ConnectionError) -> Self {
        ClientError::Connect(format!("{err}; is tbd running?"))
    }
}

struct Slot {
    tx: oneshot::Sender<Result<Value, ClientError>>,
    buf: String,
    next_seq: u64,
}

type Slots = Arc<Mutex<HashMap<String, Slot>>>;

pub struct Client {
    out: mpsc::UnboundedSender<Frame>,
    slots: Slots,
    next_id: AtomicU64,
    welcome: Welcome,
    closed: Arc<std::sync::atomic::AtomicBool>,
}

impl Client {
    /// Connect using the connection file at `path`.
    pub async fn connect_file(path: &std::path::Path, name: &str) -> Result<Self, ClientError> {
        let info = ConnectionInfo::read(path)?;
        Self::connect(&info, name).await
    }

    /// Connect and complete the handshake, giving up after `CONNECT_TIMEOUT` so a stale
    /// connection file cannot hang the caller.
    pub async fn connect(info: &ConnectionInfo, name: &str) -> Result<Self, ClientError> {
        tokio::time::timeout(CONNECT_TIMEOUT, Self::connect_inner(info, name))
            .await
            .map_err(|_| ClientError::Connect("timed out waiting for the daemon".into()))?
    }

    async fn connect_inner(info: &ConnectionInfo, name: &str) -> Result<Self, ClientError> {
        let mut request = info
            .ws_url()
            .into_client_request()
            .map_err(|e| ClientError::Connect(e.to_string()))?;
        request.headers_mut().insert(
            AUTHORIZATION,
            format!("Bearer {}", info.token)
                .parse()
                .map_err(|_| ClientError::Connect("token is not a valid header value".into()))?,
        );
        let (ws, _) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| ClientError::Connect(e.to_string()))?;
        let (mut sink, mut stream) = ws.split();

        let hello = Frame::Hello(Hello {
            protocol: PROTOCOL_VERSION,
            role: Role::Client,
            name: name.to_owned(),
            capabilities: None,
        });
        sink.send(Message::text(hello.to_text()))
            .await
            .map_err(|e| ClientError::Connect(e.to_string()))?;
        let welcome = loop {
            match stream.next().await {
                Some(Ok(Message::Text(text))) => match Frame::parse(text.as_str()) {
                    Ok(Frame::Welcome(w)) => break w,
                    Ok(_) | Err(_) => return Err(ClientError::Protocol("expected welcome".into())),
                },
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                _ => return Err(ClientError::Closed),
            }
        };

        let slots: Slots = Arc::default();
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (out, mut out_rx) = mpsc::unbounded_channel::<Frame>();

        tokio::spawn(async move {
            while let Some(frame) = out_rx.recv().await {
                if sink.send(Message::text(frame.to_text())).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        let reader_slots = Arc::clone(&slots);
        let reader_closed = Arc::clone(&closed);
        tokio::spawn(async move {
            while let Some(Ok(message)) = stream.next().await {
                match message {
                    Message::Text(text) => {
                        if let Ok(frame) = Frame::parse(text.as_str()) {
                            dispatch(&reader_slots, frame);
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            reader_closed.store(true, Ordering::SeqCst);
            let drained: Vec<Slot> = lock(&reader_slots).drain().map(|(_, s)| s).collect();
            for slot in drained {
                let _ = slot.tx.send(Err(ClientError::Closed));
            }
        });

        Ok(Self {
            out,
            slots,
            next_id: AtomicU64::new(1),
            welcome,
            closed,
        })
    }

    pub fn welcome(&self) -> &Welcome {
        &self.welcome
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Run an operation and wait for its (possibly streamed) result.
    pub async fn request(
        &self,
        op: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let (tx, rx) = oneshot::channel();
        lock(&self.slots).insert(
            id.clone(),
            Slot {
                tx,
                buf: String::new(),
                next_seq: 0,
            },
        );
        let frame = Frame::Request {
            id: id.clone(),
            op: op.to_owned(),
            params,
            timeout_ms: timeout.map(|t| u64::try_from(t.as_millis()).unwrap_or(u64::MAX)),
        };
        if self.out.send(frame).is_err() {
            lock(&self.slots).remove(&id);
            return Err(ClientError::Closed);
        }
        rx.await.unwrap_or(Err(ClientError::Closed))
    }
}

fn lock(slots: &Slots) -> std::sync::MutexGuard<'_, HashMap<String, Slot>> {
    slots.lock().unwrap_or_else(|e| e.into_inner())
}

fn dispatch(slots: &Slots, frame: Frame) {
    let mut map = lock(slots);
    match frame {
        Frame::Chunk { id, seq, data } => {
            let Some(slot) = map.get_mut(&id) else { return };
            if seq != slot.next_seq || slot.buf.len() + data.len() > MAX_STREAM_BYTES {
                if let Some(slot) = map.remove(&id) {
                    let _ = slot.tx.send(Err(ClientError::Protocol(
                        "out-of-order or oversized chunk".into(),
                    )));
                }
                return;
            }
            slot.next_seq += 1;
            slot.buf.push_str(&data);
        }
        Frame::Response {
            id,
            ok,
            result,
            error,
            streamed,
        } => {
            let Some(slot) = map.remove(&id) else { return };
            let outcome = if !ok {
                Err(ClientError::Remote(error.unwrap_or_else(|| {
                    ErrorBody::new("unknown", "request failed")
                })))
            } else if streamed {
                serde_json::from_str(&slot.buf)
                    .map_err(|e| ClientError::Protocol(format!("streamed result is not JSON: {e}")))
            } else {
                Ok(result.unwrap_or(Value::Null))
            };
            let _ = slot.tx.send(outcome);
        }
        _ => {}
    }
}

/// A lazily connected, self-healing handle, for long-running servers such as the MCP server:
/// the daemon or Thunderbird may restart between calls.
pub struct Daemon {
    path: PathBuf,
    name: String,
    current: tokio::sync::Mutex<Option<Arc<Client>>>,
}

impl Daemon {
    pub fn new(connection_file: Option<PathBuf>, name: &str) -> Self {
        Self {
            path: connection_file.unwrap_or_else(default_path),
            name: name.to_owned(),
            current: tokio::sync::Mutex::new(None),
        }
    }

    async fn client(&self) -> Result<Arc<Client>, ClientError> {
        let mut current = self.current.lock().await;
        if let Some(client) = current.as_ref().filter(|c| !c.is_closed()) {
            return Ok(Arc::clone(client));
        }
        let client = Arc::new(Client::connect_file(&self.path, &self.name).await?);
        *current = Some(Arc::clone(&client));
        Ok(client)
    }

    pub async fn request(
        &self,
        op: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, ClientError> {
        self.client().await?.request(op, params, timeout).await
    }
}
