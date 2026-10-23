//! One JSON object per log line, in the shape Cloud Logging reads
//! (`severity`, `message`).
//!
//! Nothing secret is written. Before a line is printed, [`redact`] replaces
//! the value of any field named like a credential (`authorization`,
//! `proxy-authorization`, `x-goog-iap-jwt-assertion`, `cookie`, `token`,
//! `code`, `verifier`, `secret`, ...) and any token-shaped text inside a
//! string value (a JWT, or `Bearer <credential>`) with `[redacted]`.

use std::sync::Mutex;

use serde_json::{json, Map, Value};

const REDACTED: &str = "[redacted]";

/// Field names whose values are credentials, compared case-insensitively.
const SECRET_FIELDS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "proxy_authorization",
    "x-goog-iap-jwt-assertion",
    "x_goog_iap_jwt_assertion",
    "assertion",
    "cookie",
    "set-cookie",
    "token",
    "access_token",
    "code",
    "verifier",
    "secret",
    "password",
];

fn is_secret_field(name: &str) -> bool {
    SECRET_FIELDS.iter().any(|f| name.eq_ignore_ascii_case(f))
}

fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'+' | b'/' | b'=')
}

/// `text` with JWT-shaped runs and `Bearer x` credentials replaced.
pub fn redact_text(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        let rest = &text[i..];
        // `eyJ` opens a base64url JSON header: a JWT, or part of one.
        if rest.starts_with("eyJ") {
            let end = rest
                .bytes()
                .position(|b| !is_token_char(b))
                .unwrap_or(rest.len());
            out.push_str(REDACTED);
            i += end;
            continue;
        }
        let lower_bearer = rest.len() >= 7 && rest.as_bytes()[..7].eq_ignore_ascii_case(b"bearer ");
        if lower_bearer {
            out.push_str(&rest[..7]);
            i += 7;
            let end = text[i..]
                .bytes()
                .position(|b| !is_token_char(b))
                .unwrap_or(text.len() - i);
            if end > 0 {
                out.push_str(REDACTED);
            }
            i += end;
            continue;
        }
        let ch = rest.chars().next().expect("not at the end");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Redact `value` in place.
pub fn redact(value: &mut Value) {
    match value {
        Value::String(text) => *text = redact_text(text),
        Value::Array(items) => items.iter_mut().for_each(redact),
        Value::Object(map) => {
            for (name, item) in map.iter_mut() {
                if is_secret_field(name) && !item.is_null() {
                    *item = json!(REDACTED);
                } else {
                    redact(item);
                }
            }
        }
        _ => {}
    }
}

/// Lines kept instead of printed, when a test turned capture on.
static CAPTURE: Mutex<Option<Vec<String>>> = Mutex::new(None);

/// Keep log lines in memory instead of printing them. For tests that assert
/// on what the server logged.
pub fn start_capture() {
    CAPTURE.lock().unwrap().get_or_insert_with(Vec::new);
}

/// The lines captured so far (everything since [`start_capture`]).
pub fn captured() -> Vec<String> {
    CAPTURE.lock().unwrap().clone().unwrap_or_default()
}

fn emit(severity: &str, message: &str, fields: Value) {
    let mut line = Map::new();
    line.insert("severity".into(), json!(severity));
    line.insert("message".into(), json!(message));
    if let Value::Object(fields) = fields {
        line.extend(fields);
    }
    let mut line = Value::Object(line);
    redact(&mut line);
    let line = line.to_string();
    match CAPTURE.lock().unwrap().as_mut() {
        Some(lines) => lines.push(line),
        None => println!("{line}"),
    }
}

pub fn info(message: &str, fields: Value) {
    emit("INFO", message, fields);
}

pub fn warn(message: &str, fields: Value) {
    emit("WARNING", message, fields);
}

pub fn error(message: &str, fields: Value) {
    emit("ERROR", message, fields);
}

#[cfg(test)]
mod tests {
    use super::*;

    const JWT: &str = "eyJhbGciOiJIUzI1NiIsImtpZCI6InQxIn0.eyJzdWIiOiJhZGEifQ.c2lnbmF0dXJlLWJ5dGVz";

    #[test]
    fn credential_fields_and_token_shaped_text_are_redacted() {
        let mut line = json!({
            "Authorization": format!("Bearer {JWT}"),
            "x-goog-iap-jwt-assertion": JWT,
            "Proxy-Authorization": "Basic abc",
            "error": format!("bad token {JWT} for ada"),
            "nested": {"verifier": "v", "list": [format!("bearer {JWT}")]},
            "route": "/v1/cache",
            "count": 3,
        });
        redact(&mut line);
        let text = line.to_string();
        assert!(!text.contains("eyJ"), "{text}");
        assert!(!text.contains("c2lnbmF0dXJl"), "{text}");
        assert!(!text.contains("Basic abc"));
        assert_eq!(line["route"], "/v1/cache");
        assert_eq!(line["count"], 3);
        assert_eq!(line["error"], "bad token [redacted] for ada");
        assert_eq!(line["nested"]["list"][0], "bearer [redacted]");
    }

    #[test]
    fn plain_text_is_untouched() {
        assert_eq!(
            redact_text("the token is not valid: ExpiredSignature"),
            "the token is not valid: ExpiredSignature"
        );
        assert_eq!(redact_text("héllo wörld"), "héllo wörld");
    }
}
