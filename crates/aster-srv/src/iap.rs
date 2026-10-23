//! Who is viewing the dashboard: a Google IAP assertion.
//!
//! IAP puts a signed JWT in `x-goog-iap-jwt-assertion` on every request it
//! lets through. It is accepted when it is ES256, signed by one of Google's
//! published IAP keys, unexpired, issued by `https://cloud.google.com/iap`
//! and addressed to this server's audience. Keys are fetched from the JWK set
//! and refetched when a token names a key not yet seen, at most once a
//! minute. The viewer is the token's `email`.
//!
//! GKE ingress assigns the backend service id, so an audience configured
//! with a trailing `/` matches any audience that starts with it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use tokio::sync::{Mutex, RwLock};

use crate::config::{Config, EmailDomains, IapAudience};

pub const IAP_ISSUER: &str = "https://cloud.google.com/iap";
pub const IAP_JWKS_URL: &str = "https://www.gstatic.com/iap/verify/public_key-jwk";
/// The header IAP puts its assertion in.
pub const IAP_HEADER: &str = "x-goog-iap-jwt-assertion";

const REFETCH_INTERVAL: Duration = Duration::from_secs(60);

/// Who is viewing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Viewer {
    /// IAP's stable subject, `accounts.google.com:<id>`. Unlike the email it
    /// survives a rename, so person tokens are bound to it. Development
    /// viewers (no IAP) get `dev:<email>`.
    pub subject: String,
    pub email: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ViewerError {
    /// No assertion, or one that is not valid: 401.
    Unauthenticated(String),
    /// A valid identity that may not view the dashboard: 403.
    Forbidden(String),
}

impl std::fmt::Display for ViewerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ViewerError::Unauthenticated(reason) => write!(f, "{reason}"),
            ViewerError::Forbidden(email) => write!(f, "{email} may not view this dashboard"),
        }
    }
}

#[derive(Deserialize)]
struct Claims {
    aud: String,
    sub: Option<String>,
    email: Option<String>,
}

enum Mode {
    /// The dashboard is off.
    Disabled,
    /// Trust this viewer without IAP. Development only.
    Dev(String),
    /// Verify IAP assertions for this audience.
    Iap(String),
}

pub struct IapVerifier {
    mode: Mode,
    domains: Option<EmailDomains>,
    /// Where keys come from; `None` uses only keys added with [`Self::with_key`].
    jwks_url: Option<String>,
    keys: RwLock<HashMap<String, DecodingKey>>,
    /// When keys were last fetched. Held for the whole fetch, so a caller
    /// that arrives mid-fetch waits and then finds the fetched keys.
    last_fetch: Mutex<Option<Instant>>,
    client: reqwest::Client,
}

impl IapVerifier {
    pub fn new(config: &Config) -> Self {
        let mode = match (&config.iap_audience, &config.ui_dev_email) {
            (Some(audience), _) => Mode::Iap(audience.clone()),
            (None, Some(email)) => Mode::Dev(email.clone()),
            (None, None) => Mode::Disabled,
        };
        Self {
            mode,
            domains: config.ui_email_domains.clone(),
            jwks_url: Some(IAP_JWKS_URL.to_string()),
            keys: RwLock::new(HashMap::new()),
            last_fetch: Mutex::new(None),
            client: crate::http_client()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("HTTP client builds"),
        }
    }

    /// Trust one signing key and fetch no others. For tests.
    pub fn with_key(mut self, kid: &str, key: DecodingKey) -> Self {
        self.jwks_url = None;
        self.keys.get_mut().insert(kid.to_string(), key);
        self
    }

    pub fn allows_email(&self, email: &str) -> bool {
        match &self.domains {
            Some(domains) => domains.allows(email),
            None => true,
        }
    }

    /// Whether the dashboard is on at all.
    pub fn enabled(&self) -> bool {
        !matches!(self.mode, Mode::Disabled)
    }

    /// Whether the dashboard trusts a configured viewer instead of IAP.
    pub fn is_dev(&self) -> bool {
        matches!(self.mode, Mode::Dev(_))
    }

    /// Check the `x-goog-iap-jwt-assertion` header value. Callers check
    /// [`Self::enabled`] first.
    pub async fn verify(&self, assertion: Option<&str>) -> Result<Viewer, ViewerError> {
        let audience = match &self.mode {
            Mode::Disabled => {
                return Err(ViewerError::Unauthenticated(
                    "the dashboard is disabled".into(),
                ))
            }
            Mode::Dev(email) => return self.allow(format!("dev:{email}"), email.clone()),
            Mode::Iap(audience) => audience,
        };
        let token = assertion
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| ViewerError::Unauthenticated("sign in through IAP".into()))?;
        let invalid =
            |reason: String| ViewerError::Unauthenticated(format!("invalid assertion: {reason}"));

        let header = decode_header(token).map_err(|_| invalid("it is malformed".into()))?;
        if header.alg != Algorithm::ES256 {
            return Err(invalid("assertions must be signed with ES256".into()));
        }
        let kid = header
            .kid
            .ok_or_else(|| invalid("the assertion names no key".into()))?;
        let key = self.key(&kid).await.map_err(invalid)?;

        let mut validation = Validation::new(Algorithm::ES256);
        // The audience is compared exactly below.
        validation.validate_aud = false;
        validation.set_issuer(&[IAP_ISSUER]);
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        validation.validate_nbf = true;
        validation.leeway = 5;
        let claims = decode::<Claims>(token, &key, &validation)
            .map_err(|_| invalid("it did not verify".into()))?
            .claims;
        if !audience_matches(audience, &claims.aud) {
            return Err(invalid("the assertion is for another audience".into()));
        }
        let email = claims
            .email
            .filter(|email| !email.is_empty())
            .ok_or_else(|| invalid("the assertion names no email".into()))?;
        let subject = claims
            .sub
            .filter(|sub| !sub.is_empty() && sub.len() <= 256 && !sub.contains(char::is_control))
            .ok_or_else(|| invalid("the assertion names no subject".into()))?;
        self.allow(subject, email)
    }

    fn allow(&self, subject: String, email: String) -> Result<Viewer, ViewerError> {
        match &self.domains {
            Some(domains) if !domains.allows(&email) => Err(ViewerError::Forbidden(email)),
            _ => Ok(Viewer { subject, email }),
        }
    }

    async fn key(&self, kid: &str) -> Result<DecodingKey, String> {
        if let Some(key) = self.keys.read().await.get(kid) {
            return Ok(key.clone());
        }
        let unknown = || "unknown signing key".to_string();
        let Some(url) = &self.jwks_url else {
            return Err(unknown());
        };
        // One fetch at a time, with the lock held across it. Callers that
        // queued behind a fetch look again once they hold the lock, so they
        // get the key it fetched instead of failing while it was in flight.
        let mut last = self.last_fetch.lock().await;
        if let Some(key) = self.keys.read().await.get(kid) {
            return Ok(key.clone());
        }
        if last.is_some_and(|at| at.elapsed() < REFETCH_INTERVAL) {
            return Err(unknown());
        }
        *last = Some(Instant::now());
        let set: JwkSet = self
            .client
            .get(url)
            .send()
            .await
            .and_then(|response| response.error_for_status())
            .map_err(|e| format!("cannot fetch signing keys: {e}"))?
            .json()
            .await
            .map_err(|e| format!("cannot read signing keys: {e}"))?;
        // The fetched set replaces the old one: the map stays as small as the
        // issuer's set, and a key the issuer withdrew stops verifying.
        let mut fetched = HashMap::new();
        for jwk in &set.keys {
            if let (Some(id), Ok(key)) = (&jwk.common.key_id, DecodingKey::from_jwk(jwk)) {
                fetched.insert(id.clone(), key);
            }
        }
        let mut keys = self.keys.write().await;
        *keys = fetched;
        keys.get(kid).cloned().ok_or_else(unknown)
    }
}

/// The assertion's audience is exactly the configured backend service. A
/// prefix match would accept an assertion IAP issued for another backend
/// service of the same project.
fn audience_matches(configured: &str, audience: &str) -> bool {
    match (IapAudience::parse(configured), IapAudience::parse(audience)) {
        (Some(want), Some(got)) => want.backend_service.is_some() && want == got,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::json;

    const AUDIENCE: &str = "/projects/123456/global/backendServices/987";

    /// A P-256 key pair made for this test process, so no private key is
    /// checked in: `(signing key as PKCS#8, public key as an uncompressed point)`.
    fn iap_key_pair() -> &'static (Vec<u8>, Vec<u8>) {
        use aws_lc_rs::rand::SystemRandom;
        use aws_lc_rs::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
        static PAIR: std::sync::OnceLock<(Vec<u8>, Vec<u8>)> = std::sync::OnceLock::new();
        PAIR.get_or_init(|| {
            let pkcs8 = EcdsaKeyPair::generate_pkcs8(
                &ECDSA_P256_SHA256_FIXED_SIGNING,
                &SystemRandom::new(),
            )
            .expect("a P-256 key is generated");
            let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref())
                .expect("the generated key loads");
            (pkcs8.as_ref().to_vec(), pair.public_key().as_ref().to_vec())
        })
    }

    fn config(extra: &[(&'static str, &'static str)]) -> Config {
        let extra = extra.to_vec();
        Config::from_env(move |name| match name {
            "ASTER_SRV_DATABASE_URL" => Some("postgres://localhost/x".into()),
            "ASTER_SRV_REPOSITORIES" => Some("ArchAstro/*".into()),
            other => extra
                .iter()
                .find(|(key, _)| *key == other)
                .map(|(_, value)| value.to_string()),
        })
        .unwrap()
    }

    fn verifier(extra: &[(&'static str, &'static str)]) -> IapVerifier {
        IapVerifier::new(&config(extra)).with_key("k1", DecodingKey::from_ec_der(&iap_key_pair().1))
    }

    fn token(claims: serde_json::Value, kid: &str) -> String {
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(kid.to_string());
        encode(
            &header,
            &claims,
            &EncodingKey::from_ec_der(&iap_key_pair().0),
        )
        .unwrap()
    }

    fn claims() -> serde_json::Value {
        json!({
            "iss": IAP_ISSUER,
            "aud": "/projects/123456/global/backendServices/987",
            "exp": chrono::Utc::now().timestamp() + 300,
            "sub": "accounts.google.com:1234",
            "email": "ann@example.com",
        })
    }

    #[tokio::test]
    async fn accepts_an_assertion_for_the_configured_audience() {
        let prefix = verifier(&[("ASTER_SRV_IAP_AUDIENCE", AUDIENCE)]);
        let assertion = token(claims(), "k1");
        let viewer = prefix.verify(Some(&assertion)).await.unwrap();
        assert_eq!(viewer.email, "ann@example.com");

        let exact = verifier(&[(
            "ASTER_SRV_IAP_AUDIENCE",
            "/projects/123456/global/backendServices/987",
        )]);
        assert!(exact.verify(Some(&assertion)).await.is_ok());
    }

    #[tokio::test]
    async fn refuses_other_audiences_issuers_algorithms_keys_and_expired_assertions() {
        let v = verifier(&[("ASTER_SRV_IAP_AUDIENCE", AUDIENCE)]);
        let check = |claims, kid| {
            let assertion = token(claims, kid);
            let v = &v;
            async move { v.verify(Some(&assertion)).await }
        };
        // Another backend service of the same project is another audience.
        let mut other_backend = claims();
        other_backend["aud"] = json!("/projects/123456/global/backendServices/988");
        assert!(check(other_backend, "k1").await.is_err());
        let mut other_project = claims();
        other_project["aud"] = json!("/projects/999/global/backendServices/987");
        assert!(matches!(
            check(other_project, "k1").await,
            Err(ViewerError::Unauthenticated(_))
        ));
        let mut bare_prefix = claims();
        bare_prefix["aud"] = json!("/projects/123456/global/backendServices/");
        assert!(check(bare_prefix, "k1").await.is_err());
        let mut wrong_issuer = claims();
        wrong_issuer["iss"] = json!("https://accounts.google.com");
        assert!(check(wrong_issuer, "k1").await.is_err());
        let mut expired = claims();
        expired["exp"] = json!(chrono::Utc::now().timestamp() - 3600);
        assert!(check(expired, "k1").await.is_err());
        let mut no_email = claims();
        no_email.as_object_mut().unwrap().remove("email");
        assert!(check(no_email, "k1").await.is_err());
        assert!(check(claims(), "other").await.is_err());

        let exact = verifier(&[(
            "ASTER_SRV_IAP_AUDIENCE",
            "/projects/123456/global/backendServices/1",
        )]);
        let assertion = token(claims(), "k1");
        assert!(exact.verify(Some(&assertion)).await.is_err());

        // An RS256 token, however well signed, is not an IAP assertion.
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("k1".into());
        let rsa = include_bytes!("../tests/fixtures/test-oidc-key.pem");
        let forged = encode(&header, &claims(), &EncodingKey::from_rsa_pem(rsa).unwrap()).unwrap();
        assert!(v.verify(Some(&forged)).await.is_err());
    }

    #[tokio::test]
    async fn needs_an_assertion() {
        let v = verifier(&[("ASTER_SRV_IAP_AUDIENCE", AUDIENCE)]);
        assert!(matches!(
            v.verify(None).await,
            Err(ViewerError::Unauthenticated(_))
        ));
        assert!(v.verify(Some("not-a-jwt")).await.is_err());
        assert!(v.verify(Some("  ")).await.is_err());
    }

    #[tokio::test]
    async fn the_viewer_carries_the_stable_subject_and_an_assertion_without_one_is_refused() {
        let v = verifier(&[("ASTER_SRV_IAP_AUDIENCE", AUDIENCE)]);
        let viewer = v.verify(Some(&token(claims(), "k1"))).await.unwrap();
        assert_eq!(viewer.subject, "accounts.google.com:1234");
        let mut nameless = claims();
        nameless.as_object_mut().unwrap().remove("sub");
        assert!(v.verify(Some(&token(nameless, "k1"))).await.is_err());
        let mut empty = claims();
        empty["sub"] = json!("");
        assert!(v.verify(Some(&token(empty, "k1"))).await.is_err());
        let dev = IapVerifier::new(&config(&[("ASTER_SRV_UI_DEV_EMAIL", "me@example.com")]));
        assert_eq!(
            dev.verify(None).await.unwrap().subject,
            "dev:me@example.com"
        );
    }

    #[tokio::test]
    async fn filters_viewers_by_email_domain() {
        let v = verifier(&[
            ("ASTER_SRV_IAP_AUDIENCE", AUDIENCE),
            ("ASTER_SRV_UI_EMAIL_DOMAINS", "example.com"),
        ]);
        assert!(v.verify(Some(&token(claims(), "k1"))).await.is_ok());
        let mut service = claims();
        service["email"] = json!("ci@proj.iam.gserviceaccount.com");
        assert_eq!(
            v.verify(Some(&token(service, "k1"))).await,
            Err(ViewerError::Forbidden(
                "ci@proj.iam.gserviceaccount.com".into()
            ))
        );
    }

    #[tokio::test]
    async fn modes_follow_the_configuration() {
        let off = IapVerifier::new(&config(&[]));
        assert!(!off.enabled());
        let dev = IapVerifier::new(&config(&[("ASTER_SRV_UI_DEV_EMAIL", "me@example.com")]));
        assert!(dev.enabled() && dev.is_dev());
        assert_eq!(dev.verify(None).await.unwrap().email, "me@example.com");
        // An IAP audience wins over the development viewer.
        let both = IapVerifier::new(&config(&[
            ("ASTER_SRV_UI_DEV_EMAIL", "me@example.com"),
            ("ASTER_SRV_IAP_AUDIENCE", AUDIENCE),
        ]));
        assert!(both.enabled() && !both.is_dev());
        assert!(both.verify(None).await.is_err());
    }

    #[tokio::test]
    async fn concurrent_assertions_for_an_unseen_key_share_one_slow_fetch() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64URL;
        use base64::Engine;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // The test key as a JWK: the uncompressed point is 0x04 || x || y.
        let point = &iap_key_pair().1;
        let body = json!({"keys": [{
            "kty": "EC", "crv": "P-256", "kid": "slow", "use": "sig", "alg": "ES256",
            "x": BASE64URL.encode(&point[1..33]), "y": BASE64URL.encode(&point[33..65]),
        }]})
        .to_string();
        let fetches = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/oauth2/v1/certs", listener.local_addr().unwrap());
        tokio::spawn({
            let fetches = Arc::clone(&fetches);
            async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    fetches.fetch_add(1, Ordering::SeqCst);
                    let body = body.clone();
                    tokio::spawn(async move {
                        let mut buffer = [0u8; 2048];
                        let _ = stream.read(&mut buffer).await;
                        // Slow enough that every caller below arrives mid-fetch.
                        tokio::time::sleep(Duration::from_millis(400)).await;
                        let _ = stream
                            .write_all(
                                format!(
                                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                                    body.len()
                                )
                                .as_bytes(),
                            )
                            .await;
                    });
                }
            }
        });

        let mut v = IapVerifier::new(&config(&[("ASTER_SRV_IAP_AUDIENCE", AUDIENCE)]));
        v.jwks_url = Some(url);
        v.keys
            .get_mut()
            .insert("withdrawn".into(), DecodingKey::from_secret(b"x"));
        let v = Arc::new(v);
        let assertion = token(claims(), "slow");
        let mut calls = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let (v, assertion) = (Arc::clone(&v), assertion.clone());
            calls.spawn(async move { v.verify(Some(&assertion)).await });
        }
        while let Some(result) = calls.join_next().await {
            let viewer = result.unwrap();
            assert!(
                viewer.is_ok(),
                "a caller that arrived during the fetch must get the fetched key: {viewer:?}"
            );
        }
        assert_eq!(
            fetches.load(Ordering::SeqCst),
            1,
            "one fetch serves them all"
        );

        // A key the issuer really does not have still fails, and does not refetch within a minute.
        assert!(v.verify(Some(&token(claims(), "nope"))).await.is_err());
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_audience_must_be_the_one_configured_backend_service() {
        let exact = "/projects/1/global/backendServices/2";
        assert!(audience_matches(exact, exact));
        for other in [
            "/projects/1/global/backendServices/3",
            "/projects/1/global/backendServices/23",
            "/projects/12/global/backendServices/2",
            "/projects/1/global/backendServices/2/x",
            "/projects/1/global/backendServices/",
        ] {
            assert!(!audience_matches(exact, other), "{other}");
        }
        // A project-wide prefix matches nothing, not even the backend
        // services of that project: another backend's assertion is refused.
        let prefix = "/projects/1/global/backendServices/";
        assert!(!audience_matches(prefix, exact));
        assert!(!audience_matches(prefix, prefix));
    }

    #[test]
    fn malformed_configured_audiences_match_nothing() {
        let aud = "/projects/1/global/backendServices/2";
        for configured in ["/", "", "/projects/", "/projects/1/", "/projects/1/global/"] {
            assert!(!audience_matches(configured, aud), "{configured:?}");
        }
    }
}
