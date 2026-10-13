//! Who is calling: a GitHub Actions OIDC token, or a configured static
//! token in development.
//!
//! An OIDC token is accepted when it is signed by the issuer's current key,
//! is unexpired, names this server's audience, and comes from a repository
//! on the allowlist. Keys are fetched from the issuer's JWKS and refetched
//! when a token names a key not yet seen, at most once a minute.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use tokio::sync::{Mutex, RwLock};

use crate::config::{Config, RepositoryAllowlist, StaticToken};

/// The caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// `owner/name`.
    pub repository: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AuthError {
    /// No bearer token: 401.
    Missing,
    /// A token that is not valid: 401.
    Invalid(String),
    /// A valid token for a repository that may not use the server: 403.
    Forbidden(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::Missing => write!(f, "send a bearer token"),
            AuthError::Invalid(reason) => write!(f, "the token is not valid: {reason}"),
            AuthError::Forbidden(repository) => {
                write!(f, "{repository} may not use this server")
            }
        }
    }
}

#[derive(Deserialize)]
struct Claims {
    repository: String,
}

const REFETCH_INTERVAL: Duration = Duration::from_secs(60);

pub struct Verifier {
    audience: String,
    issuer: String,
    allowlist: RepositoryAllowlist,
    static_tokens: Vec<StaticToken>,
    /// Where keys come from; `None` uses only keys added with [`Self::with_key`].
    jwks_url: Option<String>,
    keys: RwLock<HashMap<String, DecodingKey>>,
    last_fetch: Mutex<Option<Instant>>,
    client: reqwest::Client,
}

impl Verifier {
    pub fn new(config: &Config) -> Self {
        Self {
            audience: config.audience.clone(),
            issuer: config.issuer.clone(),
            allowlist: config.repositories.clone(),
            static_tokens: config.static_tokens.clone(),
            jwks_url: Some(format!(
                "{}/.well-known/jwks",
                config.issuer.trim_end_matches('/')
            )),
            keys: RwLock::new(HashMap::new()),
            last_fetch: Mutex::new(None),
            client: reqwest::Client::builder()
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

    /// Check an `Authorization` header value.
    pub async fn verify(&self, authorization: Option<&str>) -> Result<Identity, AuthError> {
        let token = authorization
            .and_then(|value| {
                value
                    .strip_prefix("Bearer ")
                    .or_else(|| value.strip_prefix("bearer "))
            })
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .ok_or(AuthError::Missing)?;

        if let Some(found) = self
            .static_tokens
            .iter()
            .find(|candidate| constant_time_eq(candidate.token.as_bytes(), token.as_bytes()))
        {
            return Ok(Identity {
                repository: found.repository.clone(),
            });
        }

        let header = decode_header(token).map_err(|e| AuthError::Invalid(e.to_string()))?;
        if header.alg != Algorithm::RS256 {
            return Err(AuthError::Invalid(
                "tokens must be signed with RS256".into(),
            ));
        }
        let kid = header
            .kid
            .ok_or_else(|| AuthError::Invalid("the token names no key".into()))?;
        let key = self.key(&kid).await?;

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&[&self.audience]);
        validation.set_issuer(&[&self.issuer]);
        validation.set_required_spec_claims(&["exp", "aud", "iss"]);
        let claims = decode::<Claims>(token, &key, &validation)
            .map_err(|e| AuthError::Invalid(e.to_string()))?
            .claims;
        if !self.allowlist.allows(&claims.repository) {
            return Err(AuthError::Forbidden(claims.repository));
        }
        Ok(Identity {
            repository: claims.repository,
        })
    }

    async fn key(&self, kid: &str) -> Result<DecodingKey, AuthError> {
        if let Some(key) = self.keys.read().await.get(kid) {
            return Ok(key.clone());
        }
        let Some(url) = &self.jwks_url else {
            return Err(AuthError::Invalid(format!("unknown signing key {kid}")));
        };
        {
            let mut last = self.last_fetch.lock().await;
            if last.is_some_and(|at| at.elapsed() < REFETCH_INTERVAL) {
                return Err(AuthError::Invalid(format!("unknown signing key {kid}")));
            }
            *last = Some(Instant::now());
        }
        let set: JwkSet = self
            .client
            .get(url)
            .send()
            .await
            .and_then(|response| response.error_for_status())
            .map_err(|e| AuthError::Invalid(format!("cannot fetch signing keys: {e}")))?
            .json()
            .await
            .map_err(|e| AuthError::Invalid(format!("cannot read signing keys: {e}")))?;
        let mut keys = self.keys.write().await;
        for jwk in &set.keys {
            if let (Some(id), Ok(key)) = (&jwk.common.key_id, DecodingKey::from_jwk(jwk)) {
                keys.insert(id.clone(), key);
            }
        }
        keys.get(kid)
            .cloned()
            .ok_or_else(|| AuthError::Invalid(format!("unknown signing key {kid}")))
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::json;

    const PRIVATE: &[u8] = include_bytes!("../tests/fixtures/test-oidc-key.pem");
    const PUBLIC: &[u8] = include_bytes!("../tests/fixtures/test-oidc-key.pub.pem");

    fn config() -> Config {
        Config::from_env(|name| match name {
            "ASTER_SRV_DATABASE_URL" => Some("postgres://localhost/x".into()),
            "ASTER_SRV_REPOSITORIES" => Some("ArchAstro/*".into()),
            "ASTER_SRV_STATIC_TOKENS" => Some("ArchAstro/dev=dev-token-0123456789".into()),
            _ => None,
        })
        .unwrap()
    }

    fn verifier() -> Verifier {
        Verifier::new(&config()).with_key("k1", DecodingKey::from_rsa_pem(PUBLIC).unwrap())
    }

    fn token(claims: serde_json::Value, kid: &str) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.to_string());
        encode(
            &header,
            &claims,
            &EncodingKey::from_rsa_pem(PRIVATE).unwrap(),
        )
        .unwrap()
    }

    fn claims(repository: &str) -> serde_json::Value {
        json!({
            "iss": crate::config::GITHUB_ISSUER,
            "aud": "aster-srv",
            "exp": chrono::Utc::now().timestamp() + 300,
            "repository": repository,
        })
    }

    #[tokio::test]
    async fn accepts_a_github_token_for_an_allowed_repository() {
        let bearer = format!("Bearer {}", token(claims("ArchAstro/firstlanding"), "k1"));
        let identity = verifier().verify(Some(&bearer)).await.unwrap();
        assert_eq!(identity.repository, "ArchAstro/firstlanding");
    }

    #[tokio::test]
    async fn refuses_other_repositories_audiences_issuers_and_expired_tokens() {
        let v = verifier();
        let check = |claims| format!("Bearer {}", token(claims, "k1"));
        assert_eq!(
            v.verify(Some(&check(claims("evil/repo")))).await,
            Err(AuthError::Forbidden("evil/repo".into()))
        );
        let mut wrong_audience = claims("ArchAstro/x");
        wrong_audience["aud"] = json!("someone-else");
        assert!(matches!(
            v.verify(Some(&check(wrong_audience))).await,
            Err(AuthError::Invalid(_))
        ));
        let mut wrong_issuer = claims("ArchAstro/x");
        wrong_issuer["iss"] = json!("https://evil.example");
        assert!(matches!(
            v.verify(Some(&check(wrong_issuer))).await,
            Err(AuthError::Invalid(_))
        ));
        let mut expired = claims("ArchAstro/x");
        expired["exp"] = json!(chrono::Utc::now().timestamp() - 3600);
        assert!(matches!(
            v.verify(Some(&check(expired))).await,
            Err(AuthError::Invalid(_))
        ));
        let unknown_key = format!("Bearer {}", token(claims("ArchAstro/x"), "other"));
        assert!(matches!(
            v.verify(Some(&unknown_key)).await,
            Err(AuthError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn needs_a_bearer_token() {
        let v = verifier();
        assert_eq!(v.verify(None).await, Err(AuthError::Missing));
        assert_eq!(v.verify(Some("Basic abc")).await, Err(AuthError::Missing));
        assert!(matches!(
            v.verify(Some("Bearer not-a-jwt")).await,
            Err(AuthError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn static_tokens_stand_in_for_their_repository() {
        let identity = verifier()
            .verify(Some("Bearer dev-token-0123456789"))
            .await
            .unwrap();
        assert_eq!(identity.repository, "ArchAstro/dev");
    }
}
