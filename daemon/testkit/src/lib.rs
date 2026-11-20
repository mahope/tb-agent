//! A fake Thunderbird extension. It speaks the extension side of the protocol and answers the
//! core operations from a small fixed mailbox, so contract tests need no Thunderbird.

mod daemon;
#[cfg(test)]
mod streaming_tests;

pub use daemon::TestDaemon;

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tb_protocol::{Capabilities, ConnectionInfo, Frame, Hello, PROTOCOL_VERSION, Role};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL;

pub const INBOX: &str = "imap://work@mail.example.com/INBOX";
pub const DRAFTS: &str = "imap://work@mail.example.com/Drafts";
pub const TRASH: &str = "imap://work@mail.example.com/Trash";
pub const PERSONAL_INBOX: &str = "imap://me@home.example.net/INBOX";

/// Id of the message whose body is large enough to be streamed in chunks.
pub const BIG_MESSAGE: &str = "big@example.com";
/// Requests naming this message ID are never answered.
pub const HANG_MESSAGE: &str = "hang@example.com";
/// Body size of [`BIG_MESSAGE`] in bytes.
pub const BIG_BODY_BYTES: usize = 200_000;

#[derive(Debug, Clone)]
pub struct FakeConfig {
    /// Modules announced in the hello. Defaults to `core` only.
    pub modules: Vec<String>,
    /// Results with a larger JSON text are sent as chunks.
    pub stream_threshold: usize,
    /// Size of each chunk.
    pub chunk_size: usize,
}

impl Default for FakeConfig {
    fn default() -> Self {
        Self {
            modules: vec!["core".into()],
            stream_threshold: 64 * 1024,
            chunk_size: 32 * 1024,
        }
    }
}

/// Handle to a running fake extension.
pub struct FakeExtension {
    task: JoinHandle<()>,
    calls: Arc<Mutex<Vec<(String, Value)>>>,
}

impl FakeExtension {
    /// Connect to the daemon and start answering requests. Returns once the daemon has
    /// accepted the hello, so requests sent afterwards reach this extension.
    pub async fn connect(info: &ConnectionInfo, config: FakeConfig) -> Result<Self> {
        let mut request = info.ws_url().into_client_request()?;
        request.headers_mut().insert(
            SEC_WEBSOCKET_PROTOCOL,
            format!("tb.token.{}", info.token).parse()?,
        );
        let (ws, _) = tokio_tungstenite::connect_async(request)
            .await
            .context("daemon refused the extension connection")?;
        let (mut sink, mut stream) = ws.split();
        let hello = Frame::Hello(Hello {
            protocol: PROTOCOL_VERSION,
            role: Role::Extension,
            name: "tb-fake-extension".into(),
            capabilities: Some(Capabilities {
                thunderbird: Some("154.0".into()),
                extension_version: Some("0.0.0-fake".into()),
                modules: config.modules.clone(),
            }),
        });
        sink.send(Message::text(hello.to_text())).await?;
        loop {
            match stream.next().await {
                Some(Ok(Message::Text(text))) => match Frame::parse(text.as_str())? {
                    Frame::Welcome(_) => break,
                    other => return Err(anyhow!("expected welcome, got {other:?}")),
                },
                Some(Ok(_)) => {}
                _ => return Err(anyhow!("daemon closed during handshake")),
            }
        }

        let calls: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
        let seen = Arc::clone(&calls);
        let task = tokio::spawn(async move {
            let mailbox = Mailbox::new();
            while let Some(Ok(message)) = stream.next().await {
                let Message::Text(text) = message else {
                    continue;
                };
                let Ok(Frame::Request { id, op, params, .. }) = Frame::parse(text.as_str()) else {
                    continue;
                };
                seen.lock().unwrap().push((op.clone(), params.clone()));
                if params.get("messageId").and_then(Value::as_str) == Some(HANG_MESSAGE) {
                    continue; // never answers, to test timeouts, cancels and disconnects
                }
                let frames = match mailbox.handle(&op, &params) {
                    Ok(result) => result_frames(&id, &result, &config),
                    Err((code, message)) => vec![Frame::err(&id, &code, message)],
                };
                for frame in frames {
                    if sink.send(Message::text(frame.to_text())).await.is_err() {
                        return;
                    }
                }
            }
        });
        Ok(Self { task, calls })
    }

    /// Every (operation, params) the extension received, in order.
    pub fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().unwrap().clone()
    }

    pub fn disconnect(self) {
        self.task.abort();
    }
}

fn result_frames(id: &str, result: &Value, config: &FakeConfig) -> Vec<Frame> {
    let text = serde_json::to_string(result).expect("result serializes");
    if text.len() <= config.stream_threshold {
        return vec![Frame::ok(id, result.clone())];
    }
    let mut frames = Vec::new();
    let mut rest = text.as_str();
    let mut seq = 0;
    while !rest.is_empty() {
        let mut cut = config.chunk_size.min(rest.len());
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        let (head, tail) = rest.split_at(cut);
        frames.push(Frame::Chunk {
            id: id.to_owned(),
            seq,
            data: head.to_owned(),
        });
        rest = tail;
        seq += 1;
    }
    frames.push(Frame::Response {
        id: id.to_owned(),
        ok: true,
        result: None,
        error: None,
        streamed: true,
    });
    frames
}

struct Mailbox {
    messages: Vec<Value>,
}

type Failure = (String, String);

fn not_found(what: &str) -> Failure {
    ("not_found".into(), format!("{what} not found"))
}

impl Mailbox {
    fn new() -> Self {
        let header =
            |id: &str, folder: &str, subject: &str, author: &str, date: &str, read: bool| {
                json!({
                    "id": id,
                    "threadId": id,
                    "subject": subject,
                    "author": author,
                    "recipients": ["mads@example.com"],
                    "ccList": [],
                    "date": date,
                    "read": read,
                    "flagged": false,
                    "tags": [],
                    "folderPath": folder,
                })
            };
        Self {
            messages: vec![
                header(
                    "m1@example.com",
                    INBOX,
                    "Quarterly report",
                    "Alice <alice@example.com>",
                    "2026-11-18T09:00:00.000Z",
                    false,
                ),
                header(
                    "m2@example.com",
                    INBOX,
                    "Lunch on Friday?",
                    "Bob <bob@example.com>",
                    "2026-11-17T12:30:00.000Z",
                    true,
                ),
                header(
                    "m3@example.com",
                    PERSONAL_INBOX,
                    "Invoice 2026-11",
                    "Billing <billing@example.org>",
                    "2026-11-16T08:15:00.000Z",
                    false,
                ),
                header(
                    BIG_MESSAGE,
                    INBOX,
                    "Large newsletter",
                    "News <news@example.com>",
                    "2026-11-15T07:00:00.000Z",
                    true,
                ),
            ],
        }
    }

    fn find(&self, params: &Value) -> Option<&Value> {
        let id = params.get("messageId")?.as_str()?;
        let folder = params.get("folderPath")?.as_str()?;
        self.messages
            .iter()
            .find(|m| m["id"] == id && m["folderPath"] == folder)
    }

    fn handle(&self, op: &str, params: &Value) -> Result<Value, Failure> {
        match op {
            "listAccounts" => Ok(json!([
                {"id": "account1", "name": "Work", "type": "imap",
                 "identities": [{"id": "id1", "name": "Mads", "email": "mads@example.com"}]},
                {"id": "account2", "name": "Personal", "type": "imap",
                 "identities": [{"id": "id2", "name": "Mads", "email": "me@home.example.net"}]},
            ])),
            "listFolders" => {
                let all = [
                    ("account1", "Inbox", INBOX, 3, 1),
                    ("account1", "Drafts", DRAFTS, 0, 0),
                    ("account1", "Trash", TRASH, 0, 0),
                    ("account2", "Inbox", PERSONAL_INBOX, 1, 1),
                ];
                let wanted = params.get("accountId").and_then(Value::as_str);
                let folders: Vec<Value> = all
                    .iter()
                    .filter(|f| wanted.is_none_or(|w| w == f.0))
                    .map(|(account, name, path, total, unread)| {
                        json!({"accountId": account, "name": name, "path": path,
                               "totalMessages": total, "unreadMessages": unread})
                    })
                    .collect();
                Ok(Value::Array(folders))
            }
            "searchMessages" | "getRecentMessages" => {
                let query = params
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_lowercase();
                let folder = params.get("folderPath").and_then(Value::as_str);
                let unread_only = params
                    .get("unreadOnly")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let hits: Vec<Value> = self
                    .messages
                    .iter()
                    .filter(|m| folder.is_none_or(|f| m["folderPath"] == f))
                    .filter(|m| !unread_only || m["read"] == false)
                    .filter(|m| {
                        query.split_whitespace().all(|t| {
                            m["subject"]
                                .as_str()
                                .unwrap_or("")
                                .to_lowercase()
                                .contains(t)
                                || m["author"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_lowercase()
                                    .contains(t)
                        })
                    })
                    .cloned()
                    .collect();
                if let Some(offset) = params.get("offset").and_then(Value::as_u64) {
                    let limit = params
                        .get("maxResults")
                        .and_then(Value::as_u64)
                        .unwrap_or(50);
                    let total = hits.len();
                    let page: Vec<Value> = hits
                        .into_iter()
                        .skip(offset as usize)
                        .take(limit as usize)
                        .collect();
                    let has_more = (offset + limit) < total as u64;
                    return Ok(json!({"messages": page, "totalMatches": total,
                                     "offset": offset, "limit": limit, "hasMore": has_more}));
                }
                if params.get("countOnly").and_then(Value::as_bool) == Some(true) {
                    return Ok(json!({"count": hits.len()}));
                }
                Ok(Value::Array(hits))
            }
            "getMessage" => {
                let m = self.find(params).ok_or_else(|| not_found("message"))?;
                let mut out = m.clone();
                if params.get("rawSource").and_then(Value::as_bool) == Some(true) {
                    return Ok(
                        json!({"id": m["id"], "rawSource": format!("Subject: {}\r\n\r\nbody\r\n", m["subject"].as_str().unwrap_or(""))}),
                    );
                }
                let body = if m["id"] == BIG_MESSAGE {
                    "x".repeat(BIG_BODY_BYTES)
                } else {
                    format!(
                        "Hello, this is the body of \"{}\".",
                        m["subject"].as_str().unwrap_or("")
                    )
                };
                out["body"] = Value::String(body);
                out["bodyFormat"] = params
                    .get("bodyFormat")
                    .cloned()
                    .unwrap_or_else(|| json!("markdown"));
                out["attachments"] = json!([]);
                Ok(out)
            }
            "getMessages" => {
                let items = params["messages"].as_array().cloned().unwrap_or_default();
                let mut out = Vec::new();
                for item in &items {
                    out.push(match self.handle("getMessage", item) {
                        Ok(v) => v,
                        Err((code, message)) => {
                            json!({"error": {"code": code, "message": message}})
                        }
                    });
                }
                Ok(Value::Array(out))
            }
            "getMessageHeaders" => self
                .find(params)
                .cloned()
                .ok_or_else(|| not_found("message")),
            "batchGetMessageHeaders" => {
                let folder = params["folderPath"].as_str().unwrap_or_default();
                let ids: Vec<&str> = params["messageIds"]
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                Ok(Value::Array(
                    self.messages
                        .iter()
                        .filter(|m| {
                            m["folderPath"] == folder
                                && ids.contains(&m["id"].as_str().unwrap_or(""))
                        })
                        .cloned()
                        .collect(),
                ))
            }
            "updateMessage" => {
                let count = params
                    .get("messageIds")
                    .and_then(Value::as_array)
                    .map_or(usize::from(params.get("messageId").is_some()), Vec::len);
                Ok(json!({"success": true, "updated": count}))
            }
            "deleteMessages" => {
                let count = params["messageIds"].as_array().map_or(0, Vec::len);
                Ok(json!({"success": true, "deleted": count}))
            }
            "saveDraft" => Ok(
                json!({"success": true, "folderPath": DRAFTS, "messageId": "draft-1@example.com"}),
            ),
            "sendMail" | "replyToMessage" | "forwardMessage" => {
                let direct = params.get("skipReview").and_then(Value::as_bool) == Some(true);
                Ok(json!({"success": true, "mode": if direct { "sent" } else { "review" }}))
            }
            other => Err((
                "unsupported_op".into(),
                format!("fake extension does not implement {other}"),
            )),
        }
    }
}
