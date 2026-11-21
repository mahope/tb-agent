//! Routing state: the connected extension and the requests waiting for it.

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tb_protocol::{
    Capabilities, ErrorBody, Frame, MAX_FRAME_BYTES, MAX_STREAM_BYTES, PROTOCOL_VERSION, code,
    find_op, validate_params,
};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::AbortHandle;

use crate::audit::{Audit, MessageState, WriteIntent};
use crate::policy::{Permission, Policy, Scope};
use tb_protocol::frame::Inventory;

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
    inventory: Option<Inventory>,
}

const LATE_WRITE_WINDOW: Duration = Duration::from_secs(120);

struct Pending {
    owner: u64,
    client_id: String,
    client_tx: Option<Tx>,
    ext_conn: u64,
    streamed_bytes: usize,
    timer: AbortHandle,
    projection: Option<(Scope, String, String, u64)>,
    write: Option<Write>,
}

enum Write {
    Snapshot {
        forward: Box<Frame>,
        intent: WriteIntent,
        folder: String,
        after_folder: Option<String>,
    },
    Dispatched {
        audit_id: String,
        expected_after: Option<(Vec<String>, String)>,
        deletion: bool,
    },
}

struct State {
    ext: Option<ExtConn>,
    pending: HashMap<String, Pending>,
    audit: Audit,
}

pub struct Hub {
    pub token: String,
    pub extension_token: String,
    policy: Policy,
    pub request_timeout: Duration,
    next_conn: AtomicU64,
    next_req: AtomicU64,
    state: Mutex<State>,
}

impl Hub {
    pub fn new(
        token: String,
        extension_token: String,
        request_timeout: Duration,
        policy: Policy,
        audit: Audit,
    ) -> Self {
        Self {
            token,
            extension_token,
            policy,
            request_timeout,
            next_conn: AtomicU64::new(1),
            next_req: AtomicU64::new(1),
            state: Mutex::new(State {
                ext: None,
                pending: HashMap::new(),
                audit,
            }),
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
    pub fn attach_extension(&self, conn_id: u64, tx: Tx, mut caps: Capabilities) {
        let mut state = self.lock();
        let inventory = caps.inventory.take().map(|inventory| *inventory);
        let old = state.ext.replace(ExtConn {
            conn_id,
            tx,
            caps,
            inventory,
        });
        if let Some(old) = old {
            let _ = old
                .tx
                .send(Out::Close(1012, "replaced by a newer extension connection"));
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

    /// A client went away: stop replying and cancel its undispatched writes and reads.
    pub fn detach_client(self: &std::sync::Arc<Self>, conn_id: u64) {
        let mut state = self.lock();
        let ids: Vec<String> = state
            .pending
            .iter()
            .filter(|(_, p)| p.owner == conn_id && p.client_tx.is_some())
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            self.abandon(&mut state, &id);
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
            return reply(Frame::err(
                id,
                code::UNKNOWN_OP,
                format!("unknown operation `{op}`"),
            ));
        };
        if let Err(message) = validate_params(spec, &params) {
            return reply(Frame::err(id, code::BAD_REQUEST, message));
        }
        if op == "status" {
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
        if !spec.is_local() && !ext.caps.has_module(&spec.module) {
            let message = format!(
                "the connected extension does not provide the `{}` module needed by `{op}`",
                spec.module
            );
            drop(state);
            return reply(Frame::err(id, code::UNSUPPORTED_OP, message));
        }

        let Some(inventory) = ext.inventory.as_ref() else {
            return reply(Frame::err(
                id,
                code::UNSUPPORTED_OP,
                "extension has not provided an account policy inventory",
            ));
        };
        if state
            .pending
            .values()
            .any(|p| p.owner == owner && p.client_id == id && p.client_tx.is_some())
        {
            drop(state);
            return reply(Frame::err(
                id,
                code::BAD_REQUEST,
                "request id already in use",
            ));
        }
        let ext_conn = ext.conn_id;
        let ext_tx = ext.tx.clone();
        let daemon_id = format!("d{}", self.next_req.fetch_add(1, Ordering::Relaxed));
        let request = |op: String, params: Value| Frame::Request {
            id: daemon_id.clone(),
            op,
            params,
            timeout_ms: Some(u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX)),
        };
        let forward_result = if op == "undo" {
            self.prepare_undo(&mut state, &params, request)
        } else {
            let properties = &spec.input_schema["properties"];
            let params = Value::Object(
                params
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter(|(key, _)| properties.get(*key).is_some())
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
            );
            let independent = matches!(
                spec.module.as_str(),
                "experiment.calendar" | "experiment.contacts"
            );
            let route = match self.policy.route(inventory, op, params, independent) {
                Ok(route) => route,
                Err(message) => return reply(Frame::err(id, code::PERMISSION_DENIED, message)),
            };
            if route.scope.accounts.is_empty() && !independent {
                return reply(Frame::ok(id, empty_result(op, &route.params)));
            }
            let mut params = route.params;
            params["_scope"] = route.scope.wire();
            if spec.access.is_mutating() {
                let after_folder = route.reversible.then(|| {
                    if params["trash"] == true || route.op == "deleteMessages" {
                        let account = &inventory.folders
                            [params["folderPath"].as_str().expect("checked folder")];
                        inventory.trash_folders[account].clone()
                    } else {
                        params["moveTo"]
                            .as_str()
                            .or(params["folderPath"].as_str())
                            .expect("checked folder")
                            .to_owned()
                    }
                });
                let intent = WriteIntent {
                    accounts: route.scope.accounts,
                    operation: route.op.clone(),
                    permissions: route.permissions,
                    message_ids: route.message_ids,
                    previous: Vec::new(),
                    reversible: route.reversible,
                    undo_of: None,
                };
                if let Some(folder) = route.snapshot_folder {
                    let snapshot = request(
                        "snapshotMessages".into(),
                        json!({
                            "folderPath": folder, "messageIds": intent.message_ids, "_scope": params["_scope"]
                        }),
                    );
                    Ok((
                        snapshot,
                        None,
                        Some(Write::Snapshot {
                            forward: Box::new(request(route.op, params)),
                            intent,
                            folder,
                            after_folder,
                        }),
                    ))
                } else {
                    begin(&mut state.audit, intent)
                        .map(|write| (request(route.op, params), None, Some(write)))
                }
            } else {
                let projection = route
                    .scope
                    .restricts_results()
                    .then(|| (route.scope, route.op.clone(), String::new(), 0));
                Ok((request(route.op, params), projection, None))
            }
        };
        let (forward, projection, write) = match forward_result {
            Ok(forward) => forward,
            Err((error, message)) => return reply(Frame::err(id, error, message)),
        };
        if ext_tx.send(Out::Frame(forward)).is_err() {
            return reply(Frame::err(
                id,
                code::EXTENSION_DISCONNECTED,
                "extension connection is closing",
            ));
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
                client_tx: Some(client_tx.clone()),
                ext_conn,
                streamed_bytes: 0,
                projection,
                write,
                timer,
            },
        );
    }

    fn prepare_undo(
        &self,
        state: &mut State,
        params: &Value,
        request: impl Fn(String, Value) -> Frame,
    ) -> Result<Dispatch, Failure> {
        let audit_id = params["auditId"].as_str().expect("validated audit id");
        let undo = state
            .audit
            .undo(audit_id)
            .ok_or((code::UNDO_UNAVAILABLE, "audit entry has no available undo"))?;
        let inventory = state
            .ext
            .as_ref()
            .and_then(|ext| ext.inventory.as_ref())
            .expect("inventory checked");
        let scope = self
            .policy
            .undo_scope(
                inventory,
                &undo.write.accounts,
                &undo.write.permissions,
                undo.write
                    .previous
                    .iter()
                    .chain(&undo.after)
                    .map(|m| m.folder_path.clone()),
            )
            .map_err(|message| (code::PERMISSION_DENIED, message))?;
        let forward = request(
            "restoreMessages".into(),
            json!({
                "_scope": scope.wire(),
                "_allowTrashMove": self.policy.allows_on_all(&scope.accounts, Permission::Trash),
                "_allowTagRestore": self.policy.allows_on_all(&scope.accounts, Permission::Tag),
                "expected": undo.after, "restore": undo.write.previous,
            }),
        );
        let intent = WriteIntent {
            operation: "undo".into(),
            previous: undo.after,
            reversible: false,
            undo_of: Some(audit_id.to_owned()),
            ..undo.write
        };
        let write = begin(&mut state.audit, intent)?;
        Ok((forward, None, Some(write)))
    }

    /// A client asked to cancel one of its requests.
    pub fn client_cancel(self: &std::sync::Arc<Self>, owner: u64, client_id: &str) {
        let mut state = self.lock();
        let found = state
            .pending
            .iter()
            .find(|(_, p)| p.owner == owner && p.client_id == client_id && p.client_tx.is_some())
            .map(|(id, _)| id.clone());
        if let Some(id) = found {
            let p = &state.pending[&id];
            let _ = p
                .client_tx
                .as_ref()
                .expect("active client")
                .send(Out::Frame(Frame::err(
                    p.client_id.clone(),
                    code::CANCELLED,
                    "request cancelled",
                )));
            self.abandon(&mut state, &id);
        }
    }

    fn expire(self: &std::sync::Arc<Self>, daemon_id: &str) {
        let mut state = self.lock();
        if let Some(p) = state.pending.get(daemon_id)
            && let Some(tx) = &p.client_tx
        {
            let _ = tx.send(Out::Frame(Frame::err(
                p.client_id.clone(),
                code::TIMEOUT,
                "the extension did not answer in time",
            )));
            self.abandon(&mut state, daemon_id);
        }
    }

    fn abandon(self: &std::sync::Arc<Self>, state: &mut State, id: &str) {
        if let Some(p) = state.pending.get_mut(id)
            && matches!(p.write, Some(Write::Dispatched { .. }))
            && p.client_tx.take().is_some()
        {
            // Once sent, the write can take effect. Keep its reply path for the journal.
            p.timer.abort();
            let hub = std::sync::Arc::downgrade(self);
            let id = id.to_owned();
            p.timer = tokio::spawn(async move {
                tokio::time::sleep(LATE_WRITE_WINDOW).await;
                if let Some(hub) = hub.upgrade() {
                    drop_pending(&mut hub.lock(), &id, false);
                }
            })
            .abort_handle();
        } else {
            drop_pending(state, id, true);
        }
    }

    /// Handle a frame from the extension, including late replies for dispatched writes.
    pub fn extension_frame(&self, conn_id: u64, frame: Frame) {
        let mut state = self.lock();
        match frame {
            Frame::Response {
                id,
                ok,
                result,
                error,
                streamed,
            } => {
                let Some(p) = state.pending.get_mut(&id) else {
                    return;
                };
                if p.ext_conn != conn_id {
                    return;
                }
                let client_id = p.client_id.clone();
                let client_tx = p.client_tx.clone();
                if let Some(write) = p.write.take() {
                    match write {
                        Write::Snapshot {
                            mut forward,
                            mut intent,
                            folder,
                            after_folder,
                        } if ok => {
                            let client_tx = client_tx.expect("active snapshot");
                            let previous = checked_states(result, &intent.message_ids, &folder);
                            let outcome = previous
                                .filter(|_| !streamed)
                                .ok_or((code::PROTOCOL, "invalid message snapshot"))
                                .and_then(|previous| {
                                    if let Frame::Request { params, .. } = forward.as_mut() {
                                        params["_expected"] = json!(previous);
                                    }
                                    intent.previous = previous;
                                    let expected_after = after_folder
                                        .map(|folder| (intent.message_ids.clone(), folder));
                                    let deletion = intent.operation == "deleteMessages";
                                    persist_intent(&mut state.audit, intent).map(|audit_id| {
                                        Write::Dispatched {
                                            audit_id,
                                            expected_after,
                                            deletion,
                                        }
                                    })
                                });
                            match outcome {
                                Ok(write) => {
                                    let ext = state.ext.as_ref().expect("active extension");
                                    if ext.tx.send(Out::Frame(*forward)).is_ok() {
                                        state
                                            .pending
                                            .get_mut(&id)
                                            .expect("pending snapshot")
                                            .write = Some(write);
                                        return;
                                    }
                                    let _ = client_tx.send(Out::Frame(Frame::err(
                                        client_id,
                                        code::EXTENSION_DISCONNECTED,
                                        "extension connection is closing",
                                    )));
                                }
                                Err((code, message)) => {
                                    let _ = client_tx
                                        .send(Out::Frame(Frame::err(client_id, code, message)));
                                }
                            }
                            drop_pending(&mut state, &id, false);
                            return;
                        }
                        Write::Dispatched {
                            audit_id,
                            expected_after,
                            deletion,
                        } => {
                            let mut result = result.unwrap_or(Value::Null);
                            let after_value = result
                                .as_object_mut()
                                .and_then(|result| result.remove("_after"));
                            let moved_to_trash = result["movedToTrash"].as_bool();
                            let after_required = if deletion {
                                moved_to_trash == Some(true)
                            } else {
                                expected_after.is_some()
                            };
                            let after = expected_after.filter(|_| after_required).and_then(
                                |(ids, folder)| checked_states(after_value, &ids, &folder),
                            );
                            let confirmed = ok
                                && !streamed
                                && (!deletion || moved_to_trash.is_some())
                                && (!after_required || after.is_some());
                            let conflict = !ok
                                && !streamed
                                && error.as_ref().is_some_and(|error| error.code == "conflict");
                            let created = confirmed
                                .then(|| result["messageId"].as_str().map(str::to_owned))
                                .flatten();
                            let persisted = state.audit.finish(
                                audit_id.clone(),
                                confirmed,
                                conflict,
                                after.unwrap_or_default(),
                                created,
                            );
                            let frame = if persisted.is_err() {
                                audit_error(
                                    client_id,
                                    audit_id,
                                    code::AUDIT_FAILED,
                                    "could not persist write outcome; check the mailbox before retrying",
                                )
                            } else if confirmed {
                                if !result.is_object() {
                                    result = json!({"result": result});
                                }
                                result["auditId"] = json!(audit_id);
                                Frame::ok(client_id, result)
                            } else if let Some(error) = error.filter(|_| !ok && !streamed) {
                                Frame::Response {
                                    id: client_id,
                                    ok: false,
                                    result: None,
                                    error: Some(error),
                                    streamed: false,
                                }
                            } else {
                                audit_error(
                                    client_id,
                                    audit_id,
                                    code::AUDIT_FAILED,
                                    "write outcome was not confirmed; check the mailbox before retrying",
                                )
                            };
                            if let Some(tx) = client_tx {
                                let _ = tx.send(Out::Frame(frame));
                            }
                            drop_pending(&mut state, &id, false);
                            return;
                        }
                        Write::Snapshot { .. } => {}
                    }
                }
                let client_tx = client_tx.expect("active read or snapshot");
                if let Some((scope, op, buffered, _)) = p.projection.take() {
                    let inventory = state
                        .ext
                        .as_ref()
                        .and_then(|ext| ext.inventory.as_ref())
                        .expect("inventory checked");
                    let value = if streamed {
                        serde_json::from_str(&buffered).ok()
                    } else {
                        result
                    };
                    if let Some(value) = value.filter(|_| ok) {
                        send_projected(&client_tx, client_id, scope.project(inventory, &op, value));
                    } else {
                        let error_code = match error.as_ref().map(|error| error.code.as_str()) {
                            Some("not_found") => "not_found",
                            _ => code::PROTOCOL,
                        };
                        let _ = client_tx.send(Out::Frame(Frame::err(
                            client_id,
                            error_code,
                            "restricted read could not be completed",
                        )));
                    }
                } else {
                    let _ = client_tx.send(Out::Frame(Frame::Response {
                        id: client_id,
                        ok,
                        result,
                        error,
                        streamed,
                    }));
                }
                drop_pending(&mut state, &id, false);
            }
            Frame::Chunk { id, seq, data } => {
                let Some(p) = state.pending.get_mut(&id) else {
                    return;
                };
                if p.ext_conn != conn_id {
                    return;
                }
                let Some(tx) = &p.client_tx else {
                    return;
                };
                p.streamed_bytes = p.streamed_bytes.saturating_add(data.len());
                if p.streamed_bytes > MAX_STREAM_BYTES {
                    let _ = tx.send(Out::Frame(Frame::err(
                        p.client_id.clone(),
                        code::PAYLOAD_TOO_LARGE,
                        format!("streamed result exceeds {MAX_STREAM_BYTES} bytes"),
                    )));
                    drop_pending(&mut state, &id, true);
                    return;
                }
                if p.write.is_some()
                    || p.projection
                        .as_ref()
                        .is_some_and(|(_, _, _, next)| *next != seq)
                {
                    let _ = tx.send(Out::Frame(Frame::err(
                        p.client_id.clone(),
                        code::PROTOCOL,
                        "unexpected result chunk",
                    )));
                    drop_pending(&mut state, &id, true);
                    return;
                }
                if let Some((_, _, buffered, next)) = &mut p.projection {
                    buffered.push_str(&data);
                    *next += 1;
                    return;
                }
                let _ = tx.send(Out::Frame(Frame::Chunk {
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

type Failure = (&'static str, &'static str);
type Projection = (Scope, String, String, u64);
type Dispatch = (Frame, Option<Projection>, Option<Write>);

fn persist_intent(audit: &mut Audit, write: WriteIntent) -> Result<String, Failure> {
    audit
        .begin(write)
        .map_err(|_| (code::AUDIT_FAILED, "could not persist write intent"))
}

fn begin(audit: &mut Audit, write: WriteIntent) -> Result<Write, Failure> {
    Ok(Write::Dispatched {
        audit_id: persist_intent(audit, write)?,
        expected_after: None,
        deletion: false,
    })
}

fn audit_error(id: String, audit_id: String, code: &str, message: &str) -> Frame {
    let mut error = ErrorBody::new(code, message);
    error.data = Some(json!({"auditId": audit_id}));
    Frame::Response {
        id,
        ok: false,
        result: None,
        error: Some(error),
        streamed: false,
    }
}

fn checked_states(value: Option<Value>, ids: &[String], folder: &str) -> Option<Vec<MessageState>> {
    let states: Vec<MessageState> = serde_json::from_value(value?).ok()?;
    let actual: BTreeSet<_> = states.iter().map(|m| &m.message_id).collect();
    let instances: BTreeSet<_> = states.iter().map(|m| &m.instance_id).collect();
    (states.len() == ids.len()
        && ids.iter().all(|id| actual.contains(id))
        && states
            .iter()
            .all(|m| m.folder_path == folder && !m.instance_id.is_empty())
        && instances.len() == states.len())
    .then_some(states)
}

fn empty_result(op: &str, params: &Value) -> Value {
    if op == "getAccountAccess" {
        json!({"mode": "restricted", "accounts": []})
    } else if matches!(op, "searchMessages" | "getRecentMessages") && params.get("offset").is_some()
    {
        json!({"messages": [], "totalMatches": 0, "offset": params["offset"],
            "limit": params.get("maxResults").cloned().unwrap_or(json!(50)), "hasMore": false})
    } else if op == "listFolders" && params["format"] == "table" {
        json!({"columns": ["accountId", "depth", "isFavorite", "name", "path", "totalMessages", "type", "unreadMessages"], "rows": []})
    } else if params["countOnly"] == true {
        json!({"count": 0})
    } else {
        json!([])
    }
}

fn send_projected(tx: &Tx, client_id: String, value: Value) {
    let frame = Frame::ok(client_id.clone(), value);
    if frame.to_text().len() <= MAX_FRAME_BYTES {
        let _ = tx.send(Out::Frame(frame));
        return;
    }
    let Frame::Response {
        result: Some(result),
        ..
    } = frame
    else {
        unreachable!()
    };
    let text = result.to_string();
    let mut rest = text.as_str();
    let mut seq = 0;
    while !rest.is_empty() {
        let mut end = rest.len().min(1024 * 1024);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let (data, remaining) = rest.split_at(end);
        let _ = tx.send(Out::Frame(Frame::Chunk {
            id: client_id.clone(),
            seq,
            data: data.into(),
        }));
        seq += 1;
        rest = remaining;
    }
    let _ = tx.send(Out::Frame(Frame::Response {
        id: client_id,
        ok: true,
        result: None,
        error: None,
        streamed: true,
    }));
}

fn drop_pending(state: &mut State, daemon_id: &str, tell_extension: bool) {
    if let Some(p) = state.pending.remove(daemon_id) {
        p.timer.abort();
        if tell_extension && let Some(ext) = state.ext.as_ref().filter(|e| e.conn_id == p.ext_conn)
        {
            let _ = ext.tx.send(Out::Frame(Frame::Cancel {
                id: daemon_id.to_owned(),
            }));
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
            if let Some(tx) = p.client_tx {
                let _ = tx.send(Out::Frame(Frame::err(
                    p.client_id,
                    error_code,
                    "the Thunderbird extension disconnected before answering",
                )));
            }
        }
    }
}
