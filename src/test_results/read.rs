//! `aster srv`: read the datasets aster-srv's dashboard shows, as JSON.
//!
//! Each dataset is one `GET /v1/...` route. The credential is the CI one
//! when the environment has it (`ASTER_SRV_TOKEN`, GitHub OIDC), else the
//! token `aster cache login` stored for this server. A person's token can
//! read every dataset of any repository the server serves; it names the
//! repository with `repo`, which defaults the way the cache client derives
//! it (`GITHUB_REPOSITORY`, else the checkout's `origin`).

use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, bail, Result};
use serde_json::Value;

use super::client::{percent_encode, Server};
use super::upload::{proxy_token_from_env, Credentials};
use crate::cache::{stored_token, SIGNED_OUT_MESSAGE};

const TIMEOUT: Duration = Duration::from_secs(20);

/// A dashboard dataset with its own subcommand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dataset {
    /// Cache size, lookups and last writer.
    CacheSummary,
    /// Tests in the flaky state, highest score first.
    Flaky,
    /// Tests that fail on every default-branch run since some commit.
    Broken,
    /// The tests Aster keeps from failing builds.
    Quarantine,
    /// The last week's failures grouped by failure hash.
    Clusters,
    /// One test's state and recent results.
    Test(String),
}

impl Dataset {
    pub fn path(&self) -> String {
        match self {
            Dataset::CacheSummary => "/v1/cache/summary".into(),
            Dataset::Flaky => "/v1/test-results/tests?state=flaky".into(),
            Dataset::Broken => "/v1/test-results/tests?state=broken".into(),
            Dataset::Quarantine => "/v1/test-results/quarantine".into(),
            Dataset::Clusters => "/v1/test-results/clusters".into(),
            Dataset::Test(id) => format!("/v1/test-results/tests/{}", percent_encode(id)),
        }
    }
}

/// `path` with `repo` added when it is a `/v1/` route that names none.
pub fn with_repo(path: &str, repo: Option<&str>) -> String {
    let Some(repo) = repo else {
        return path.to_string();
    };
    let (route, query) = path.split_once('?').unwrap_or((path, ""));
    let names_repo = query
        .split('&')
        .any(|pair| pair.split('=').next() == Some("repo"));
    if !route.starts_with("/v1/") || names_repo {
        return path.to_string();
    }
    let separator = if query.is_empty() { '?' } else { '&' };
    format!("{path}{separator}repo={}", percent_encode(repo))
}

/// The repository to read: `--repo`, else `GITHUB_REPOSITORY` or the
/// checkout's `origin`.
pub fn default_repo(
    flag: Option<&str>,
    workspace_root: &Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    flag.map(str::to_string)
        .or_else(|| Some(super::detect_run(workspace_root, "", None, env).repo))
        .filter(|repo| !repo.trim().is_empty())
}

/// `GET path` on `url` and return the body, or say why not.
pub fn get(
    url: &str,
    path: &str,
    env: &dyn Fn(&str) -> Option<String>,
    now: SystemTime,
) -> Result<Value> {
    let proxy_token = proxy_token_from_env(env);
    let (server, person) = match Credentials::from_env(env) {
        Some(credentials) => (Server::connect(url, &credentials, proxy_token)?, false),
        None => {
            let stored = stored_token(url, now).ok_or_else(|| anyhow!(SIGNED_OUT_MESSAGE))?;
            (
                Server::with_token(url, stored.token, proxy_token, TIMEOUT)?,
                true,
            )
        }
    };
    let (status, body) = server.get_status(path)?;
    if (200..300).contains(&status) {
        return Ok(body);
    }
    let reason = body["error"].as_str().unwrap_or("no reason given");
    match status {
        401 if person => {
            bail!("the server did not accept your sign-in ({reason}); run aster cache login")
        }
        401 => bail!("the server did not accept the credentials ({reason})"),
        403 => bail!("the server refused the request (403): {reason}"),
        404 => bail!("not found: {path}"),
        other => bail!("the server answered {other}: {reason}"),
    }
}

#[cfg(test)]
mod tests {
    use super::super::client::testing::serve;
    use super::*;

    #[test]
    fn datasets_map_to_their_routes() {
        assert_eq!(Dataset::Flaky.path(), "/v1/test-results/tests?state=flaky");
        assert_eq!(
            Dataset::Test("a/b c".into()).path(),
            "/v1/test-results/tests/a%2Fb%20c"
        );
    }

    #[test]
    fn repo_is_added_to_v1_routes_that_name_none() {
        let repo = Some("acme/app");
        assert_eq!(
            with_repo("/v1/test-results/quarantine", repo),
            "/v1/test-results/quarantine?repo=acme%2Fapp"
        );
        assert_eq!(
            with_repo("/v1/test-results/tests?state=flaky", repo),
            "/v1/test-results/tests?state=flaky&repo=acme%2Fapp"
        );
        assert_eq!(
            with_repo("/v1/cache/summary?repo=other/x", repo),
            "/v1/cache/summary?repo=other/x"
        );
        assert_eq!(with_repo("/healthz", repo), "/healthz");
        assert_eq!(with_repo("/v1/cache/summary", None), "/v1/cache/summary");
    }

    #[test]
    fn a_ci_token_is_sent_and_a_refusal_is_explained() {
        let (url, seen) = serve(vec![
            ("/v1/ok", 200, r#"{"tests":[]}"#.into()),
            ("/v1/denied", 403, r#"{"error":"nope"}"#.into()),
            ("/v1/stale", 401, r#"{"error":"expired"}"#.into()),
        ]);
        let env = |name: &str| (name == "ASTER_SRV_TOKEN").then(|| "ci-secret".to_string());
        let now = SystemTime::now();
        assert_eq!(
            get(&url, "/v1/ok", &env, now).unwrap()["tests"],
            serde_json::json!([])
        );
        assert!(seen.lock().unwrap()[0].contains("authorization: bearer ci-secret"));
        let denied = get(&url, "/v1/denied", &env, now).unwrap_err().to_string();
        assert!(
            denied.contains("403") && denied.contains("nope"),
            "{denied}"
        );
        let stale = get(&url, "/v1/stale", &env, now).unwrap_err().to_string();
        assert!(
            stale.contains("did not accept") && stale.contains("expired"),
            "{stale}"
        );
        let missing = get(&url, "/v1/absent", &env, now).unwrap_err().to_string();
        assert!(missing.contains("not found"), "{missing}");
    }

    #[test]
    fn a_cleartext_server_on_another_host_is_refused_for_reads() {
        let env = |name: &str| (name == "ASTER_SRV_TOKEN").then(|| "secret".to_string());
        let error = get(
            "http://aster-srv.example.com",
            "/v1/test-results/tests",
            &env,
            SystemTime::now(),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("must be https"), "{error:#}");
    }
}
