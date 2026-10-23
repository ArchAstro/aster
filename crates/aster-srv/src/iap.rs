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

        let header = decode_header(token).map_err(|e| invalid(e.to_string()))?;
        if header.alg != Algorithm::ES256 {
            return Err(invalid("assertions must be signed with ES256".into()));
        }
        let kid = header
            .kid
            .ok_or_else(|| invalid("the assertion names no key".into()))?;
        let key = self.key(&kid).await.map_err(invalid)?;

        let mut validation = Validation::new(Algorithm::ES256);
        // The audience may be a prefix, so it is checked below.
        validation.validate_aud = false;
        validation.set_issuer(&[IAP_ISSUER]);
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        let claims = decode::<Claims>(token, &key, &validation)
            .map_err(|e| invalid(e.to_string()))?
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
        {
            let mut last = self.last_fetch.lock().await;
            if last.is_some_and(|at| at.elapsed() < REFETCH_INTERVAL) {
                return Err(unknown());
            }
            *last = Some(Instant::now());
        }
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
        let mut keys = self.keys.write().await;
        for jwk in &set.keys {
            if let (Some(id), Ok(key)) = (&jwk.common.key_id, DecodingKey::from_jwk(jwk)) {
                keys.insert(id.clone(), key);
            }
        }
        keys.get(kid).cloned().ok_or_else(unknown)
    }
}

/// `configured` names one backend service, matched exactly, or (ending in
/// `/`) a project, matching any of its backend services. Both sides must
/// have the full IAP audience shape, whatever configuration validation let
/// through.
fn audience_matches(configured: &str, audience: &str) -> bool {
    let (Some(want), Some(got)) = (IapAudience::parse(configured), IapAudience::parse(audience))
    else {
        return false;
    };
    match want.backend_service {
        Some(_) => want == got,
        None => want.project == got.project && got.backend_service.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::json;

    const AUDIENCE: &str = "/projects/123456/global/backendServices/";

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
    async fn accepts_an_assertion_for_a_prefix_or_an_exact_audience() {
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
        let mut other_project = claims();
        other_project["aud"] = json!("/projects/999/global/backendServices/987");
        assert!(matches!(
            check(other_project, "k1").await,
            Err(ViewerError::Unauthenticated(_))
        ));
        let mut bare_prefix = claims();
        bare_prefix["aud"] = json!(AUDIENCE);
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

    #[test]
    fn audience_prefixes_need_a_trailing_slash() {
        let p = "/projects/1/global/backendServices/";
        assert!(audience_matches(p, "/projects/1/global/backendServices/2"));
        assert!(!audience_matches(p, p));
        assert!(!audience_matches(
            p,
            "/projects/12/global/backendServices/2"
        ));
        assert!(!audience_matches(
            p,
            "/projects/1/global/backendServices/2/x"
        ));
        let exact = "/projects/1/global/backendServices/2";
        assert!(audience_matches(exact, exact));
        assert!(!audience_matches(
            exact,
            "/projects/1/global/backendServices/23"
        ));
    }

    #[test]
    fn malformed_configured_audiences_match_nothing() {
        let aud = "/projects/1/global/backendServices/2";
        for configured in ["/", "", "/projects/", "/projects/1/", "/projects/1/global/"] {
            assert!(!audience_matches(configured, aud), "{configured:?}");
        }
    }
}
