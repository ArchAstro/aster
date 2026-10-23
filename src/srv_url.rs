//! Which `ASTER_SRV_URL` values Aster will talk to, and the HTTP policy for
//! them.
//!
//! Every client of aster-srv (the person sign-in, the cache backend, test
//! result uploads and reads, quarantine, balance, summary) goes through
//! [`checked_origin`] before it builds an agent or sends a request. A bearer
//! token, an OIDC token or a sign-in code over plain http to another host
//! could be read by anyone on the path, so that is refused.

use anyhow::{bail, Context, Result};

/// `scheme://authority` of `url`, with the path dropped and the host
/// lowercased. `None` unless the scheme is http or https.
pub(crate) fn server_origin(url: &str) -> Option<String> {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?.to_ascii_lowercase();
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    Some(format!("{scheme}://{authority}"))
}

/// Whether `origin` (as [`server_origin`] returns it) may carry credentials:
/// https, or http to this machine.
pub(crate) fn is_secure_origin(origin: &str) -> bool {
    let Some((scheme, _)) = origin.split_once("://") else {
        return false;
    };
    scheme == "https" || is_loopback_origin(origin)
}

/// Whether `origin` names this machine (`127.0.0.1`, `::1`, `localhost`).
fn is_loopback_origin(origin: &str) -> bool {
    let Some((_, authority)) = origin.split_once("://") else {
        return false;
    };
    let host = match authority.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => authority.split(':').next().unwrap_or(""),
    };
    matches!(host, "127.0.0.1" | "::1" | "localhost")
}

/// A normal origin for `server_url`, or why it cannot be used. Nothing has
/// been sent when this fails.
pub(crate) fn checked_origin(server_url: &str) -> Result<String> {
    let origin = server_origin(server_url)
        .with_context(|| format!("ASTER_SRV_URL must be an http(s) URL, not '{server_url}'"))?;
    if !is_secure_origin(&origin) {
        bail!(
            "ASTER_SRV_URL must be https (plain http is only for this machine); \
             '{origin}' would send credentials in the clear"
        );
    }
    Ok(origin)
}

/// How an agent that talks to `origin` (already through [`checked_origin`])
/// treats the scheme: `(https_only, max_redirects)`.
///
/// An https origin only follows https redirects, so a redirect cannot
/// downgrade. A loopback http origin follows no redirects at all, so it
/// cannot be sent to another host.
pub(crate) fn scheme_policy(origin: &str) -> (bool, u32) {
    if origin.starts_with("https://") {
        (true, 10)
    } else {
        (!is_loopback_origin(origin), 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_are_normalised_and_odd_urls_refused() {
        assert_eq!(
            server_origin("https://Aster-Srv.Example.com/some/path?x=1").as_deref(),
            Some("https://aster-srv.example.com")
        );
        assert_eq!(
            server_origin("http://127.0.0.1:8080").as_deref(),
            Some("http://127.0.0.1:8080")
        );
        for bad in [
            "",
            "ftp://x",
            "example.com",
            "https://",
            "https://user@example.com",
            "http://127.0.0.1@evil.com",
        ] {
            assert_eq!(server_origin(bad), None, "{bad}");
        }
    }

    #[test]
    fn only_https_or_this_machine_is_secure() {
        for ok in [
            "https://aster-srv.example.com",
            "http://127.0.0.1:8080",
            "http://localhost:3000",
            "http://[::1]:3000",
        ] {
            assert!(is_secure_origin(ok), "{ok}");
        }
        for bad in [
            "http://aster-srv.example.com",
            "http://10.0.0.5:8080",
            "http://127.0.0.1.evil.com",
            "http://localhost.evil.com",
        ] {
            assert!(!is_secure_origin(bad), "{bad}");
        }
    }

    #[test]
    fn the_scheme_policy_blocks_downgrades() {
        assert_eq!(scheme_policy("https://aster-srv.example.com"), (true, 10));
        assert_eq!(scheme_policy("http://127.0.0.1:8080"), (false, 0));
        assert_eq!(scheme_policy("http://[::1]:8080"), (false, 0));
        // An origin that was never checked still cannot downgrade.
        assert_eq!(scheme_policy("http://example.com"), (true, 0));
    }

    #[test]
    fn checked_origin_refuses_cleartext_to_another_host() {
        assert!(checked_origin("https://aster-srv.example.com/").is_ok());
        assert!(checked_origin("http://127.0.0.1:1").is_ok());
        let error = checked_origin("http://aster-srv.example.com").unwrap_err();
        assert!(format!("{error:#}").contains("must be https"), "{error:#}");
        assert!(checked_origin("ftp://x").is_err());
    }
}
