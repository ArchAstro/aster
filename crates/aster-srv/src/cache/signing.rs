//! V4 signed URLs for one GCS object.
//!
//! The signature is RSA-SHA256 over the canonical request, produced by IAM
//! Credentials `signBlob` as the pod's own service account. No key file is
//! stored or loaded. Tests inject a signer that records the payload and
//! returns a fixed signature, so the suite can prove the URL expires and
//! names no key.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use base64::Engine;
use serde::Serialize;

/// How long a signed URL stays valid.
pub const URL_TTL: Duration = Duration::from_secs(15 * 60);

const SIGN_SCOPE: &str = "https://www.googleapis.com/auth/iam";

/// What a signed URL may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Put,
}

impl Method {
    fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Put => "PUT",
        }
    }
}

/// Signs a GCS V4 URL. The signer never holds a private key.
pub trait Signer: Send + Sync {
    fn sign<'a>(&'a self, payload: &'a [u8]) -> BoxFuture<'a, Result<Vec<u8>>>;
    /// `name@project.iam.gserviceaccount.com`, the credential the URL names.
    fn service_account_email(&self) -> &str;
}

type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// Sign through IAM Credentials `signBlob` as the pod's service account.
pub struct IamSigner {
    client: reqwest::Client,
    email: String,
    access_token: String,
}

impl IamSigner {
    /// The pod's own service account and a token that can call `signBlob`.
    pub async fn from_metadata() -> Result<Self> {
        let provider = gcp_auth::provider()
            .await
            .context("no Google credentials")?;
        let access = provider
            .token(&[SIGN_SCOPE])
            .await
            .context("cannot get a Google access token")?;
        let email = service_account_email()
            .await
            .context("cannot read the pod service account")?;
        Ok(Self {
            client: crate::http_client()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("HTTP client builds"),
            email,
            access_token: access.as_str().to_string(),
        })
    }
}

async fn service_account_email() -> Result<String> {
    // Workload Identity exposes the account email on the metadata server.
    // gcp_auth does not surface it, so read the same metadata the token came from.
    let client = crate::http_client()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("HTTP client builds");
    let email = client
        .get("http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/email")
        .header("Metadata-Flavor", "Google")
        .send()
        .await
        .context("cannot reach the metadata server")?
        .error_for_status()
        .context("the metadata server refused the service account email")?
        .text()
        .await?;
    let email = email.trim().to_string();
    if email.is_empty() || !email.contains('@') {
        anyhow::bail!("the metadata server returned no service account email");
    }
    Ok(email)
}

#[derive(Serialize)]
struct SignBlobRequest<'a> {
    payload: &'a str,
}

#[derive(serde::Deserialize)]
struct SignBlobResponse {
    #[serde(rename = "signedBlob")]
    signed_blob: String,
}

impl Signer for IamSigner {
    fn sign<'a>(&'a self, payload: &'a [u8]) -> BoxFuture<'a, Result<Vec<u8>>> {
        Box::pin(async move {
            let body = SignBlobRequest {
                payload: &base64::engine::general_purpose::STANDARD.encode(payload),
            };
            let url = format!(
                "https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/{}:signBlob",
                self.email
            );
            let signed: SignBlobResponse = self
                .client
                .post(url)
                .bearer_auth(&self.access_token)
                .json(&body)
                .send()
                .await
                .context("cannot reach IAM Credentials")?
                .error_for_status()
                .context("IAM Credentials refused signBlob")?
                .json()
                .await
                .context("cannot read the signature")?;
            base64::engine::general_purpose::STANDARD
                .decode(signed.signed_blob.trim())
                .context("the signature is not base64")
        })
    }

    fn service_account_email(&self) -> &str {
        &self.email
    }
}

/// A URL the client uses directly against GCS.
pub async fn signed_url(
    signer: &dyn Signer,
    bucket: &str,
    object: &str,
    method: Method,
    now: SystemTime,
    ttl: Duration,
) -> Result<String> {
    let expires = ttl.as_secs().min(604_800);
    let datetime = format_datetime(now);
    let datestamp = &datetime[..8];
    let credential_scope = format!("{datestamp}/auto/storage/goog4_request");
    let credential = format!("{}/{}", signer.service_account_email(), credential_scope);
    let canonical_headers = format!(
        "host:storage.googleapis.com\nx-goog-content-sha256:UNSIGNED-PAYLOAD\nx-goog-date:{datetime}\n"
    );
    let signed_headers = "host;x-goog-content-sha256;x-goog-date";
    let canonical_query = canonical_query(&credential, &datetime, expires, signed_headers);
    let encoded_object = canonical_object(object);
    let canonical_request = format!(
        "{method}\n/{bucket}/{encoded_object}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\nUNSIGNED-PAYLOAD",
        method = method.as_str(),
    );
    let hash = sha256_hex(canonical_request.as_bytes());
    let string_to_sign = format!("GOOG4-RSA-SHA256\n{datetime}\n{credential_scope}\n{hash}");
    let signature = hex_encode(&signer.sign(string_to_sign.as_bytes()).await?);
    Ok(format!(
        "https://storage.googleapis.com/{bucket}/{object}?{canonical_query}&X-Goog-Signature={signature}"
    ))
}

fn canonical_object(object: &str) -> String {
    // GCS hashes each path segment, then joins them with `%2F`. The URL
    // itself keeps the slashes; only the string to sign encodes them.
    object
        .split('/')
        .map(percent_encode)
        .collect::<Vec<_>>()
        .join("%2F")
}

fn canonical_query(credential: &str, datetime: &str, expires: u64, signed_headers: &str) -> String {
    // Query parameters must be sorted by name. The signature is appended after.
    let expires = expires.to_string();
    let pairs = [
        ("X-Goog-Algorithm", "GOOG4-RSA-SHA256"),
        ("X-Goog-Credential", credential),
        ("X-Goog-Date", datetime),
        ("X-Goog-Expires", expires.as_str()),
        ("X-Goog-SignedHeaders", signed_headers),
    ];
    pairs
        .iter()
        .map(|(name, value)| format!("{name}={}", percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn format_datetime(now: SystemTime) -> String {
    let secs = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let days = secs / 86_400;
    let time = secs % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z",
        hour = time / 3600,
        minute = (time % 3600) / 60,
        second = time % 60,
    )
}

/// Days since the Unix epoch to a civil date. Howard Hinnant's algorithm.
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = (if month <= 2 { y + 1 } else { y }) as i32;
    (year, month, day)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex_encode(&Sha256::digest(bytes))
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

fn percent_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// When a signed URL stops being valid, from its `X-Goog-Date` and
/// `X-Goog-Expires`. `None` when those parameters are missing or unreadable.
pub fn expires_at(url: &str) -> Option<SystemTime> {
    let query = url.split_once('?')?.1;
    let mut date = None;
    let mut expires = None;
    for pair in query.split('&') {
        let (name, value) = pair.split_once('=')?;
        match name {
            "X-Goog-Date" => date = Some(value.to_string()),
            "X-Goog-Expires" => expires = value.parse::<u64>().ok(),
            _ => {}
        }
    }
    let signed = parse_datetime(&date?)?;
    Some(signed + Duration::from_secs(expires?))
}

fn parse_datetime(value: &str) -> Option<SystemTime> {
    if value.len() != 16 || !value.ends_with('Z') {
        return None;
    }
    let year: i32 = value[0..4].parse().ok()?;
    let month: u32 = value[4..6].parse().ok()?;
    let day: u32 = value[6..8].parse().ok()?;
    let hour: u32 = value[9..11].parse().ok()?;
    let minute: u32 = value[11..13].parse().ok()?;
    let second: u32 = value[13..15].parse().ok()?;
    let days = days_from_civil(year, month, day)?;
    let secs = days * 86_400 + i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second);
    if secs < 0 {
        return None;
    }
    UNIX_EPOCH.checked_add(Duration::from_secs(secs as u64))
}

fn days_from_civil(year: i32, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year } as i64;
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if month > 2 { month - 3 } else { month + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + day as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe as i64 - 719_468)
}

/// A signer for tests. It records each payload and returns `signature`.
pub struct FixedSigner {
    pub email: String,
    pub signature: Vec<u8>,
    pub signed: std::sync::Mutex<Vec<Vec<u8>>>,
}

impl Signer for FixedSigner {
    fn sign<'a>(&'a self, payload: &'a [u8]) -> BoxFuture<'a, Result<Vec<u8>>> {
        Box::pin(async move {
            self.signed
                .lock()
                .expect("signer lock")
                .push(payload.to_vec());
            Ok(self.signature.clone())
        })
    }

    fn service_account_email(&self) -> &str {
        &self.email
    }
}
