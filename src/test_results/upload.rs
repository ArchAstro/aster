//! Sending a results file to aster-srv.
//!
//! In GitHub Actions the credential is an OIDC token for the `aster-srv`
//! audience (the job needs `permissions: id-token: write`). Elsewhere
//! `ASTER_SRV_TOKEN` supplies a static token. Uploading is best effort for
//! the build: callers report a failure and carry on.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// What the server said it stored.
#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct Receipt {
    pub run_id: String,
    pub duplicate: bool,
    pub targets: u64,
    pub tests: u64,
}

/// How to authenticate.
#[derive(Debug, PartialEq, Eq)]
pub enum Credentials {
    /// `ASTER_SRV_TOKEN`.
    Static(String),
    /// GitHub Actions' token request endpoint and its request token.
    GitHubOidc {
        request_url: String,
        request_token: String,
        audience: String,
    },
}

impl Credentials {
    /// From `ASTER_SRV_TOKEN`, else the GitHub Actions OIDC variables.
    pub fn from_env(env: &dyn Fn(&str) -> Option<String>) -> Option<Self> {
        let var = |name: &str| env(name).filter(|v| !v.is_empty());
        if let Some(token) = var("ASTER_SRV_TOKEN") {
            return Some(Credentials::Static(token));
        }
        Some(Credentials::GitHubOidc {
            request_url: var("ACTIONS_ID_TOKEN_REQUEST_URL")?,
            request_token: var("ACTIONS_ID_TOKEN_REQUEST_TOKEN")?,
            audience: var("ASTER_SRV_AUDIENCE").unwrap_or_else(|| "aster-srv".to_string()),
        })
    }
}

fn agent() -> ureq::Agent {
    // ureq is built without its own crypto provider; use the aws-lc-rs one
    // the rest of Aster uses. Installing twice is harmless.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_global(Some(Duration::from_secs(60)))
        .user_agent(concat!("aster/", env!("CARGO_PKG_VERSION")))
        .http_status_as_error(false)
        .build()
        .into()
}

fn bearer(agent: &ureq::Agent, credentials: &Credentials) -> Result<String> {
    match credentials {
        Credentials::Static(token) => Ok(token.clone()),
        Credentials::GitHubOidc {
            request_url,
            request_token,
            audience,
        } => {
            #[derive(Deserialize)]
            struct Token {
                value: String,
            }
            let separator = if request_url.contains('?') { '&' } else { '?' };
            let mut response = agent
                .get(&format!("{request_url}{separator}audience={audience}"))
                .header("Authorization", &format!("Bearer {request_token}"))
                .call()
                .context("cannot get an OIDC token from GitHub Actions")?;
            if !response.status().is_success() {
                bail!(
                    "GitHub Actions refused an OIDC token ({}); does the job have \
                     `permissions: id-token: write`?",
                    response.status().as_u16()
                );
            }
            let token: Token = response
                .body_mut()
                .read_json()
                .context("cannot read GitHub Actions' OIDC token")?;
            Ok(token.value)
        }
    }
}

/// Upload the NDJSON file at `path` to the server at `server_url`.
///
/// A network error, 429 or 5xx is retried twice, after 1 and 3 seconds.
/// Any other refusal is returned with the server's reason.
pub fn upload(server_url: &str, path: &Path, credentials: &Credentials) -> Result<Receipt> {
    let ndjson = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&ndjson)?;
    let body = encoder.finish()?;

    let agent = agent();
    let token = bearer(&agent, credentials)?;
    let url = format!("{}/v1/test-results/runs", server_url.trim_end_matches('/'));
    let mut last_error = None;
    for (attempt, wait) in [0u64, 1, 3].into_iter().enumerate() {
        if wait > 0 {
            std::thread::sleep(Duration::from_secs(wait));
        }
        let response = agent
            .post(&url)
            .header("Authorization", &format!("Bearer {token}"))
            .header("Content-Type", "application/x-ndjson")
            .header("Content-Encoding", "gzip")
            .send(&body[..]);
        match response {
            Ok(mut response) if response.status().is_success() => {
                return response
                    .body_mut()
                    .read_json()
                    .context("cannot read the server's reply");
            }
            Ok(mut response) => {
                let code = response.status().as_u16();
                let reason = response
                    .body_mut()
                    .read_json::<serde_json::Value>()
                    .ok()
                    .and_then(|v| v["error"].as_str().map(str::to_string))
                    .unwrap_or_default();
                if code != 429 && code < 500 {
                    bail!("the server refused the results ({code}): {reason}");
                }
                last_error = Some(anyhow::anyhow!(
                    "the server could not take the results ({code}): {reason}"
                ));
            }
            Err(error) => {
                last_error = Some(
                    anyhow::Error::new(error)
                        .context(format!("cannot reach {url} (attempt {})", attempt + 1)),
                );
            }
        }
    }
    Err(last_error.expect("at least one attempt"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Read};
    use std::net::TcpListener;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn credentials_prefer_a_static_token_then_github_oidc() {
        assert_eq!(Credentials::from_env(&env_of(&[])), None);
        assert_eq!(
            Credentials::from_env(&env_of(&[
                ("ASTER_SRV_TOKEN", "t"),
                ("ACTIONS_ID_TOKEN_REQUEST_URL", "u")
            ])),
            Some(Credentials::Static("t".into()))
        );
        assert_eq!(
            Credentials::from_env(&env_of(&[
                (
                    "ACTIONS_ID_TOKEN_REQUEST_URL",
                    "https://gh/token?api-version=2.0"
                ),
                ("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "rt"),
            ])),
            Some(Credentials::GitHubOidc {
                request_url: "https://gh/token?api-version=2.0".into(),
                request_token: "rt".into(),
                audience: "aster-srv".into(),
            })
        );
    }

    /// A one-shot HTTP server that answers each request with the next
    /// reply and records what it received.
    fn serve(replies: Vec<(u16, &'static str)>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for (status, body) in replies {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                    head.push_str(&line);
                }
                let mut body_in = vec![0; length];
                reader.read_exact(&mut body_in).unwrap();
                seen.push(head);
                let mut stream = stream;
                write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
            seen
        });
        (url, handle)
    }

    fn results_file() -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"type":"run","run_id":"r"}}"#).unwrap();
        file
    }

    #[test]
    fn uploads_gzip_with_the_bearer_token() {
        let (url, server) = serve(vec![(
            201,
            r#"{"run_id":"r","duplicate":false,"targets":1,"tests":2}"#,
        )]);
        let receipt = upload(
            &url,
            results_file().path(),
            &Credentials::Static("secret".into()),
        )
        .unwrap();
        assert_eq!(
            receipt,
            Receipt {
                run_id: "r".into(),
                duplicate: false,
                targets: 1,
                tests: 2
            }
        );
        let head = server.join().unwrap().remove(0).to_ascii_lowercase();
        assert!(head.starts_with("post /v1/test-results/runs"), "{head}");
        assert!(head.contains("authorization: bearer secret"));
        assert!(head.contains("content-encoding: gzip"));
    }

    #[test]
    fn retries_a_server_error_then_succeeds() {
        let (url, server) = serve(vec![
            (503, r#"{"error":"busy"}"#),
            (
                200,
                r#"{"run_id":"r","duplicate":true,"targets":0,"tests":0}"#,
            ),
        ]);
        let receipt = upload(
            &url,
            results_file().path(),
            &Credentials::Static("t".into()),
        )
        .unwrap();
        assert!(receipt.duplicate);
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[test]
    fn does_not_retry_a_refusal() {
        let (url, server) = serve(vec![(
            403,
            r#"{"error":"evil/repo may not use this server"}"#,
        )]);
        let error = upload(
            &url,
            results_file().path(),
            &Credentials::Static("t".into()),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("(403): evil/repo may not use this server"),
            "{error}"
        );
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    fn gets_a_github_oidc_token_for_the_audience() {
        let (url, server) = serve(vec![
            (200, r#"{"value":"oidc-jwt"}"#),
            (
                201,
                r#"{"run_id":"r","duplicate":false,"targets":0,"tests":0}"#,
            ),
        ]);
        let credentials = Credentials::GitHubOidc {
            request_url: format!("{url}/token?api-version=2.0"),
            request_token: "request".into(),
            audience: "aster-srv".into(),
        };
        upload(&url, results_file().path(), &credentials).unwrap();
        let seen = server.join().unwrap();
        let token_request = seen[0].to_ascii_lowercase();
        assert!(
            token_request.starts_with("get /token?api-version=2.0&audience=aster-srv"),
            "{token_request}"
        );
        assert!(token_request.contains("authorization: bearer request"));
        assert!(seen[1]
            .to_ascii_lowercase()
            .contains("authorization: bearer oidc-jwt"));
    }
}
