//! Where and what one Aster invocation ran, for its results stream.

use std::path::Path;

use aster_results_model::RunRecord;

/// Build the run record from the CI environment, falling back to the local
/// git checkout.
///
/// `env` looks up an environment variable; it is a parameter so tests can
/// supply their own. `ASTER_RUN_ID` and `ASTER_RUNNER` override the
/// detected run id and runner label.
pub fn detect_run(
    workspace_root: &Path,
    target: &str,
    lane: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
) -> RunRecord {
    let var = |name: &str| env(name).filter(|value| !value.is_empty());
    let repository = git2::Repository::discover(workspace_root).ok();

    let repo = var("GITHUB_REPOSITORY")
        .or_else(|| repository.as_ref().and_then(origin_repo))
        .unwrap_or_else(|| {
            workspace_root
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "unknown".to_string())
        });
    let sha = var("GITHUB_SHA")
        .or_else(|| {
            repository
                .as_ref()
                .and_then(|r| r.head().ok())
                .and_then(|head| head.target())
                .map(|oid| oid.to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());
    let branch = var("GITHUB_HEAD_REF")
        .or_else(|| var("GITHUB_REF_NAME"))
        .or_else(|| {
            repository
                .as_ref()
                .and_then(|r| r.head().ok())
                .filter(|head| head.is_branch())
                .and_then(|head| head.shorthand().ok().map(str::to_string))
        });
    let pr = var("GITHUB_REF").and_then(|r| pull_request(&r));
    let attempt = var("GITHUB_RUN_ATTEMPT").and_then(|a| a.parse().ok());
    let job = var("GITHUB_JOB");
    let nonce = nonce();
    let run_id = var("ASTER_RUN_ID").unwrap_or_else(|| match var("GITHUB_RUN_ID") {
        Some(run) => format!(
            "gh:{repo}:{run}:{}:{}:{target}:{}:{nonce}",
            attempt.unwrap_or(1),
            job.as_deref().unwrap_or("-"),
            lane.unwrap_or("-"),
        ),
        None => format!("local:{target}:{nonce}"),
    });

    RunRecord {
        run_id,
        repo,
        sha,
        branch,
        pr,
        workflow: var("GITHUB_WORKFLOW"),
        job,
        attempt,
        lane: lane.map(str::to_string),
        runner: var("ASTER_RUNNER").or_else(|| var("RUNNER_NAME")),
        aster_version: env!("CARGO_PKG_VERSION").to_string(),
        started_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    }
}

/// The pull request a ref belongs to: `refs/pull/123/merge`, or a merge
/// queue branch `refs/heads/gh-readonly-queue/main/pr-123-<sha>`.
fn pull_request(reference: &str) -> Option<u64> {
    if let Some(rest) = reference.strip_prefix("refs/pull/") {
        return rest.split('/').next()?.parse().ok();
    }
    let queue = reference.strip_prefix("refs/heads/gh-readonly-queue/")?;
    let entry = queue.rsplit('/').next()?;
    entry.strip_prefix("pr-")?.split('-').next()?.parse().ok()
}

/// `owner/name` from the `origin` remote, for GitHub-style URLs.
fn origin_repo(repository: &git2::Repository) -> Option<String> {
    let remote = repository.find_remote("origin").ok()?;
    owner_and_name(remote.url().ok()?)
}

fn owner_and_name(url: &str) -> Option<String> {
    let path = url
        .split_once("://")
        .map(|(_, rest)| rest.split_once('/').map(|(_, path)| path).unwrap_or(""))
        .or_else(|| url.split_once(':').map(|(_, path)| path))?;
    let path = path.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = path.rsplit('/');
    let name = parts.next().filter(|n| !n.is_empty())?;
    let owner = parts.next().filter(|o| !o.is_empty())?;
    Some(format!("{owner}/{name}"))
}

/// Eight random bytes as hex, so two invocations in one CI job never share
/// a run id.
fn nonce() -> String {
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        bytes = (nanos as u64 ^ u64::from(std::process::id())).to_le_bytes();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn github_actions_pull_request() {
        let tmp = tempfile::tempdir().unwrap();
        let env = env_of(&[
            ("GITHUB_REPOSITORY", "ArchAstro/firstlanding"),
            ("GITHUB_SHA", "9f1c"),
            ("GITHUB_HEAD_REF", "feature/x"),
            ("GITHUB_REF_NAME", "16161/merge"),
            ("GITHUB_REF", "refs/pull/16161/merge"),
            ("GITHUB_WORKFLOW", "CI"),
            ("GITHUB_JOB", "test-ci"),
            ("GITHUB_RUN_ID", "18233412"),
            ("GITHUB_RUN_ATTEMPT", "2"),
            ("RUNNER_NAME", "blacksmith-1"),
        ]);
        let run = detect_run(tmp.path(), "test-ci", Some("services-platform"), &env);
        assert_eq!(run.repo, "ArchAstro/firstlanding");
        assert_eq!(run.sha, "9f1c");
        assert_eq!(run.branch.as_deref(), Some("feature/x"));
        assert_eq!(run.pr, Some(16161));
        assert_eq!(run.attempt, Some(2));
        assert_eq!(run.lane.as_deref(), Some("services-platform"));
        assert_eq!(run.runner.as_deref(), Some("blacksmith-1"));
        assert!(
            run.run_id.starts_with(
                "gh:ArchAstro/firstlanding:18233412:2:test-ci:test-ci:services-platform:"
            ),
            "{}",
            run.run_id
        );
        assert!(run.started_at.ends_with('Z'));
    }

    #[test]
    fn every_invocation_gets_its_own_run_id() {
        let tmp = tempfile::tempdir().unwrap();
        let env = env_of(&[("GITHUB_RUN_ID", "1"), ("GITHUB_REPOSITORY", "a/b")]);
        let a = detect_run(tmp.path(), "test", None, &env);
        let b = detect_run(tmp.path(), "test", None, &env);
        assert_ne!(a.run_id, b.run_id);
    }

    #[test]
    fn overrides_win() {
        let tmp = tempfile::tempdir().unwrap();
        let env = env_of(&[
            ("ASTER_RUN_ID", "fixed"),
            ("ASTER_RUNNER", "blacksmith-8vcpu-ubuntu-2404"),
            ("RUNNER_NAME", "instance-7"),
        ]);
        let run = detect_run(tmp.path(), "test", None, &env);
        assert_eq!(run.run_id, "fixed");
        assert_eq!(run.runner.as_deref(), Some("blacksmith-8vcpu-ubuntu-2404"));
    }

    #[test]
    fn local_run_reads_git() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(tmp.path()).unwrap();
        repo.remote("origin", "git@github.com:ArchAstro/aster.git")
            .unwrap();
        let signature = git2::Signature::now("t", "t@example.com").unwrap();
        let tree = repo
            .find_tree(repo.index().unwrap().write_tree().unwrap())
            .unwrap();
        let oid = repo
            .commit(Some("HEAD"), &signature, &signature, "init", &tree, &[])
            .unwrap();
        let run = detect_run(tmp.path(), "test", None, &env_of(&[]));
        assert_eq!(run.repo, "ArchAstro/aster");
        assert_eq!(run.sha, oid.to_string());
        assert!(run.branch.is_some());
        assert!(run.run_id.starts_with("local:test:"));
        assert_eq!(run.pr, None);
    }

    #[test]
    fn reads_pull_requests_from_refs() {
        assert_eq!(pull_request("refs/pull/42/merge"), Some(42));
        assert_eq!(
            pull_request("refs/heads/gh-readonly-queue/main/pr-16161-0123abc"),
            Some(16161)
        );
        assert_eq!(pull_request("refs/heads/main"), None);
    }

    #[test]
    fn reads_owner_and_name_from_remote_urls() {
        for url in [
            "https://github.com/ArchAstro/aster.git",
            "https://github.com/ArchAstro/aster",
            "git@github.com:ArchAstro/aster.git",
            "ssh://git@github.com/ArchAstro/aster.git",
        ] {
            assert_eq!(
                owner_and_name(url).as_deref(),
                Some("ArchAstro/aster"),
                "{url}"
            );
        }
        assert_eq!(owner_and_name("https://example.com/"), None);
    }
}
