//! HMAC-SHA256 over remote manifest bytes.
//!
//! Local manifests stay unsigned: the disk is trusted. A remote object is
//! signed only when [`SIGNATURE_KEY_ENV`] is set. The MAC covers the exact
//! [`super::artifact::Manifest::encode`] bytes, which already name every blob
//! digest, so a swapped blob fails restore even if the envelope is rewritten.
//!
//! HTTP action-cache objects become a JSON envelope so bazel-remote still
//! sees `application/json`. The envelope carries `signed: true`, which a
//! [`Manifest`] does not, so an unsigned reader cannot decode the wrapper
//! as a manifest and ignore `sig`. aster-srv stores the encode bytes in
//! Postgres and does not return unknown fields, so this client does not
//! sign that path: a signature the server drops would make every lookup a
//! miss.

use std::sync::atomic::{AtomicBool, Ordering};

use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use super::artifact::Manifest;

/// `ASTER_REMOTE_CACHE_SIGNATURE_KEY`. Unset or empty means remote manifests
/// are unsigned, which is the historical wire format.
pub const SIGNATURE_KEY_ENV: &str = "ASTER_REMOTE_CACHE_SIGNATURE_KEY";

type HmacSha256 = Hmac<Sha256>;

static WARNED: AtomicBool = AtomicBool::new(false);

/// Trimmed key, or `None` when unset or whitespace.
pub fn signing_key() -> Option<String> {
    std::env::var(SIGNATURE_KEY_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Lowercase hex HMAC-SHA256 of `bytes` under `key`.
pub fn sign(key: &str, bytes: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(key.as_bytes()).expect("HMAC accepts any key length");
    mac.update(bytes);
    hex_encode(&mac.finalize().into_bytes())
}

/// Constant-time check that `sig` is the hex HMAC of `bytes` under `key`.
pub fn verify(key: &str, bytes: &[u8], sig: &str) -> bool {
    let Ok(expected) = hex_decode(sig.trim()) else {
        return false;
    };
    let mut mac = HmacSha256::new_from_slice(key.as_bytes()).expect("HMAC accepts any key length");
    mac.update(bytes);
    mac.verify_slice(&expected).is_ok()
}

/// HTTP action-cache body. `manifest` is the UTF-8 of [`Manifest::encode`].
///
/// `signed` is required and must be `true`. [`Manifest`] has no such field,
/// so [`Manifest::decode`] rejects this object instead of skipping `sig`.
#[derive(Debug, Serialize, Deserialize)]
struct Envelope {
    manifest: String,
    sig: Option<String>,
    signed: bool,
}

/// Bytes to `PUT` for `manifest`. Unsigned when `key` is `None`.
pub fn seal(key: Option<&str>, manifest: &Manifest) -> Vec<u8> {
    let encoded = manifest.encode();
    let Some(key) = key else {
        return encoded;
    };
    let envelope = Envelope {
        manifest: String::from_utf8(encoded.clone()).expect("manifest JSON is UTF-8"),
        sig: Some(sign(key, &encoded)),
        signed: true,
    };
    serde_json::to_vec(&envelope).expect("signature envelope is JSON")
}

/// Manifest inside a remote body, or a miss.
///
/// With no key, a plain manifest decodes and an envelope is a miss: the
/// historical reader does not understand the wrapper. With a key, a missing
/// or wrong `sig` is a miss and warns once. The key is never included in the
/// warning.
pub fn open(key: Option<&str>, bytes: &[u8]) -> Option<Manifest> {
    match key {
        None => Manifest::decode(bytes),
        Some(key) => open_signed(key, bytes),
    }
}

fn open_signed(key: &str, bytes: &[u8]) -> Option<Manifest> {
    let envelope = match serde_json::from_slice::<Envelope>(bytes) {
        Ok(envelope) if envelope.signed => envelope,
        _ => {
            warn_once();
            return None;
        }
    };
    let Some(sig) = envelope.sig.as_deref().filter(|sig| !sig.is_empty()) else {
        warn_once();
        return None;
    };
    let manifest_bytes = envelope.manifest.as_bytes();
    if !verify(key, manifest_bytes, sig) {
        warn_once();
        return None;
    }
    Manifest::decode(manifest_bytes)
}

fn warn_once() {
    if WARNED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        eprintln!(
            "[aster] Warning: remote cache manifest signature is missing or invalid; treating it as a cache miss"
        );
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

fn hex_decode(sig: &str) -> Result<Vec<u8>, ()> {
    if sig.len() != 64 || !sig.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(());
    }
    let mut out = Vec::with_capacity(32);
    let bytes = sig.as_bytes();
    for index in (0..64).step_by(2) {
        let high = hex_value(bytes[index])?;
        let low = hex_value(bytes[index + 1])?;
        out.push((high << 4) | low);
    }
    Ok(out)
}

fn hex_value(byte: u8) -> Result<u8, ()> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::artifact::{Digest, OutputEntry, OutputKind, MANIFEST_SCHEMA_VERSION};

    fn manifest() -> Manifest {
        Manifest {
            schema: MANIFEST_SCHEMA_VERSION,
            key: "ab".repeat(32),
            target: "//app:build".to_string(),
            exit_code: 0,
            duration_ms: 12,
            created_at: "2026-10-20T00:00:00Z".to_string(),
            outputs: vec![OutputEntry {
                path: "out/app".to_string(),
                kind: OutputKind::File,
                digest: Some(Digest::of_bytes(b"blob")),
                mode: 0o644,
                target: None,
            }],
            log: None,
            aster_version: "0.21.0".to_string(),
        }
    }

    #[test]
    fn hmac_sha256_matches_rfc4231_case_1() {
        // RFC 4231 test case 1. Key is 20 bytes of 0x0b; data is "Hi There".
        let key = "\u{0b}".repeat(20);
        assert_eq!(
            sign(&key, b"Hi There"),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn valid_signature_hits_and_tamper_or_wrong_key_misses() {
        let manifest = manifest();
        let bytes = manifest.encode();
        let sealed = seal(Some("test-key"), &manifest);
        let hit = open(Some("test-key"), &sealed).expect("valid signature is a hit");
        assert_eq!(hit, manifest);

        let mut tampered: serde_json::Value = serde_json::from_slice(&sealed).unwrap();
        tampered["manifest"] =
            serde_json::Value::String(String::from_utf8(bytes.clone()).unwrap() + " ");
        let tampered_bytes = serde_json::to_vec(&tampered).unwrap();
        assert!(open(Some("test-key"), &tampered_bytes).is_none());

        assert!(open(Some("other-key"), &sealed).is_none());
        assert!(open(Some("test-key"), &bytes).is_none());

        // No key keeps the historical unsigned wire, and does not accept an
        // envelope it cannot unwrap. Manifest::decode must not either: the
        // envelope has no manifest fields, only a string named `manifest`.
        assert_eq!(seal(None, &manifest), bytes);
        assert_eq!(open(None, &bytes), Some(manifest));
        assert!(open(None, &sealed).is_none());
        assert!(Manifest::decode(&sealed).is_none());
    }
}
