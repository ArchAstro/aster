//! Person tokens: what `aster cache login` hands a laptop.
//!
//! A token is an HS256 JWT this server signs and verifies with its own keys
//! (`ASTER_SRV_CLI_TOKEN_KEYS`, each at least 256 bits). Claims, all required:
//!
//! | claim   | value |
//! |---------|-------|
//! | `iss`   | the server's public URL (`ASTER_SRV_PUBLIC_URL`) |
//! | `aud`   | the same URL: the token is for this server only |
//! | `sub`   | the stable IAP subject (`accounts.google.com:1234...`), which survives an email rename |
//! | `email` | the email IAP verified at sign-in |
//! | `scope` | `cache:read` |
//! | `iat`, `nbf`, `exp` | issue time, not-before, expiry; `exp - iat` is at most 12 hours |
//! | `jti`   | 128 random bits, unique per token |
//!
//! The header carries `kid`, which picks one of the configured keys: the
//! first signs, every one verifies, so rotation is "add a new first key,
//! drop the old one a TTL later". The algorithm is pinned to HS256 and a
//! token that names another (`none`, HS384, RS256 ...) is refused. Clock
//! skew leeway is 30 seconds. There is no refresh: when a token expires the
//! person signs in again.
//!
//! Tokens minted before these claims were required (no `sub`, `nbf` or
//! `email`) are refused; those people sign in once more. No release had
//! issued them to anyone but developers.
//!
//! Secret comparisons: JWT signatures are checked by `jsonwebtoken`, which
//! uses a constant-time HMAC verify; the confirmation-form MAC uses
//! `aws_lc_rs::hmac::verify`, also constant-time. Neither is compared with
//! `==`.
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

/// The one scope a person token carries.
pub const SCOPE: &str = "cache:read";
/// The longest a token lives, `exp - iat`, enforced at verify time as well as
/// at issue.
pub const MAX_TTL_SECONDS: u64 = 12 * 60 * 60;
/// Clock skew accepted on `exp`, `nbf` and `iat`.
pub const LEEWAY_SECONDS: u64 = 30;
const _: () = assert!(LEEWAY_SECONDS <= 60);
/// The longest token verified at all; real ones are about 450 bytes.
const MAX_TOKEN_BYTES: usize = 2048;

/// A person signed in with `aster cache login`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Person {
    /// The stable IAP subject.
    pub subject: String,
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
    email: String,
    scope: String,
    iat: i64,
    nbf: i64,
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
            ttl: config.cli_token_ttl.min(MAX_TTL_SECONDS),
        })
    }

    /// A token for the person IAP named `subject` and `email`, signed with
    /// the first key.
    pub fn issue(&self, subject: &str, email: &str) -> anyhow::Result<Issued> {
        let key = &self.keys[0];
        let now = Utc::now().timestamp();
        let expires_at = now + self.ttl as i64;
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some(key.kid.clone());
        let token = encode(
            &header,
            &Claims {
                iss: self.issuer.clone(),
                aud: self.issuer.clone(),
                sub: subject.into(),
                email: email.into(),
                scope: SCOPE.into(),
                iat: now,
                nbf: now,
                exp: expires_at,
                jti: random_token(16)?,
            },
            &EncodingKey::from_secret(&key.secret),
        )?;
        Ok(Issued { token, expires_at })
    }

    /// Check a token this server issued: HS256, a known key, this server as
    /// issuer and audience, every claim present, within its validity window
    /// (and a window of at most 12 hours), and the cache read scope.
    pub fn verify(&self, token: &str) -> Result<Person, AuthError> {
        let invalid = |reason: &str| AuthError::Invalid(reason.to_string());
        if token.len() > MAX_TOKEN_BYTES {
            return Err(invalid("the token is too long"));
        }
        let header = decode_header(token).map_err(|e| invalid(&e.to_string()))?;
        if header.alg != Algorithm::HS256 {
            return Err(invalid("person tokens are signed with HS256"));
        }
        if header.jku.is_some()
            || header.jwk.is_some()
            || header.x5u.is_some()
            || header.x5c.is_some()
        {
            return Err(invalid("the token header names a key location"));
        }
        let kid = header
            .kid
            .ok_or_else(|| invalid("the token names no key"))?;
        // The kid is the caller's text, so it is never echoed back.
        let key = self
            .keys
            .iter()
            .find(|key| key.kid == kid)
            .ok_or_else(|| invalid("unknown signing key"))?;
        let mut validation = Validation::new(Algorithm::HS256);
        validation.algorithms = vec![Algorithm::HS256];
        validation.set_audience(&[&self.issuer]);
        validation.set_issuer(&[&self.issuer]);
        validation.set_required_spec_claims(&["exp", "nbf", "aud", "iss", "sub"]);
        validation.validate_nbf = true;
        validation.leeway = LEEWAY_SECONDS;
        let claims = decode::<Claims>(token, &DecodingKey::from_secret(&key.secret), &validation)
            .map_err(|e| invalid(&e.to_string()))?
            .claims;
        let now = Utc::now().timestamp();
        let leeway = LEEWAY_SECONDS as i64;
        if !claims.scope.split_whitespace().any(|scope| scope == SCOPE) {
            return Err(invalid("the token lacks the cache:read scope"));
        }
        if claims.sub.is_empty() || claims.email.is_empty() {
            return Err(invalid("the token names no person"));
        }
        if claims.jti.is_empty() || claims.jti.len() > 64 {
            return Err(invalid("the token has no usable id"));
        }
        if claims.iat > now + leeway || claims.nbf < claims.iat - leeway || claims.exp < claims.nbf
        {
            return Err(invalid("the token's times are inconsistent"));
        }
        if claims.exp - claims.iat > MAX_TTL_SECONDS as i64 {
            return Err(invalid("the token lives longer than 12 hours"));
        }
        Ok(Person {
            subject: claims.sub,
            email: claims.email,
        })
    }

    /// A MAC over the confirmation form's fields, so only the page this
    /// server rendered for this person can be submitted for them.
    pub fn mac(&self, fields: &[&str]) -> String {
        let key = &self.keys[0];
        let key = hmac::Key::new(hmac::HMAC_SHA256, &key.secret);
        BASE64URL.encode(hmac::sign(&key, &mac_input(fields)).as_ref())
    }

    /// Whether `mac` is a MAC of `fields` under any configured key.
    pub fn check_mac(&self, fields: &[&str], mac: &str) -> bool {
        let Ok(tag) = BASE64URL.decode(mac) else {
            return false;
        };
        let input = mac_input(fields);
        self.keys.iter().any(|key| {
            let key = hmac::Key::new(hmac::HMAC_SHA256, &key.secret);
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
    use serde_json::{json, Value};

    const URL: &str = "https://aster-srv.example.com";
    const S1: &str = "q3Xv8mR2nLzK0wYb7Pj4cT1sUe9HdAfG5oIiMxVuNtE=";
    const S2: &str = "Zk3pQw9TnVb2Lx8RcJ5mYd7HsAe1UgF4oIiNtXuMvB0";
    const SUB: &str = "accounts.google.com:112233445566";

    fn issuer_for(keys: &str) -> TokenIssuer {
        let keys = keys.to_string();
        let config = Config::from_env(|name| match name {
            "ASTER_SRV_DATABASE_URL" => Some("postgres://localhost/x".into()),
            "ASTER_SRV_REPOSITORIES" => Some("ArchAstro/*".into()),
            "ASTER_SRV_PUBLIC_URL" => Some(URL.into()),
            "ASTER_SRV_IAP_AUDIENCE" => Some("/projects/1/global/backendServices/".into()),
            "ASTER_SRV_UI_EMAIL_DOMAINS" => Some("example.com".into()),
            "ASTER_SRV_CLI_TOKEN_KEYS" => Some(keys.clone()),
            _ => None,
        })
        .unwrap();
        TokenIssuer::from_config(&config).unwrap()
    }

    fn k1() -> String {
        format!("k1:{S1}")
    }
    fn k2() -> String {
        format!("k2:{S2}")
    }
    fn secret(text: &str) -> Vec<u8> {
        crate::config::decode_key_secret(text).unwrap()
    }

    /// A well-formed claim set, for a test to break one claim of.
    fn claims() -> Value {
        let now = Utc::now().timestamp();
        json!({
            "iss": URL, "aud": URL, "sub": SUB, "email": "ada@example.com",
            "scope": "cache:read", "iat": now, "nbf": now, "exp": now + 3600, "jti": "abc123",
        })
    }

    fn sign(alg: Algorithm, kid: Option<&str>, claims: &Value, key: &[u8]) -> String {
        let mut header = Header::new(alg);
        header.kid = kid.map(str::to_string);
        encode(&header, claims, &EncodingKey::from_secret(key)).unwrap()
    }

    fn hs256(claims: &Value) -> String {
        sign(Algorithm::HS256, Some("k1"), claims, &secret(S1))
    }

    fn ada() -> Person {
        Person {
            subject: SUB.into(),
            email: "ada@example.com".into(),
        }
    }

    fn refused(issuer: &TokenIssuer, token: &str) -> String {
        match issuer.verify(token) {
            Err(AuthError::Invalid(reason)) => reason,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_token_round_trips_with_every_claim_and_a_12_hour_life() {
        let issuer = issuer_for(&k1());
        let issued = issuer.issue(SUB, "ada@example.com").unwrap();
        assert_eq!(issuer.verify(&issued.token).unwrap(), ada());
        let lifetime = issued.expires_at - Utc::now().timestamp();
        assert!(
            (12 * 3600 - 10..=12 * 3600).contains(&lifetime),
            "{lifetime}"
        );
        // Decode the payload as an attacker would see it.
        let payload = issued.token.split('.').nth(1).unwrap();
        let payload: Value = serde_json::from_slice(&BASE64URL.decode(payload).unwrap()).unwrap();
        assert_eq!(payload["iss"], URL);
        assert_eq!(payload["aud"], URL, "the audience is the public URL");
        assert_eq!(
            payload["sub"], SUB,
            "sub is the stable subject, not the email"
        );
        assert_eq!(payload["email"], "ada@example.com");
        assert_eq!(payload["scope"], "cache:read");
        assert_eq!(payload["iat"], payload["nbf"]);
        assert!(payload["jti"].as_str().unwrap().len() >= 22);
        let header = decode_header(&issued.token).unwrap();
        assert_eq!(header.alg, Algorithm::HS256);
        assert_eq!(header.kid.as_deref(), Some("k1"));
        // Every token has its own jti.
        let again = issuer.issue(SUB, "ada@example.com").unwrap();
        assert_ne!(issued.token, again.token);
    }

    #[test]
    fn the_issuer_never_signs_for_longer_than_12_hours() {
        let mut issuer = issuer_for(&k1());
        issuer.ttl = 7 * 24 * 3600;
        // Even a hand-built issuer is clamped by `from_config`; a stray
        // longer token is refused at verify time regardless.
        let long = issuer.issue(SUB, "ada@example.com").unwrap();
        assert!(long.expires_at - Utc::now().timestamp() > 12 * 3600);
        assert!(refused(&issuer, &long.token).contains("12 hours"));
    }

    #[test]
    fn tokens_without_sub_nbf_email_iat_or_jti_are_refused() {
        let issuer = issuer_for(&k1());
        assert_eq!(issuer.verify(&hs256(&claims())).unwrap(), ada());
        for missing in [
            "sub", "nbf", "email", "iat", "jti", "scope", "exp", "iss", "aud",
        ] {
            let mut c = claims();
            c.as_object_mut().unwrap().remove(missing);
            assert!(issuer.verify(&hs256(&c)).is_err(), "missing {missing}");
        }
        // The shape #182 minted: sub was the email, no nbf, no email claim.
        let now = Utc::now().timestamp();
        let old = json!({"iss": URL, "aud": "aster-cli", "sub": "ada@example.com",
            "scope": "cache:read", "iat": now, "exp": now + 3600, "jti": "x"});
        assert!(issuer.verify(&hs256(&old)).is_err());
        for (name, value) in [("sub", json!("")), ("email", json!("")), ("jti", json!(""))] {
            let mut c = claims();
            c[name] = value;
            assert!(issuer.verify(&hs256(&c)).is_err(), "empty {name}");
        }
    }

    #[test]
    fn issuer_audience_and_scope_must_match() {
        let issuer = issuer_for(&k1());
        for (name, value) in [
            ("iss", json!("https://evil.example")),
            ("aud", json!("aster-cli")),
            ("aud", json!("https://other.example.com")),
            ("scope", json!("cache:write")),
        ] {
            let mut c = claims();
            c[name] = value.clone();
            assert!(issuer.verify(&hs256(&c)).is_err(), "{name}={value}");
        }
    }

    #[test]
    fn the_algorithm_is_pinned_to_hs256() {
        let issuer = issuer_for(&k1());
        let c = claims();
        for alg in [Algorithm::HS384, Algorithm::HS512] {
            let token = sign(alg, Some("k1"), &c, &secret(S1));
            assert!(refused(&issuer, &token).contains("HS256"), "{alg:?}");
        }
        // `alg: none`, with and without a signature segment.
        let b64 = |v: &Value| BASE64URL.encode(v.to_string());
        let header = b64(&json!({"alg": "none", "kid": "k1"}));
        assert!(issuer.verify(&format!("{header}.{}.", b64(&c))).is_err());
        assert!(issuer.verify(&format!("{header}.{}", b64(&c))).is_err());
        // A header that points at its own key is refused outright.
        let header = b64(&json!({"alg": "HS256", "kid": "k1", "jku": "https://evil.example/keys"}));
        let signed = hs256(&c);
        let forged = format!("{header}.{}", signed.split_once('.').unwrap().1);
        assert!(issuer.verify(&forged).is_err());
    }

    #[test]
    fn a_token_needs_a_known_kid_and_the_matching_secret() {
        let issuer = issuer_for(&k1());
        let c = claims();
        assert!(
            refused(&issuer, &sign(Algorithm::HS256, None, &c, &secret(S1))).contains("no key")
        );
        let unknown = sign(Algorithm::HS256, Some("zz"), &c, &secret(S1));
        let reason = refused(&issuer, &unknown);
        assert!(
            reason.contains("unknown signing key") && !reason.contains("zz"),
            "{reason}"
        );
        // Right kid, wrong secret.
        assert!(issuer
            .verify(&sign(Algorithm::HS256, Some("k1"), &c, &secret(S2)))
            .is_err());
        // The base64 text itself is not the key.
        assert!(issuer
            .verify(&sign(Algorithm::HS256, Some("k1"), &c, S1.as_bytes()))
            .is_err());
        // A flipped signature byte.
        let mut token = hs256(&c).into_bytes();
        let last = token.len() - 2;
        token[last] = if token[last] == b'A' { b'B' } else { b'A' };
        assert!(issuer.verify(&String::from_utf8(token).unwrap()).is_err());
        assert!(issuer.verify(&"a".repeat(MAX_TOKEN_BYTES + 1)).is_err());
    }

    #[test]
    fn times_are_enforced_at_verify_time_with_a_30_second_leeway() {
        let issuer = issuer_for(&k1());
        let now = Utc::now().timestamp();
        let with = |iat: i64, nbf: i64, exp: i64| {
            let mut c = claims();
            c["iat"] = json!(iat);
            c["nbf"] = json!(nbf);
            c["exp"] = json!(exp);
            hs256(&c)
        };
        assert!(issuer.verify(&with(now, now, now + 60)).is_ok());
        // Expired a few seconds ago: inside the leeway. Long ago: refused.
        assert!(issuer.verify(&with(now - 100, now - 100, now - 10)).is_ok());
        assert!(issuer
            .verify(&with(now - 100, now - 100, now - 40))
            .is_err());
        // Not yet valid: inside the leeway, and beyond it.
        assert!(issuer.verify(&with(now + 10, now + 10, now + 600)).is_ok());
        assert!(issuer
            .verify(&with(now + 120, now + 120, now + 600))
            .is_err());
        // Issued in the future with an earlier nbf.
        assert!(issuer.verify(&with(now + 120, now, now + 600)).is_err());
        // exp before nbf.
        assert!(issuer.verify(&with(now, now, now - 1)).is_err());
        // 12 hours exactly is allowed; a second more is not, even though
        // the signature is good and it has not expired.
        assert!(issuer.verify(&with(now, now, now + 12 * 3600)).is_ok());
        assert!(issuer.verify(&with(now, now, now + 12 * 3600 + 1)).is_err());
        assert!(issuer
            .verify(&with(now - 1, now - 1, now + 364 * 24 * 3600))
            .is_err());
    }

    #[test]
    fn rotation_accepts_the_current_and_previous_key_and_drops_the_old_one() {
        let old = issuer_for(&k1())
            .issue(SUB, "ada@example.com")
            .unwrap()
            .token;
        // Both listed, new first: k2 signs, k1 still verifies.
        let rotating = issuer_for(&format!("{},{}", k2(), k1()));
        assert_eq!(rotating.verify(&old).unwrap(), ada());
        let fresh = rotating.issue(SUB, "ada@example.com").unwrap().token;
        assert_eq!(decode_header(&fresh).unwrap().kid.as_deref(), Some("k2"));
        assert_eq!(rotating.verify(&fresh).unwrap(), ada());
        // A replica still on the old config accepts neither new tokens...
        assert!(issuer_for(&k1()).verify(&fresh).is_err());
        // ...and with the previous key dropped, its tokens stop working.
        let rotated = issuer_for(&k2());
        assert!(rotated.verify(&old).is_err());
        assert_eq!(rotated.verify(&fresh).unwrap(), ada());
    }

    #[test]
    fn a_mac_binds_every_field_and_survives_rotation() {
        let issuer = issuer_for(&k1());
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
        assert!(issuer_for(&format!("{},{}", k2(), k1())).check_mac(&fields, &mac));
        assert!(!issuer_for(&k2()).check_mac(&fields, &mac));
    }

    #[test]
    fn a_jwt_is_never_a_valid_mac_and_a_mac_never_a_jwt() {
        let issuer = issuer_for(&k1());
        let token = issuer.issue(SUB, "ada@example.com").unwrap().token;
        let signature = token.rsplit('.').next().unwrap();
        assert!(!issuer.check_mac(&["ada@example.com"], signature));
        let mac = issuer.mac(&["a"]);
        assert!(issuer.verify(&format!("{mac}.{mac}.{mac}")).is_err());
    }
}
