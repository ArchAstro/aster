//! Reading from aster-srv: the same credentials and proxy header as an
//! upload, one attempt per request. Callers decide what a failed read costs;
//! for the build it costs nothing.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use super::upload::{agent, bearer, Credentials};
use crate::srv_url::checked_origin;

/// The most pages `failures_for_github_run` follows.
const MAX_PAGES: usize = 200;

/// A connection to one aster-srv, holding the bearer token.
pub struct Server {
    agent: ureq::Agent,
    base: String,
    token: String,
    proxy_token: Option<String>,
}

impl Server {
    /// Get the bearer token (for GitHub OIDC, from GitHub's token endpoint).
    pub fn connect(
        url: &str,
        credentials: &Credentials,
        proxy_token: Option<String>,
    ) -> Result<Self> {
        Self::connect_with_timeout(url, credentials, proxy_token, Duration::from_secs(20))
    }

    /// [`Server::connect`] whose requests give up after `timeout`.
    pub fn connect_with_timeout(
        url: &str,
        credentials: &Credentials,
        proxy_token: Option<String>,
        timeout: Duration,
    ) -> Result<Self> {
        let origin = checked_origin(url)?;
        let agent = agent(&origin, timeout);
        let token = bearer(&agent, credentials)?;
        Ok(Self {
            agent,
            base: url.trim_end_matches('/').to_string(),
            token,
            proxy_token,
        })
    }

    /// `GET path` (with its query) as JSON. `None` is a 404; any other
    /// refusal or a network error is an error.
    pub fn get_json(&self, path: &str) -> Result<Option<Value>> {
        let url = format!("{}{path}", self.base);
        let mut request = self
            .agent
            .get(&url)
            .header("Authorization", &format!("Bearer {}", self.token));
        if let Some(proxy) = &self.proxy_token {
            request = request.header("Proxy-Authorization", &format!("Bearer {proxy}"));
        }
        let mut response = request
            .call()
            .with_context(|| format!("cannot reach {}", self.base))?;
        let code = response.status().as_u16();
        if code == 404 {
            return Ok(None);
        }
        if !response.status().is_success() {
            let reason = response
                .body_mut()
                .read_json::<Value>()
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_string))
                .unwrap_or_default();
            bail!("the server refused the request ({code}): {reason}");
        }
        response
            .body_mut()
            .read_json()
            .map(Some)
            .context("cannot read the server's reply")
    }

    /// A server that sends a signed-in person's token (`aster srv login`)
    /// instead of a CI credential.
    pub fn with_token(
        url: &str,
        token: String,
        proxy_token: Option<String>,
        timeout: Duration,
    ) -> Result<Self> {
        let origin = checked_origin(url)?;
        Ok(Self {
            agent: agent(&origin, timeout),
            base: url.trim_end_matches('/').to_string(),
            token,
            proxy_token,
        })
    }

    /// `GET path` (with its query): the status and the JSON body, `Null`
    /// when the body is not JSON. Any status is an answer; only a network
    /// failure is an error.
    pub fn get_status(&self, path: &str) -> Result<(u16, Value)> {
        let url = format!("{}{path}", self.base);
        let mut request = self
            .agent
            .get(&url)
            .header("Authorization", &format!("Bearer {}", self.token));
        if let Some(proxy) = &self.proxy_token {
            request = request.header("Proxy-Authorization", &format!("Bearer {proxy}"));
        }
        let mut response = request
            .call()
            .with_context(|| format!("cannot reach {}", self.base))?;
        let code = response.status().as_u16();
        let body = response.body_mut().read_json().unwrap_or(Value::Null);
        Ok((code, body))
    }

    /// The server's record of one test, or `None` when it does not know it.
    pub fn test(&self, test_id: &str) -> Result<Option<Value>> {
        self.get_json(&format!(
            "/v1/test-results/tests/{}?limit=1",
            percent_encode(test_id)
        ))
    }

    /// The failures of every Aster run in a GitHub Actions run, all pages
    /// merged into the first page's shape (`next_cursor` null). `None` when
    /// the server has no runs for it.
    pub fn failures_for_github_run(
        &self,
        github_run: &str,
        attempt: Option<u32>,
    ) -> Result<Option<Value>> {
        let mut path = format!(
            "/v1/test-results/failures?github_run={}",
            percent_encode(github_run)
        );
        if let Some(attempt) = attempt {
            path.push_str(&format!("&attempt={attempt}"));
        }
        let Some(mut merged) = self.get_json(&path)? else {
            return Ok(None);
        };
        for _ in 1..MAX_PAGES {
            let Some(cursor) = merged["next_cursor"].as_str().map(str::to_string) else {
                return Ok(Some(merged));
            };
            let Some(page) =
                self.get_json(&format!("{path}&cursor={}", percent_encode(&cursor)))?
            else {
                bail!("the server lost the GitHub run between pages");
            };
            if let (Some(all), Some(more)) = (
                merged["failures"].as_array_mut(),
                page["failures"].as_array().cloned(),
            ) {
                all.extend(more);
            }
            merged["next_cursor"] = page["next_cursor"].clone();
        }
        if merged["next_cursor"].is_null() {
            return Ok(Some(merged));
        }
        bail!("more than {MAX_PAGES} pages of failures");
    }
}

/// Percent-encode everything but unreserved characters.
pub fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod testing {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// A local HTTP server: each request gets the reply of the first route
    /// whose prefix its target starts with (404 when none does). Returns the
    /// URL and the request lines seen so far, with the headers lowercased.
    /// The server thread lives until the process ends.
    pub fn serve(routes: Vec<(&'static str, u16, String)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                let target = head
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();
                log.lock().unwrap().push(head.to_ascii_lowercase());
                let (status, body) = routes
                    .iter()
                    .find(|(prefix, _, _)| target.starts_with(prefix))
                    .map(|(_, status, body)| (*status, body.as_str()))
                    .unwrap_or((404, r#"{"error":"no such route"}"#));
                let mut stream = stream;
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (url, seen)
    }

    /// A URL nothing listens on.
    pub fn closed_port() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::testing::serve;
    use super::*;

    fn connect(url: &str) -> Server {
        Server::connect(
            url,
            &Credentials::Static("secret".into()),
            Some("iap".into()),
        )
        .unwrap()
    }

    #[test]
    fn percent_encodes_all_but_unreserved() {
        assert_eq!(percent_encode("a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(percent_encode("gh:a/b c"), "gh%3Aa%2Fb%20c");
    }

    #[test]
    fn sends_both_tokens_and_maps_404_to_none() {
        let (url, seen) = serve(vec![(
            "/v1/test-results/tests/known",
            200,
            "{\"a\":1}".into(),
        )]);
        let server = connect(&url);
        assert_eq!(server.test("known").unwrap().unwrap()["a"], 1);
        assert!(server.test("missing").unwrap().is_none());
        let seen = seen.lock().unwrap();
        assert!(seen[0].starts_with("get /v1/test-results/tests/known?limit=1 "));
        assert!(seen[0].contains("authorization: bearer secret"));
        assert!(seen[0].contains("proxy-authorization: bearer iap"));
    }

    #[test]
    fn other_statuses_are_errors_with_the_reason() {
        let (url, _) = serve(vec![("/", 403, r#"{"error":"not your repo"}"#.into())]);
        let error = connect(&url).test("x").unwrap_err();
        assert!(
            error.to_string().contains("(403): not your repo"),
            "{error}"
        );
    }

    #[test]
    fn follows_cursors_and_merges_failures() {
        let page = |failures: &str, cursor: &str| {
            format!(
                r#"{{"github_run":"9","runs":[{{"run_id":"r"}}],"failures":{failures},"next_cursor":{cursor}}}"#
            )
        };
        let (url, seen) = serve(vec![
            (
                "/v1/test-results/failures?github_run=9&attempt=2&cursor=c2",
                200,
                page(r#"[{"test_id":"b"}]"#, "null"),
            ),
            (
                "/v1/test-results/failures?github_run=9&attempt=2&cursor=c1",
                200,
                page(r#"[{"test_id":"a2"}]"#, r#""c2""#),
            ),
            (
                "/v1/test-results/failures?github_run=9&attempt=2",
                200,
                page(r#"[{"test_id":"a"}]"#, r#""c1""#),
            ),
        ]);
        let merged = connect(&url)
            .failures_for_github_run("9", Some(2))
            .unwrap()
            .unwrap();
        let ids: Vec<&str> = merged["failures"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["test_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["a", "a2", "b"]);
        assert!(merged["next_cursor"].is_null());
        assert_eq!(seen.lock().unwrap().len(), 3);
    }

    #[test]
    fn an_unknown_github_run_is_none() {
        let (url, _) = serve(vec![]);
        assert!(connect(&url)
            .failures_for_github_run("1", None)
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_cleartext_server_on_another_host_is_refused_before_any_request() {
        let static_token = Credentials::Static("secret".into());
        for url in ["http://aster-srv.example.com", "http://10.0.0.5:8080/x"] {
            let error = Server::connect(url, &static_token, None).err().unwrap();
            assert!(format!("{error:#}").contains("must be https"), "{error:#}");
            let error = Server::with_token(url, "t".into(), None, Duration::from_secs(1))
                .err()
                .unwrap();
            assert!(format!("{error:#}").contains("must be https"), "{error:#}");
        }
        // https and this machine are fine; a static token needs no request.
        assert!(Server::connect("https://aster-srv.example.com/", &static_token, None).is_ok());
        assert!(Server::connect("http://127.0.0.1:1", &static_token, None).is_ok());
        assert!(
            Server::with_token("http://[::1]:1", "t".into(), None, Duration::from_secs(1)).is_ok()
        );
    }
}
