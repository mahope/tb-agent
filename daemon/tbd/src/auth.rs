//! Session token handling.

use axum::http::{HeaderMap, header};
use rand::RngExt;

/// Subprotocol prefix browsers use to carry the token, since the WebSocket API in an
/// extension cannot set an `Authorization` header: `Sec-WebSocket-Protocol: tb.token.<token>`.
pub const SUBPROTOCOL_PREFIX: &str = "tb.token.";

/// 64 lowercase hex characters from the OS-seeded generator.
pub fn new_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Compare without leaking the position of the first difference.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// What the client presented, and whether it matches.
pub struct Presented {
    pub valid: bool,
    /// Subprotocol to echo back when the token arrived as one.
    pub subprotocol: Option<String>,
}

pub fn check(headers: &HeaderMap, token: &str) -> Presented {
    if let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        return Presented {
            valid: constant_time_eq(value.trim(), token),
            subprotocol: None,
        };
    }
    let offered = headers
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    for candidate in offered.split(',').map(str::trim) {
        if let Some(presented) = candidate.strip_prefix(SUBPROTOCOL_PREFIX) {
            return Presented {
                valid: constant_time_eq(presented, token),
                subprotocol: Some(candidate.to_owned()),
            };
        }
    }
    Presented {
        valid: false,
        subprotocol: None,
    }
}

/// Pages in a browser must not be able to talk to the daemon, even with a leaked token.
/// Non-browser clients send no `Origin`; the extension sends `moz-extension://<uuid>`.
pub fn origin_allowed(headers: &HeaderMap) -> bool {
    match headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        None => true,
        Some(origin) => origin.starts_with("moz-extension://"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_64_hex_and_unique() {
        let (a, b) = (new_token(), new_token());
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn bearer_and_subprotocol_are_checked() {
        let token = new_token();
        let mut h = HeaderMap::new();
        assert!(!check(&h, &token).valid);
        h.insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        assert!(check(&h, &token).valid);
        h.insert(header::AUTHORIZATION, "Bearer nope".parse().unwrap());
        assert!(!check(&h, &token).valid);

        let mut h = HeaderMap::new();
        h.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            format!("tb.v1, tb.token.{token}").parse().unwrap(),
        );
        let p = check(&h, &token);
        assert!(p.valid);
        assert_eq!(p.subprotocol.unwrap(), format!("tb.token.{token}"));
    }

    #[test]
    fn web_page_origins_are_refused() {
        let mut h = HeaderMap::new();
        assert!(origin_allowed(&h));
        h.insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        assert!(!origin_allowed(&h));
        h.insert(header::ORIGIN, "moz-extension://abc".parse().unwrap());
        assert!(origin_allowed(&h));
    }
}
