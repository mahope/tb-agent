//! Frame types exchanged over the WebSocket.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Error codes the daemon itself produces. The extension may use any other string.
pub mod code {
    pub const BAD_REQUEST: &str = "bad_request";
    pub const UNKNOWN_OP: &str = "unknown_op";
    pub const UNSUPPORTED_OP: &str = "unsupported_op";
    pub const EXTENSION_UNAVAILABLE: &str = "extension_unavailable";
    pub const EXTENSION_DISCONNECTED: &str = "extension_disconnected";
    pub const TIMEOUT: &str = "timeout";
    pub const CANCELLED: &str = "cancelled";
    pub const PAYLOAD_TOO_LARGE: &str = "payload_too_large";
    pub const PROTOCOL: &str = "protocol_error";
    pub const PERMISSION_DENIED: &str = "permission_denied";
    pub const AUDIT_FAILED: &str = "audit_failed";
    pub const UNDO_UNAVAILABLE: &str = "undo_unavailable";
    pub const INTERNAL: &str = "internal";
}

/// Who is on the other end of the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// The Thunderbird extension. Executes operations.
    Extension,
    /// An agent-facing client such as `tb` or the MCP server. Sends operations.
    Client,
}

/// Optional functionality the extension reports in its `hello`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    /// Thunderbird version, for example `154.0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thunderbird: Option<String>,
    /// Extension version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension_version: Option<String>,
    /// Module names the extension implements. `core` needs only WebExtension APIs. Other
    /// modules, for example `experiment.attachments`, may need Experiment APIs.
    #[serde(default)]
    pub modules: Vec<String>,
    /// Private routing inventory, supplied only by the authenticated extension.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inventory: Option<Inventory>,
}

/// Exact Thunderbird resource ownership for one extension connection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Inventory {
    pub accounts: Vec<String>,
    pub folders: BTreeMap<String, String>,
    pub identities: BTreeMap<String, Identity>,
    pub default_identity: Option<String>,
    pub trash_folders: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub account_id: String,
    pub email: String,
    pub drafts_folder: Option<String>,
    pub sent_folder: Option<String>,
}

impl Capabilities {
    pub fn has_module(&self, module: &str) -> bool {
        self.modules.iter().any(|m| m == module)
    }
}

/// First frame a peer sends after connecting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    pub protocol: u32,
    pub role: Role,
    /// Free-form peer name, for example `tb 0.1.0`.
    #[serde(default)]
    pub name: String,
    /// Required for `extension`, ignored for `client`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Capabilities>,
}

/// Daemon answer to `hello`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Welcome {
    pub protocol: u32,
    pub daemon_version: String,
    /// Whether an extension is connected right now.
    pub extension_connected: bool,
    /// Capabilities of the connected extension, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension: Option<Capabilities>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ErrorBody {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
            data: None,
        }
    }
}

/// One WebSocket text message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Frame {
    Hello(Hello),
    Welcome(Welcome),
    /// Ask the peer to run an operation. Sent by clients to the daemon and by the daemon to the
    /// extension (with a daemon-chosen id).
    #[serde(rename_all = "camelCase")]
    Request {
        id: String,
        op: String,
        #[serde(default)]
        params: Value,
        /// Per-request timeout in milliseconds. Defaults to 120 000 at the daemon.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
    /// Final frame of a request. Either `result` or `error` is set. When `streamed` is true the
    /// result was delivered in `chunk` frames and `result` is absent.
    #[serde(rename_all = "camelCase")]
    Response {
        id: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<ErrorBody>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        streamed: bool,
    },
    /// A slice of the JSON text of a streamed result. Concatenating the `data` of all chunks
    /// of one id in `seq` order (starting at 0) yields the result document.
    Chunk {
        id: String,
        seq: u64,
        data: String,
    },
    /// Abandon a request.
    Cancel {
        id: String,
    },
    Ping,
    Pong,
}

impl Frame {
    pub fn ok(id: impl Into<String>, result: Value) -> Self {
        Frame::Response {
            id: id.into(),
            ok: true,
            result: Some(result),
            error: None,
            streamed: false,
        }
    }

    pub fn err(id: impl Into<String>, code: &str, message: impl Into<String>) -> Self {
        Frame::Response {
            id: id.into(),
            ok: false,
            result: None,
            error: Some(ErrorBody::new(code, message)),
            streamed: false,
        }
    }

    pub fn to_text(&self) -> String {
        serde_json::to_string(self).expect("frames always serialize")
    }

    pub fn parse(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_round_trips() {
        let text = r#"{"type":"request","id":"1","op":"listAccounts"}"#;
        let frame = Frame::parse(text).unwrap();
        assert_eq!(
            frame,
            Frame::Request {
                id: "1".into(),
                op: "listAccounts".into(),
                params: Value::Null,
                timeout_ms: None
            }
        );
    }

    #[test]
    fn response_omits_empty_fields() {
        let text = Frame::ok("7", json!({"a": 1})).to_text();
        assert_eq!(
            text,
            r#"{"type":"response","id":"7","ok":true,"result":{"a":1}}"#
        );
    }

    #[test]
    fn hello_uses_camel_case() {
        let text = r#"{"type":"hello","protocol":1,"role":"extension","name":"x","capabilities":{"extensionVersion":"1","modules":["core"]}}"#;
        let Frame::Hello(h) = Frame::parse(text).unwrap() else {
            panic!("not a hello")
        };
        assert_eq!(h.role, Role::Extension);
        assert!(h.capabilities.unwrap().has_module("core"));
    }
}
