//! Routing state: the connected extension and the requests waiting for it.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tb_protocol::{Capabilities, Frame, MAX_STREAM_BYTES, PROTOCOL_VERSION, code, find_op, validate_params};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::AbortHandle;

/// What the writer task of a connection sends.
#[derive(Debug)]
pub enum Out {
    Frame(Frame),
    Close(u16, &'static str),
}

pub type Tx = UnboundedSender<Out>;

struct ExtConn {
    conn_id: u64,
    tx: Tx,
    caps: Capabilities,
}

struct Pending {
    owner: u64,
    client_id: String,
    client_tx: Tx,
    ext_conn: u64,
    streamed_bytes: usize,
    timer: AbortHandle,
}

#[derive(Default)]
struct State {
    ext: Option<ExtConn>,
    pending: HashMap<String, Pending>,
}

pub struct Hub {
    pub token: String,
    pub request_timeout: Duration,
    next_conn: AtomicU64,
    next_req: AtomicU64,
    state: Mutex<State>,
}

impl Hub {
    pub fn new(token: String, request_timeout: Duration) -> Self {
        Self {
            token,
            request_timeout,
            next_conn: AtomicU64::new(1),
            next_req: AtomicU64::new(1),
            state: Mutex::new(State::default()),
        }
    }

    pub fn new_conn_id(&self) -> u64 {
        self.next_conn.fetch_add(1, Ordering::Relaxed)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn extension_caps(&self) -> Option<Capabilities> {
        self.lock().ext.as_ref().map(|e| e.caps.clone())
    }

    /// Register the extension. A previous extension connection is closed and its requests fail.
    pub fn attach_extension(&self, conn_id: u64, tx: Tx, caps: Capabilities) {
        let mut state = self.lock();
        let old = state.ext.replace(ExtConn { conn_id, tx, caps });
        if let Some(old) = old {
            let _ = old.tx.send(Out::Close(1012, "replaced by a newer extension connection"));
            fail_pending(&mut state, old.conn_id, code::EXTENSION_DISCONNECTED);
        }
    }

    pub fn detach_extension(&self, conn_id: u64) {
        let mut state = self.lock();
        if state.ext.as_ref().is_some_and(|e| e.conn_id == conn_id) {
            state.ext = None;
        }
        fail_pending(&mut state, conn_id, code::EXTENSION_DISCONNECTED);
    }

    /// A client went away: abandon its requests and tell the extension.
    pub fn detach_client(&self, conn_id: u64) {
        let mut state = self.lock();
        let ids: Vec<String> = state
            .pending
            .iter()
            .filter(|(_, p)| p.owner == conn_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            drop_pending(&mut state, &id, true);
        }
    }

    /// Handle a `request` frame from a client.
    pub fn client_request(
        self: &std::sync::Arc<Self>,
        owner: u64,
        client_tx: &Tx,
        id: String,
        op: &str,
        params: Value,
        timeout_ms: Option<u64>,
    ) {
        let reply = |frame: Frame| {
            let _ = client_tx.send(Out::Frame(frame));
        };
        let Some(spec) = find_op(op) else {
            return reply(Frame::err(id, code::UNKNOWN_OP, format!("unknown operation `{op}`")));
        };
        if let Err(message) = validate_params(spec, &params) {
            return reply(Frame::err(id, code::BAD_REQUEST, message));
        }
        if spec.is_local() {
            return reply(Frame::ok(id, self.status()));
        }

        let timeout = timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(self.request_timeout);
        let mut state = self.lock();
        let Some(ext) = state.ext.as_ref() else {
            drop(state);
            return reply(Frame::err(
                id,
                code::EXTENSION_UNAVAILABLE,
                "Thunderbird is not connected to the daemon; start Thunderbird and check that the tb-agent extension is enabled",
            ));
        };
        if !ext.caps.has_module(&spec.module) {
            let message = format!(
                "the connected extension does not provide the `{}` module needed by `{op}`",
                spec.module
            );
            drop(state);
            return reply(Frame::err(id, code::UNSUPPORTED_OP, message));
        }

        let daemon_id = format!("d{}", self.next_req.fetch_add(1, Ordering::Relaxed));
        let ext_conn = ext.conn_id;
        let forward = Frame::Request {
            id: daemon_id.clone(),
            op: op.to_owned(),
            params,
            timeout_ms: Some(u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX)),
        };
        if ext.tx.send(Out::Frame(forward)).is_err() {
            drop(state);
            return reply(Frame::err(id, code::EXTENSION_DISCONNECTED, "extension connection is closing"));
        }

        let hub = std::sync::Arc::clone(self);
        let timer_id = daemon_id.clone();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            hub.expire(&timer_id);
        })
        .abort_handle();
        state.pending.insert(
            daemon_id,
            Pending {
                owner,
                client_id: id,
                client_tx: client_tx.clone(),
                ext_conn,
                streamed_bytes: 0,
                timer,
            },
        );
    }

    /// A client asked to cancel one of its requests.
    pub fn client_cancel(&self, owner: u64, client_id: &str) {
        let mut state = self.lock();
        let found = state
            .pending
            .iter()
            .find(|(_, p)| p.owner == owner && p.client_id == client_id)
            .map(|(id, _)| id.clone());
        if let Some(id) = found {
            if let Some(p) = state.pending.get(&id) {
                let _ = p.client_tx.send(Out::Frame(Frame::err(
                    p.client_id.clone(),
                    code::CANCELLED,
                    "request cancelled",
                )));
            }
            drop_pending(&mut state, &id, true);
        }
    }

    fn expire(&self, daemon_id: &str) {
        let mut state = self.lock();
        if let Some(p) = state.pending.get(daemon_id) {
            let _ = p.client_tx.send(Out::Frame(Frame::err(
                p.client_id.clone(),
                code::TIMEOUT,
                "the extension did not answer in time",
            )));
            drop_pending(&mut state, daemon_id, true);
        }
    }

    /// Handle a frame from the extension. Unknown ids are ignored: the request may have been
    /// cancelled or timed out already.
    pub fn extension_frame(&self, conn_id: u64, frame: Frame) {
        let mut state = self.lock();
        match frame {
            Frame::Response { id, ok, result, error, streamed } => {
                let Some(p) = state.pending.get(&id) else { return };
                if p.ext_conn != conn_id {
                    return;
                }
                let _ = p.client_tx.send(Out::Frame(Frame::Response {
                    id: p.client_id.clone(),
                    ok,
                    result,
                    error,
                    streamed,
                }));
                drop_pending(&mut state, &id, false);
            }
            Frame::Chunk { id, seq, data } => {
                let Some(p) = state.pending.get_mut(&id) else { return };
                if p.ext_conn != conn_id {
                    return;
                }
                p.streamed_bytes = p.streamed_bytes.saturating_add(data.len());
                if p.streamed_bytes > MAX_STREAM_BYTES {
                    let _ = p.client_tx.send(Out::Frame(Frame::err(
                        p.client_id.clone(),
                        code::PAYLOAD_TOO_LARGE,
                        format!("streamed result exceeds {MAX_STREAM_BYTES} bytes"),
                    )));
                    drop_pending(&mut state, &id, true);
                    return;
                }
                let _ = p.client_tx.send(Out::Frame(Frame::Chunk {
                    id: p.client_id.clone(),
                    seq,
                    data,
                }));
            }
            _ => {}
        }
    }

    pub fn status(&self) -> Value {
        let state = self.lock();
        let caps = state.ext.as_ref().map(|e| &e.caps);
        json!({
            "daemonVersion": env!("CARGO_PKG_VERSION"),
            "protocol": PROTOCOL_VERSION,
            "extensionConnected": caps.is_some(),
            "extension": caps,
            "pendingRequests": state.pending.len(),
        })
    }
}

fn drop_pending(state: &mut State, daemon_id: &str, tell_extension: bool) {
    if let Some(p) = state.pending.remove(daemon_id) {
        p.timer.abort();
        if tell_extension
            && let Some(ext) = state.ext.as_ref().filter(|e| e.conn_id == p.ext_conn)
        {
            let _ = ext.tx.send(Out::Frame(Frame::Cancel { id: daemon_id.to_owned() }));
        }
    }
}

fn fail_pending(state: &mut State, ext_conn: u64, error_code: &str) {
    let ids: Vec<String> = state
        .pending
        .iter()
        .filter(|(_, p)| p.ext_conn == ext_conn)
        .map(|(id, _)| id.clone())
        .collect();
    for id in ids {
        if let Some(p) = state.pending.remove(&id) {
            p.timer.abort();
            let _ = p.client_tx.send(Out::Frame(Frame::err(
                p.client_id,
                error_code,
                "the Thunderbird extension disconnected before answering",
            )));
        }
    }
}
