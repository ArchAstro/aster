//! Failure message handling: grouping similar failures and keeping secrets
//! out of stored messages.

use std::sync::LazyLock;

use regex::Regex;
use sha2::{Digest, Sha256};

/// Stored failure messages are cut to this many bytes.
pub const MAX_MESSAGE_BYTES: usize = 4096;

static TEMP_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:/private)?/(?:tmp|var/folders|var/tmp)/[^\s:'\x22)]*|[A-Za-z]:\\[^\s]*\\Temp\\[^\s:'\x22)]*")
        .unwrap()
});
static HEX_ADDRESS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"0x[0-9a-fA-F]+").unwrap());
static UUID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b")
        .unwrap()
});
static LONG_HEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b[0-9a-f]{12,}\b").unwrap());
static NUMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+").unwrap());
static WHITESPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

static BEARER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(bearer|basic)\s+[A-Za-z0-9._~+/=-]{8,}").unwrap());
static URL_CREDENTIALS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([a-zA-Z][a-zA-Z0-9+.-]*://)[^\s/@:]+:[^\s/@]+@").unwrap());
static KNOWN_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"\b(?:",
        r"gh[pousr]_[A-Za-z0-9]{20,}",
        r"|github_pat_[A-Za-z0-9_]{20,}",
        r"|glpat-[A-Za-z0-9_-]{20,}",
        r"|xox[abprs]-[A-Za-z0-9-]{10,}",
        r"|sk-[A-Za-z0-9_-]{20,}",
        r"|AKIA[0-9A-Z]{16}",
        r"|eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
        r")"
    ))
    .unwrap()
});
static SECRET_ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\b([A-Z0-9_]*(?:SECRET|PASSWORD|PASSWD|API_KEY|TOKEN)[A-Z0-9_]*)(\s*[=:]\s*)("[^"]*"|'[^']*'|\S+)"#)
        .unwrap()
});

/// Hash a failure message so the same failure groups together across runs.
///
/// Parts that change from run to run without changing what failed are
/// replaced before hashing: temp paths, hex addresses, UUIDs, long hex ids,
/// and every number (line numbers, PIDs such as `#PID<0.512.0>`, ports,
/// durations). Whitespace runs collapse to one space.
pub fn failure_hash(message: &str) -> String {
    let normalized = TEMP_PATH.replace_all(message, "<tmp>");
    let normalized = UUID.replace_all(&normalized, "<uuid>");
    let normalized = HEX_ADDRESS.replace_all(&normalized, "0x");
    let normalized = LONG_HEX.replace_all(&normalized, "<hex>");
    let normalized = NUMBER.replace_all(&normalized, "N");
    let normalized = WHITESPACE.replace_all(normalized.trim(), " ");
    crate::encode_hex(&Sha256::digest(normalized.as_bytes()))
}

/// Remove likely secrets from a failure message and cut it to
/// [`MAX_MESSAGE_BYTES`] at a character boundary.
pub fn scrub_message(message: &str) -> String {
    let scrubbed = URL_CREDENTIALS.replace_all(message, "${1}[redacted]@");
    let scrubbed = BEARER.replace_all(&scrubbed, "${1} [redacted]");
    let scrubbed = KNOWN_TOKEN.replace_all(&scrubbed, "[redacted]");
    let scrubbed = SECRET_ASSIGNMENT.replace_all(&scrubbed, "${1}${2}[redacted]");
    truncate(&scrubbed, MAX_MESSAGE_BYTES)
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_ignores_line_numbers_pids_and_addresses() {
        let a = "** (DBConnection.OwnershipError) cannot find owner for #PID<0.512.0> at test/a_test.exs:12";
        let b = "** (DBConnection.OwnershipError) cannot find owner for #PID<0.9981.0> at test/a_test.exs:48";
        assert_eq!(failure_hash(a), failure_hash(b));

        let c = "segfault at 0x7ffd5e8c";
        let d = "segfault at 0x10ab";
        assert_eq!(failure_hash(c), failure_hash(d));
    }

    #[test]
    fn hash_ignores_temp_paths_and_uuids() {
        let a = "missing /tmp/aster-1234/out.json for 3f2b1c9e-1a2b-4c3d-8e9f-0a1b2c3d4e5f";
        let b = "missing /private/var/folders/xy/T/aster-77/out.json for 9e8d7c6b-5a4f-4e3d-2c1b-0a9f8e7d6c5b";
        assert_eq!(failure_hash(a), failure_hash(b));
    }

    #[test]
    fn hash_ignores_whitespace_layout() {
        assert_eq!(
            failure_hash("expected 1\n   got 2"),
            failure_hash("  expected 3 got 4  ")
        );
    }

    #[test]
    fn different_failures_hash_differently() {
        assert_ne!(
            failure_hash("assertion failed: left == right"),
            failure_hash("timeout after 5000ms")
        );
    }

    #[test]
    fn scrubs_bearer_and_url_credentials() {
        let message =
            "GET https://bot:hunter22@example.com/x failed; Authorization: Bearer abcdefghijklmnop";
        let scrubbed = scrub_message(message);
        assert!(!scrubbed.contains("hunter22"), "{scrubbed}");
        assert!(!scrubbed.contains("abcdefghijklmnop"), "{scrubbed}");
        assert!(scrubbed.contains("https://[redacted]@example.com/x"));
        assert!(scrubbed.contains("Bearer [redacted]"));
    }

    #[test]
    fn scrubs_known_token_shapes() {
        for token in [
            "ghp_0123456789abcdefghijABCDEFGHIJ",
            "github_pat_11ABCDEFG0123456789_abcdefghij",
            "xoxb-1234567890-abcdefghij",
            "sk-ant-api03-abcdefghijklmnopqrstuv",
            "AKIAIOSFODNN7EXAMPLE",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abcdefghijklmnop",
        ] {
            let scrubbed = scrub_message(&format!("leaked {token} here"));
            assert_eq!(scrubbed, "leaked [redacted] here", "{token}");
        }
    }

    #[test]
    fn scrubs_secret_assignments() {
        let scrubbed = scrub_message("env DATABASE_PASSWORD=s3cr3t and API_KEY: \"abc def\"");
        assert!(!scrubbed.contains("s3cr3t"), "{scrubbed}");
        assert!(!scrubbed.contains("abc def"), "{scrubbed}");
        assert!(scrubbed.contains("DATABASE_PASSWORD=[redacted]"));
    }

    #[test]
    fn leaves_ordinary_messages_alone() {
        for message in [
            "assert_eq!(left, right) failed\n  left: 1\n right: 2",
            "SyntaxError: Unexpected token identifierName",
            "expected token: Some(\"abc\") got None",
        ] {
            assert_eq!(scrub_message(message), message);
        }
    }

    #[test]
    fn truncates_at_a_char_boundary() {
        let message = "é".repeat(MAX_MESSAGE_BYTES);
        let scrubbed = scrub_message(&message);
        assert!(scrubbed.len() <= MAX_MESSAGE_BYTES);
        assert!(scrubbed.len() >= MAX_MESSAGE_BYTES - 1);
        assert!(scrubbed.chars().all(|c| c == 'é'));
    }
}
