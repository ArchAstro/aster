//! Who is calling: a GitHub Actions OIDC token, or a configured static
//! token in development.
//!
//! An OIDC token is accepted when it is signed by the issuer's current key,
//! is unexpired, names this server's audience, and comes from a repository
//! on the allowlist. Keys are fetched from the issuer's JWKS and refetched
//! when a token names a key not yet seen, at most once a minute; callers
//! that arrive during a fetch wait for it and share its keys.

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
    /// GitHub Actions `ref` claim (`refs/heads/main`, `refs/pull/7/merge`):
    /// the git ref, not the workflow file. Cache writer patterns match it.
    /// `None` for a static token, which may write the cache.
    pub git_ref: Option<String>,
    /// The `run_id` claim. `None` for a static token.
    pub run_id: Option<String>,
    /// The `sha` claim. `None` for a static token.
    pub sha: Option<String>,
    /// The `workflow_ref` claim (`owner/repo/.github/workflows/ci.yml@refs/heads/main`):
    /// the workflow file. `None` for a static token.
    pub workflow: Option<String>,
    /// A configured static token, not an OIDC token.
    pub static_token: bool,
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
    #[serde(default)]
    #[serde(rename = "ref")]
    git_ref: Option<String>,
    #[serde(default, deserialize_with = "string_or_number")]
    run_id: Option<String>,
    #[serde(default)]
    sha: Option<String>,
    #[serde(default)]
    #[serde(rename = "workflow_ref")]
    workflow: Option<String>,
}

/// GitHub sends `run_id` as a string; accept a number too rather than
/// refuse the token over a claim that only labels cache writes.
fn string_or_number<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Ok(
        match Option::<serde_json::Value>::deserialize(deserializer)? {
            Some(serde_json::Value::String(text)) => Some(text),
            Some(serde_json::Value::Number(number)) => Some(number.to_string()),
            _ => None,
        },
    )
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
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
    /// When keys were last fetched. Held for the whole fetch, so a caller
    /// that arrives mid-fetch waits and then finds the fetched keys.
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
                git_ref: None,
                run_id: None,
                sha: None,
                workflow: None,
                static_token: true,
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
            git_ref: non_empty(claims.git_ref),
            run_id: non_empty(claims.run_id),
            sha: non_empty(claims.sha),
            workflow: non_empty(claims.workflow),
            static_token: false,
        })
    }

    async fn key(&self, kid: &str) -> Result<DecodingKey, AuthError> {
        if let Some(key) = self.keys.read().await.get(kid) {
            return Ok(key.clone());
        }
        let Some(url) = &self.jwks_url else {
            return Err(AuthError::Invalid(format!("unknown signing key {kid}")));
        };
        // One fetch at a time, with the lock held across it. Callers that
        // queued behind a fetch look again once they hold the lock, so they
        // get the key it fetched instead of failing while it was in flight.
        let mut last = self.last_fetch.lock().await;
        if let Some(key) = self.keys.read().await.get(kid) {
            return Ok(key.clone());
        }
        if last.is_some_and(|at| at.elapsed() < REFETCH_INTERVAL) {
            return Err(AuthError::Invalid(format!("unknown signing key {kid}")));
        }
        *last = Some(Instant::now());
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

    #[tokio::test]
    async fn identity_carries_the_ref_and_the_writer_claims_separately() {
        let mut full = claims("ArchAstro/firstlanding");
        full["ref"] = json!("refs/heads/main");
        full["run_id"] = json!("12345");
        full["sha"] = json!("abc123");
        full["workflow_ref"] =
            json!("ArchAstro/firstlanding/.github/workflows/ci.yml@refs/heads/main");
        let bearer = format!("Bearer {}", token(full.clone(), "k1"));
        let identity = verifier().verify(Some(&bearer)).await.unwrap();
        assert_eq!(identity.git_ref.as_deref(), Some("refs/heads/main"));
        assert_eq!(identity.run_id.as_deref(), Some("12345"));
        assert_eq!(identity.sha.as_deref(), Some("abc123"));
        assert_eq!(
            identity.workflow.as_deref(),
            Some("ArchAstro/firstlanding/.github/workflows/ci.yml@refs/heads/main")
        );

        // A numeric run_id is kept; empty claims become None.
        full["run_id"] = json!(678);
        full["sha"] = json!("");
        let bearer = format!("Bearer {}", token(full, "k1"));
        let identity = verifier().verify(Some(&bearer)).await.unwrap();
        assert_eq!(identity.run_id.as_deref(), Some("678"));
        assert_eq!(identity.sha, None);

        // Static tokens carry none of them.
        let identity = verifier()
            .verify(Some("Bearer dev-token-0123456789"))
            .await
            .unwrap();
        assert_eq!(
            (
                identity.git_ref,
                identity.run_id,
                identity.sha,
                identity.workflow
            ),
            (None, None, None, None)
        );
    }

    /// The test OIDC key as a JWK, served by a stub that answers slowly.
    const JWKS_N: &str = "855qXNm0Qs-azcPsy_QGxBFH4BPpxL3czNPvP4XV1XRv3tCIbacibo4Vbkvop-Z-4G70ZP-KNgMx_ItXqNpWN1erdKh_JWl0OVPMZpeYJH5T1jV2beTiflxah3pAAvv2lPC0m5QZR9dc6mE2XKcwyPPdhVq1CBeHmksnC8gQjM8iiQkWDuEU4FKbPxzQ6bQ0sau-STvZfy9eP8wQk1kxXE6acJaJaLd45Qqy3rTHVTOjCt3P_gjAbxY_bHY_zFZyn3hBrpuxgJN0fkIsAWHjWPKQzcGNnfbr4UI0R0YKiC69GkbWBUy1Jv-6d5o9cecUSVACA9SdH3A0TobNMwWXmQ";

    #[tokio::test]
    async fn concurrent_requests_for_an_unseen_key_share_one_slow_fetch() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let fetches = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/.well-known/jwks", listener.local_addr().unwrap());
        let body = json!({"keys": [{
            "kty": "RSA", "kid": "slow", "use": "sig", "alg": "RS256",
            "n": JWKS_N, "e": "AQAB",
        }]})
        .to_string();
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

        let mut verifier = Verifier::new(&config());
        verifier.jwks_url = Some(url);
        let verifier = Arc::new(verifier);
        let bearer = format!("Bearer {}", token(claims("ArchAstro/firstlanding"), "slow"));
        let mut calls = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let (verifier, bearer) = (Arc::clone(&verifier), bearer.clone());
            calls.spawn(async move { verifier.verify(Some(&bearer)).await });
        }
        while let Some(result) = calls.join_next().await {
            let identity = result.unwrap();
            assert!(
                identity.is_ok(),
                "a caller that arrived during the fetch must get the fetched key: {identity:?}"
            );
        }
        assert_eq!(
            fetches.load(Ordering::SeqCst),
            1,
            "one fetch serves them all"
        );

        // A key the issuer really does not have still fails, and does not refetch within a minute.
        let unknown = format!("Bearer {}", token(claims("ArchAstro/firstlanding"), "nope"));
        assert!(matches!(
            verifier.verify(Some(&unknown)).await,
            Err(AuthError::Invalid(reason)) if reason.contains("unknown signing key")
        ));
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
    }
}
