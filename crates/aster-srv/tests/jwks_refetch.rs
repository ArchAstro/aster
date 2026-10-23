//! When the verifiers refetch their issuer's signing keys, against a stub
//! JWKS server that counts fetches and can be made to fail.
//!
//! No database needed.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aster_srv::auth::Verifier;
use aster_srv::iap::IapVerifier;
use aster_srv::keyfetch::RefetchPolicy;
use aster_srv::Config;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64URL;
use base64::Engine;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PRIVATE: &[u8] = include_bytes!("fixtures/test-oidc-key.pem");
const JWKS_N: &str = "855qXNm0Qs-azcPsy_QGxBFH4BPpxL3czNPvP4XV1XRv3tCIbacibo4Vbkvop-Z-4G70ZP-KNgMx_ItXqNpWN1erdKh_JWl0OVPMZpeYJH5T1jV2beTiflxah3pAAvv2lPC0m5QZR9dc6mE2XKcwyPPdhVq1CBeHmksnC8gQjM8iiQkWDuEU4FKbPxzQ6bQ0sau-STvZfy9eP8wQk1kxXE6acJaJaLd45Qqy3rTHVTOjCt3P_gjAbxY_bHY_zFZyn3hBrpuxgJN0fkIsAWHjWPKQzcGNnfbr4UI0R0YKiC69GkbWBUy1Jv-6d5o9cecUSVACA9SdH3A0TobNMwWXmQ";
const IAP_AUDIENCE: &str = "/projects/1/global/backendServices/42";

/// Fast enough to test; the production defaults are 10s and 60s.
const POLICY: RefetchPolicy = RefetchPolicy {
    min_interval: Duration::from_millis(400),
    absent_for: Duration::from_secs(60),
};

/// A JWKS endpoint: it serves `kids` (as `rsa` or `ec` keys) while `healthy`
/// and a 500 otherwise, and counts every request.
struct Jwks {
    url: String,
    fetches: Arc<AtomicUsize>,
    healthy: Arc<AtomicBool>,
    kids: Arc<Mutex<Vec<String>>>,
}

async fn jwks(kind: &'static str) -> Jwks {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/jwks", listener.local_addr().unwrap());
    let stub = Jwks {
        url,
        fetches: Arc::default(),
        healthy: Arc::new(AtomicBool::new(true)),
        kids: Arc::default(),
    };
    let (fetches, healthy, kids) = (
        Arc::clone(&stub.fetches),
        Arc::clone(&stub.healthy),
        Arc::clone(&stub.kids),
    );
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            fetches.fetch_add(1, Ordering::SeqCst);
            let ok = healthy.load(Ordering::SeqCst);
            let keys: Vec<_> = kids
                .lock()
                .unwrap()
                .iter()
                .map(|kid| match kind {
                    "rsa" => json!({"kty": "RSA", "kid": kid, "use": "sig", "alg": "RS256",
                                    "n": JWKS_N, "e": "AQAB"}),
                    _ => {
                        let point = &ec_pair().1;
                        json!({"kty": "EC", "crv": "P-256", "kid": kid, "use": "sig", "alg": "ES256",
                               "x": BASE64URL.encode(&point[1..33]), "y": BASE64URL.encode(&point[33..65])})
                    }
                })
                .collect();
            tokio::spawn(async move {
                let mut buffer = [0u8; 2048];
                let _ = stream.read(&mut buffer).await;
                let reply = if ok {
                    let body = json!({"keys": keys}).to_string();
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                         connection: close\r\n\r\n{body}",
                        body.len()
                    )
                } else {
                    "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        .to_string()
                };
                let _ = stream.write_all(reply.as_bytes()).await;
            });
        }
    });
    stub
}

impl Jwks {
    fn serve(&self, kids: &[&str]) {
        *self.kids.lock().unwrap() = kids.iter().map(|k| k.to_string()).collect();
    }
    fn fetches(&self) -> usize {
        self.fetches.load(Ordering::SeqCst)
    }
}

fn ec_pair() -> &'static (Vec<u8>, Vec<u8>) {
    use aws_lc_rs::rand::SystemRandom;
    use aws_lc_rs::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
    static PAIR: std::sync::OnceLock<(Vec<u8>, Vec<u8>)> = std::sync::OnceLock::new();
    PAIR.get_or_init(|| {
        let pkcs8 =
            EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new())
                .unwrap();
        let pair =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref()).unwrap();
        (pkcs8.as_ref().to_vec(), pair.public_key().as_ref().to_vec())
    })
}

fn config() -> Config {
    Config::from_env(|name| match name {
        "ASTER_SRV_DATABASE_URL" => Some("postgres://localhost/x".into()),
        "ASTER_SRV_REPOSITORIES" => Some("ArchAstro/*".into()),
        "ASTER_SRV_IAP_AUDIENCE" => Some(IAP_AUDIENCE.into()),
        _ => None,
    })
    .unwrap()
}

fn oidc(kid: &str) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(kid.into());
    let token = encode(
        &header,
        &json!({
            "iss": aster_srv::config::GITHUB_ISSUER, "aud": "aster-srv",
            "exp": chrono::Utc::now().timestamp() + 300,
            "repository": "ArchAstro/firstlanding",
        }),
        &EncodingKey::from_rsa_pem(PRIVATE).unwrap(),
    )
    .unwrap();
    format!("Bearer {token}")
}

fn iap(kid: &str) -> String {
    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some(kid.into());
    encode(
        &header,
        &json!({
            "iss": "https://cloud.google.com/iap", "aud": IAP_AUDIENCE,
            "iat": chrono::Utc::now().timestamp(),
            "exp": chrono::Utc::now().timestamp() + 300,
            "email": "ada@example.com", "sub": "accounts.google.com:1",
        }),
        &EncodingKey::from_ec_der(&ec_pair().0),
    )
    .unwrap()
}

async fn pause() {
    tokio::time::sleep(POLICY.min_interval + Duration::from_millis(100)).await;
}

/// N9b: a failed fetch used to stamp the refetch time before the request, so
/// the next real rotation was refused for a full minute. Now it waits only
/// the short interval.
#[tokio::test]
async fn a_failed_key_fetch_does_not_block_the_next_real_rotation() {
    let stub = jwks("rsa").await;
    stub.healthy.store(false, Ordering::SeqCst);
    let verifier = Verifier::new(&config()).with_jwks(&stub.url, POLICY);
    // The issuer is down: the first token fails, and costs one fetch.
    assert!(verifier.verify(Some(&oidc("rotated"))).await.is_err());
    assert_eq!(stub.fetches(), 1);
    // Retries inside the short interval do not fetch again.
    assert!(verifier.verify(Some(&oidc("rotated"))).await.is_err());
    assert_eq!(stub.fetches(), 1);
    // The issuer recovers and has the rotated key: the next token after the
    // short interval succeeds, not a minute later.
    stub.healthy.store(true, Ordering::SeqCst);
    stub.serve(&["rotated"]);
    pause().await;
    let identity = verifier.verify(Some(&oidc("rotated"))).await.unwrap();
    assert_eq!(identity.repository, "ArchAstro/firstlanding");
    assert_eq!(stub.fetches(), 2);
}

/// N9b: a flood of unknown keys costs the issuer at most one fetch per
/// interval, the same unknown key is not looked for again for a minute, and
/// a real rotation behind the flood is still picked up after the short
/// interval.
#[tokio::test]
async fn an_unknown_key_flood_does_not_cause_a_refetch_storm_or_hide_a_rotation() {
    let stub = jwks("rsa").await;
    stub.serve(&["current"]);
    let verifier = Arc::new(Verifier::new(&config()).with_jwks(&stub.url, POLICY));
    assert!(verifier.verify(Some(&oidc("current"))).await.is_ok());
    assert_eq!(stub.fetches(), 1);
    pause().await;
    // 200 distinct random keys at once (signed first, so the flood is
    // faster than the refetch interval).
    let probes: Vec<String> = (0..200).map(|n| oidc(&format!("probe-{n}"))).collect();
    let mut flood = tokio::task::JoinSet::new();
    for probe in probes {
        let verifier = Arc::clone(&verifier);
        flood.spawn(async move { verifier.verify(Some(&probe)).await });
    }
    while let Some(result) = flood.join_next().await {
        assert!(result.unwrap().is_err());
    }
    assert_eq!(stub.fetches(), 2, "200 probes cost one fetch");
    // One probe after the interval costs one fetch and is remembered as
    // absent: the same key again, even after the interval, costs none.
    pause().await;
    assert!(verifier.verify(Some(&oidc("lone-probe"))).await.is_err());
    assert_eq!(stub.fetches(), 3);
    pause().await;
    assert!(verifier.verify(Some(&oidc("lone-probe"))).await.is_err());
    assert_eq!(stub.fetches(), 3, "a known-absent key is not fetched for");
    // A real rotation, still inside the minute the probes were remembered.
    stub.serve(&["current", "rotated"]);
    let identity = verifier.verify(Some(&oidc("rotated"))).await.unwrap();
    assert_eq!(identity.repository, "ArchAstro/firstlanding");
    assert_eq!(stub.fetches(), 4);
}

/// N9b, IAP: the same rules on the dashboard's verifier.
#[tokio::test]
async fn the_iap_verifier_follows_the_same_refetch_rules() {
    let stub = jwks("ec").await;
    stub.healthy.store(false, Ordering::SeqCst);
    let verifier = IapVerifier::new(&config()).with_jwks(&stub.url, POLICY);
    assert!(verifier.verify(Some(&iap("rotated"))).await.is_err());
    assert!(verifier.verify(Some(&iap("rotated"))).await.is_err());
    assert_eq!(stub.fetches(), 1);
    stub.healthy.store(true, Ordering::SeqCst);
    stub.serve(&["rotated"]);
    pause().await;
    let viewer = verifier.verify(Some(&iap("rotated"))).await.unwrap();
    assert_eq!(viewer.email, "ada@example.com");
    assert_eq!(stub.fetches(), 2);
    // A probe is remembered, a rotation behind it is not blocked.
    pause().await;
    assert!(verifier.verify(Some(&iap("probe"))).await.is_err());
    assert!(verifier.verify(Some(&iap("probe"))).await.is_err());
    assert_eq!(stub.fetches(), 3);
    stub.serve(&["rotated", "next"]);
    pause().await;
    assert!(verifier.verify(Some(&iap("next"))).await.is_ok());
    assert_eq!(stub.fetches(), 4);
}
