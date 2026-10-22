//! Person tokens: what `aster cache login` hands a laptop.
//!
//! A token is an HS256 JWT this server signs and verifies with its own keys
//! (`ASTER_SRV_CLI_TOKEN_KEYS`). Its `sub` is the email IAP verified at
//! sign-in, its `aud` is `aster-cli`, and its `scope` is `cache:read`. There
//! is no refresh: when it expires the person signs in again.
//!
//! The same keys sign the confirmation form's MAC (see [`TokenIssuer::mac`]),
//! under a prefix no JWT signing input can start with.

use aws_lc_rs::hmac;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64URL;
use base64::Engine;
use chrono::Utc;
use jsonwebtoken::{decode, decode_header, encode, Algorithm, DecodingKey, EncodingKey};
use jsonwebtoken::{Header, Validation};
use serde::{Deserialize, Serialize};

use crate::auth::AuthError;
use crate::config::{CliTokenKey, Config};

/// The `aud` of a person token.
pub const AUDIENCE: &str = "aster-cli";
/// The one scope a person token carries.
pub const SCOPE: &str = "cache:read";

/// A person signed in with `aster cache login`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Person {
    pub email: String,
}

/// A token and when it stops working.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issued {
    pub token: String,
    /// Unix seconds.
    pub expires_at: i64,
}

#[derive(Serialize, Deserialize)]
struct Claims {
    iss: String,
    aud: String,
    sub: String,
    scope: String,
    iat: i64,
    exp: i64,
    jti: String,
}

/// Mints and checks person tokens. Built only when both the keys and the
/// public URL are configured, so a half-configured server issues nothing.
pub struct TokenIssuer {
    issuer: String,
    keys: Vec<CliTokenKey>,
    ttl: u64,
}

impl TokenIssuer {
    pub fn from_config(config: &Config) -> Option<Self> {
        let issuer = config.public_url.clone()?;
        if config.cli_token_keys.is_empty() {
            return None;
        }
        Some(Self {
            issuer,
            keys: config.cli_token_keys.clone(),
            ttl: config.cli_token_ttl,
        })
    }

    /// A token for `email`, signed with the first key.
    pub fn issue(&self, email: &str) -> anyhow::Result<Issued> {
        let key = &self.keys[0];
        let now = Utc::now().timestamp();
        let expires_at = now + self.ttl as i64;
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some(key.kid.clone());
        let token = encode(
            &header,
            &Claims {
                iss: self.issuer.clone(),
                aud: AUDIENCE.into(),
                sub: email.into(),
                scope: SCOPE.into(),
                iat: now,
                exp: expires_at,
                jti: random_token(16)?,
            },
            &EncodingKey::from_secret(key.secret.as_bytes()),
        )?;
        Ok(Issued { token, expires_at })
    }

    /// Check a token this server issued: a known key, HS256, this issuer,
    /// the person audience, unexpired, and the cache read scope.
    pub fn verify(&self, token: &str) -> Result<Person, AuthError> {
        let invalid = |reason: String| AuthError::Invalid(reason);
        let header = decode_header(token).map_err(|e| invalid(e.to_string()))?;
        if header.alg != Algorithm::HS256 {
            return Err(invalid("person tokens are signed with HS256".into()));
        }
        let kid = header
            .kid
            .ok_or_else(|| invalid("the token names no key".into()))?;
        let key = self
            .keys
            .iter()
            .find(|key| key.kid == kid)
            .ok_or_else(|| invalid(format!("unknown signing key {kid}")))?;
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_audience(&[AUDIENCE]);
        validation.set_issuer(&[&self.issuer]);
        validation.set_required_spec_claims(&["exp", "aud", "iss", "sub"]);
        validation.leeway = 5;
        let claims = decode::<Claims>(
            token,
            &DecodingKey::from_secret(key.secret.as_bytes()),
            &validation,
        )
        .map_err(|e| invalid(e.to_string()))?
        .claims;
        if !claims.scope.split_whitespace().any(|scope| scope == SCOPE) {
            return Err(invalid("the token lacks the cache:read scope".into()));
        }
        if claims.sub.is_empty() {
            return Err(invalid("the token names no person".into()));
        }
        Ok(Person { email: claims.sub })
    }

    /// A MAC over the confirmation form's fields, so only the page this
    /// server rendered for this person can be submitted for them.
    pub fn mac(&self, fields: &[&str]) -> String {
        let key = &self.keys[0];
        let key = hmac::Key::new(hmac::HMAC_SHA256, key.secret.as_bytes());
        BASE64URL.encode(hmac::sign(&key, &mac_input(fields)).as_ref())
    }

    /// Whether `mac` is a MAC of `fields` under any configured key.
    pub fn check_mac(&self, fields: &[&str], mac: &str) -> bool {
        let Ok(tag) = BASE64URL.decode(mac) else {
            return false;
        };
        let input = mac_input(fields);
        self.keys.iter().any(|key| {
            let key = hmac::Key::new(hmac::HMAC_SHA256, key.secret.as_bytes());
            hmac::verify(&key, &input, &tag).is_ok()
        })
    }
}

/// Fields joined by NUL under a fixed prefix. A JWT signing input starts
/// with a base64url header, never with this prefix, so one key signs both
/// without either being usable as the other.
fn mac_input(fields: &[&str]) -> Vec<u8> {
    let mut input = b"aster-cli-auth-v1".to_vec();
    for field in fields {
        input.push(0);
        input.extend_from_slice(field.as_bytes());
    }
    input
}

/// `bytes` random bytes as base64url.
pub fn random_token(bytes: usize) -> anyhow::Result<String> {
    let mut buffer = vec![0u8; bytes];
    aws_lc_rs::rand::fill(&mut buffer)
        .map_err(|_| anyhow::anyhow!("the system random generator failed"))?;
    Ok(BASE64URL.encode(buffer))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issuer_for(keys: &str) -> TokenIssuer {
        let keys = keys.to_string();
        let config = Config::from_env(|name| match name {
            "ASTER_SRV_DATABASE_URL" => Some("postgres://localhost/x".into()),
            "ASTER_SRV_REPOSITORIES" => Some("ArchAstro/*".into()),
            "ASTER_SRV_PUBLIC_URL" => Some("https://aster-srv.example.com".into()),
            "ASTER_SRV_CLI_TOKEN_KEYS" => Some(keys.clone()),
            _ => None,
        })
        .unwrap();
        TokenIssuer::from_config(&config).unwrap()
    }

    const K1: &str = "k1:0123456789abcdef0123456789abcdef";
    const K2: &str = "k2:fedcba9876543210fedcba9876543210";

    #[test]
    fn a_token_round_trips_to_its_person() {
        let issuer = issuer_for(K1);
        let issued = issuer.issue("ada@example.com").unwrap();
        assert_eq!(
            issuer.verify(&issued.token).unwrap(),
            Person {
                email: "ada@example.com".into()
            }
        );
        let lifetime = issued.expires_at - Utc::now().timestamp();
        assert!(
            (12 * 60 * 60 - 10..=12 * 60 * 60).contains(&lifetime),
            "{lifetime}"
        );
    }

    #[test]
    fn a_rotated_out_key_no_longer_verifies() {
        let old = issuer_for(K1).issue("ada@example.com").unwrap().token;
        // Both keys listed: signing moves to k2, k1 still verifies.
        let rotating = issuer_for(&format!("{K2},{K1}"));
        assert!(rotating.verify(&old).is_ok());
        let fresh = rotating.issue("ada@example.com").unwrap().token;
        assert_eq!(decode_header(&fresh).unwrap().kid.as_deref(), Some("k2"));
        // k1 dropped: its tokens are refused, k2's stay good.
        let rotated = issuer_for(K2);
        assert!(matches!(rotated.verify(&old), Err(AuthError::Invalid(_))));
        assert!(rotated.verify(&fresh).is_ok());
    }

    #[test]
    fn a_mac_binds_every_field_and_survives_rotation() {
        let issuer = issuer_for(K1);
        let fields = ["ada@example.com", "challenge", "4000", "state", "99"];
        let mac = issuer.mac(&fields);
        assert!(issuer.check_mac(&fields, &mac));
        for index in 0..fields.len() {
            let mut changed = fields;
            changed[index] = "other";
            assert!(!issuer.check_mac(&changed, &mac), "field {index}");
        }
        assert!(!issuer.check_mac(&fields, "not base64 !"));
        assert!(!issuer.check_mac(&fields, ""));
        // A key added in front keeps old MACs good until k1 is dropped.
        assert!(issuer_for(&format!("{K2},{K1}")).check_mac(&fields, &mac));
        assert!(!issuer_for(K2).check_mac(&fields, &mac));
    }
}
