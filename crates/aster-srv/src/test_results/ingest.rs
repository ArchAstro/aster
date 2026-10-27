//! `POST /v1/test-results/runs`: store one results stream.

use std::collections::BTreeMap;
use std::io::Read;

use aster_results_model::{parse_ndjson, Record, RunRecord, TargetRecord, TestRecord};
use bytes::Bytes;
use chrono::{DateTime, NaiveDate, Utc};
use serde_json::json;
use tokio_postgres::binary_copy::BinaryCopyInWriter;
use tokio_postgres::types::{ToSql, Type};

use crate::auth::Identity;
use crate::http::Reply;
use crate::App;

/// An upload as received.
pub struct Upload {
    pub body: Bytes,
    /// Whether the body is gzip (`Content-Encoding: gzip`).
    pub gzip: bool,
}

/// What storing a stream did.
#[derive(Debug, PartialEq, Eq)]
pub struct Stored {
    pub run_id: String,
    /// The run was already stored; nothing was written.
    pub duplicate: bool,
    pub targets: usize,
    pub tests: usize,
}

pub async fn handle(app: &App, identity: &Identity, upload: Upload) -> Reply {
    let limit = app.config.max_body_bytes;
    let text = match decode(&upload, limit) {
        Ok(text) => text,
        Err(reply) => return reply,
    };
    let records = match parse_ndjson(&text) {
        Ok(records) => records,
        Err(error) => {
            return Reply::error(400, format!("the results are not valid NDJSON: {error}"));
        }
    };
    let (run, targets, tests) = match split(records) {
        Ok(parts) => parts,
        Err(message) => return Reply::error(400, message),
    };
    let (run, source) = match bind(identity, run) {
        Ok(bound) => bound,
        Err(message) => return Reply::error(403, message),
    };

    let received_at = Utc::now();
    let stored = match store(app, &run, &source, &targets, &tests, received_at).await {
        Ok(stored) => stored,
        Err(error) => {
            crate::log::error(
                "cannot store test results",
                json!({"run_id": run.run_id, "error": format!("{error:#}")}),
            );
            return Reply::error(503, "the results could not be stored; retry later");
        }
    };
    if !stored.duplicate {
        let key = raw_key(&run, received_at.date_naive());
        let body = if upload.gzip {
            upload.body
        } else {
            gzip(&upload.body)
        };
        if let Err(error) = app.raw_store.put(&key, body).await {
            crate::log::warn(
                "cannot keep the raw upload",
                json!({"run_id": run.run_id, "error": format!("{error:#}")}),
            );
        }
    }
    crate::log::info(
        "stored test results",
        json!({
            "repo": run.repo,
            "run_id": stored.run_id,
            "duplicate": stored.duplicate,
            "targets": stored.targets,
            "tests": stored.tests,
        }),
    );
    Reply::json(
        if stored.duplicate { 200 } else { 201 },
        json!({
            "run_id": stored.run_id,
            "duplicate": stored.duplicate,
            "targets": stored.targets,
            "tests": stored.tests,
        }),
    )
}

/// The upload as text, decompressing gzip up to `limit` bytes so a small
/// body cannot expand without bound.
fn decode(upload: &Upload, limit: usize) -> Result<String, Reply> {
    let bytes = if upload.gzip {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&upload.body[..])
            .take(limit as u64 + 1)
            .read_to_end(&mut out)
            .map_err(|error| Reply::error(400, format!("the body is not valid gzip: {error}")))?;
        if out.len() > limit {
            return Err(Reply::error(
                413,
                format!("the results are larger than {limit} bytes"),
            ));
        }
        out
    } else {
        upload.body.to_vec()
    };
    String::from_utf8(bytes).map_err(|_| Reply::error(400, "the results are not UTF-8"))
}

/// Where a run came from, as its token says. The classifier trusts these
/// columns, never the run record's own `branch`, to tell default-branch
/// runs from the rest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// `oidc` or `static`.
    pub kind: &'static str,
    /// The OIDC `ref` claim.
    pub git_ref: Option<String>,
    /// The OIDC `event_name` claim.
    pub event_name: Option<String>,
}

/// Check a run record against the token that uploaded it, and replace what
/// the token knows better. The record's fields are the client's to choose,
/// so without this one job could file results under another job's run id
/// (taking it first, so the real upload is a duplicate), another repository's
/// id, or a commit and branch it never ran.
///
/// - An OIDC token's run id must be `gh:<repo>:<run_id>:<run_attempt>:...`
///   for the token's own repository, run and attempt (what Aster's client
///   builds in GitHub Actions); its `sha` must be the token's; `branch`,
///   `pr` and `attempt` come from the `ref` and `run_attempt` claims.
/// - A static token's run id names its repository: a `gh:` id must be
///   `gh:<its repo>:...`, and any other id is stored as
///   `static:<its repo>:<id>`.
///
/// Repositories compare without case, as GitHub's do. So no two
/// repositories' run ids can collide.
pub fn bind(identity: &Identity, mut run: RunRecord) -> Result<(RunRecord, Source), String> {
    if !run.repo.eq_ignore_ascii_case(&identity.repository) {
        return Err(format!(
            "the token is for {} but the results are for {}",
            identity.repository, run.repo
        ));
    }
    let starts_with = |id: &str, prefix: &str| {
        id.get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
    };
    let repo_prefix = format!("gh:{}:", identity.repository);
    if identity.static_token {
        if starts_with(&run.run_id, "gh:") {
            if !starts_with(&run.run_id, &repo_prefix) {
                return Err(format!(
                    "the run id {} is not one of {}'s",
                    run.run_id, identity.repository
                ));
            }
        } else {
            run.run_id = format!("static:{}:{}", identity.repository, run.run_id);
        }
        return Ok((
            run,
            Source {
                kind: "static",
                git_ref: None,
                event_name: None,
            },
        ));
    }

    let Some(github_run) = identity.run_id.as_deref() else {
        return Err("the token has no run_id claim".into());
    };
    let prefix = match identity.run_attempt.as_deref() {
        Some(attempt) => format!("{repo_prefix}{github_run}:{attempt}:"),
        None => format!("{repo_prefix}{github_run}:"),
    };
    if !starts_with(&run.run_id, &prefix) {
        return Err(format!(
            "the run id {} is not this job's: it must start with {prefix}",
            run.run_id
        ));
    }
    if let Some(sha) = identity.sha.as_deref() {
        if run.sha != sha {
            return Err(format!(
                "the results are for commit {} but the token is for {sha}",
                run.sha
            ));
        }
    }
    if let Some(attempt) = identity.run_attempt.as_deref() {
        run.attempt = attempt.parse().ok();
    }
    if let Some(git_ref) = identity.git_ref.as_deref() {
        run.pr = pull_request(git_ref);
        if let Some(branch) = git_ref.strip_prefix("refs/heads/") {
            run.branch = Some(branch.to_string());
        }
    }
    Ok((
        run,
        Source {
            kind: "oidc",
            git_ref: identity.git_ref.clone(),
            event_name: identity.event_name.clone(),
        },
    ))
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

type Parts = (RunRecord, Vec<TargetRecord>, Vec<TestRecord>);

/// The run record (first and only one), then its targets and tests.
fn split(records: Vec<Record>) -> Result<Parts, String> {
    let mut records = records.into_iter();
    let Some(Record::Run(run)) = records.next() else {
        return Err("the results must start with a run record".into());
    };
    if run.run_id.is_empty() || run.run_id.len() > 512 {
        return Err("the run id must be 1 to 512 characters".into());
    }
    let mut targets = Vec::new();
    let mut tests = Vec::new();
    for record in records {
        match record {
            Record::Run(_) => return Err("the results hold more than one run record".into()),
            Record::Target(target) => targets.push(target),
            Record::Test(test) => tests.push(test),
        }
    }
    Ok((run, targets, tests))
}

fn enum_text<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Store a stream in one transaction. A run id already stored is a
/// duplicate: nothing is written and the first upload stands.
pub async fn store(
    app: &App,
    run: &RunRecord,
    source: &Source,
    targets: &[TargetRecord],
    tests: &[TestRecord],
    received_at: DateTime<Utc>,
) -> anyhow::Result<Stored> {
    let received_on: NaiveDate = received_at.date_naive();
    let mut client = app.pool.get().await?;
    super::ensure_partitions(&client, received_on).await?;
    let transaction = client.transaction().await?;

    let started_at = DateTime::parse_from_rfc3339(&run.started_at)
        .map(|at| at.with_timezone(&Utc))
        .unwrap_or(received_at);
    let pr = run.pr.and_then(|pr| i64::try_from(pr).ok());
    let attempt = run.attempt.and_then(|a| i32::try_from(a).ok());
    let inserted = transaction
        .execute(
            "INSERT INTO test_results.runs (run_id, repo, sha, branch, pr, workflow, job, \
             attempt, lane, runner, aster_version, started_at, received_at, \
             source, git_ref, event_name) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16) \
             ON CONFLICT (run_id) DO NOTHING",
            &[
                &run.run_id,
                &run.repo,
                &run.sha,
                &run.branch,
                &pr,
                &run.workflow,
                &run.job,
                &attempt,
                &run.lane,
                &run.runner,
                &run.aster_version,
                &started_at,
                &received_at,
                &source.kind,
                &source.git_ref,
                &source.event_name,
            ],
        )
        .await?;
    if inserted == 0 {
        transaction.rollback().await?;
        return Ok(Stored {
            run_id: run.run_id.clone(),
            duplicate: true,
            targets: 0,
            tests: 0,
        });
    }

    let sink = transaction
        .copy_in(
            "COPY test_results.target_runs (received_on, run_id, repo, address, input_hash, \
             status, cached, duration_ms, capture, confidence, exit_code) FROM STDIN BINARY",
        )
        .await?;
    let writer = BinaryCopyInWriter::new(
        sink,
        &[
            Type::DATE,
            Type::TEXT,
            Type::TEXT,
            Type::TEXT,
            Type::TEXT,
            Type::TEXT,
            Type::BOOL,
            Type::INT8,
            Type::TEXT,
            Type::TEXT,
            Type::INT4,
        ],
    );
    tokio::pin!(writer);
    for target in targets {
        let status = enum_text(&target.status);
        let confidence = target.confidence.as_ref().map(enum_text);
        let duration = i64::try_from(target.duration_ms).unwrap_or(i64::MAX);
        let row: [&(dyn ToSql + Sync); 11] = [
            &received_on,
            &run.run_id,
            &run.repo,
            &target.address,
            &target.input_hash,
            &status,
            &target.cached,
            &duration,
            &target.capture,
            &confidence,
            &target.exit_code,
        ];
        writer.as_mut().write(&row).await?;
    }
    writer.finish().await?;

    let sink = transaction
        .copy_in(
            "COPY test_results.test_results (received_on, run_id, repo, target, test_id, \
             outcome, attempt, duration_ms, failure_hash, message) FROM STDIN BINARY",
        )
        .await?;
    let writer = BinaryCopyInWriter::new(
        sink,
        &[
            Type::DATE,
            Type::TEXT,
            Type::TEXT,
            Type::TEXT,
            Type::TEXT,
            Type::TEXT,
            Type::INT4,
            Type::INT8,
            Type::TEXT,
            Type::TEXT,
        ],
    );
    tokio::pin!(writer);
    for test in tests {
        let outcome = enum_text(&test.outcome);
        let attempt = i32::try_from(test.attempt).unwrap_or(i32::MAX);
        let duration = test
            .duration_ms
            .map(|ms| i64::try_from(ms).unwrap_or(i64::MAX));
        let row: [&(dyn ToSql + Sync); 10] = [
            &received_on,
            &run.run_id,
            &run.repo,
            &test.target,
            &test.test_id,
            &outcome,
            &attempt,
            &duration,
            &test.failure_hash,
            &test.message,
        ];
        writer.as_mut().write(&row).await?;
    }
    writer.finish().await?;

    // One row per test, however many attempts it had, in `test_id` byte
    // order: every writer of `tests` locks its rows in (repo, test_id) "C"
    // order, so two uploads, or an upload and a classifier pass, sharing
    // tests wait for each other instead of deadlocking. A row is rewritten
    // only when its name changed or `last_seen` is an hour behind: every
    // upload rewriting every test (and `last_seen`'s index) bloated the
    // table with dead rows.
    let mut seen: BTreeMap<&str, &TestRecord> = BTreeMap::new();
    for test in tests {
        seen.insert(&test.test_id, test);
    }
    let (mut ids, mut target_col, mut files, mut suites, mut names) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (id, test) in &seen {
        ids.push(*id);
        target_col.push(test.target.as_str());
        files.push(test.file.as_deref());
        suites.push(test.suite.as_deref());
        names.push(test.name.as_str());
    }
    transaction
        .execute(
            "INSERT INTO test_results.tests (repo, test_id, target, file, suite, name, first_seen, last_seen) \
             SELECT $1, t.test_id, t.target, t.file, t.suite, t.name, $2, $2 \
             FROM unnest($3::text[], $4::text[], $5::text[], $6::text[], $7::text[]) \
                  WITH ORDINALITY AS t(test_id, target, file, suite, name, n) \
             ORDER BY t.n \
             ON CONFLICT (repo, test_id) DO UPDATE SET \
                 target = EXCLUDED.target, file = EXCLUDED.file, suite = EXCLUDED.suite, \
                 name = EXCLUDED.name, \
                 last_seen = GREATEST(test_results.tests.last_seen, EXCLUDED.last_seen) \
             WHERE (test_results.tests.target, test_results.tests.file, \
                    test_results.tests.suite, test_results.tests.name) \
                   IS DISTINCT FROM \
                   (EXCLUDED.target, EXCLUDED.file, EXCLUDED.suite, EXCLUDED.name) \
                OR test_results.tests.last_seen < EXCLUDED.last_seen - interval '1 hour'",
            &[
                &run.repo,
                &received_at,
                &ids,
                &target_col,
                &files,
                &suites,
                &names,
            ],
        )
        .await?;

    transaction.commit().await?;
    Ok(Stored {
        run_id: run.run_id.clone(),
        duplicate: false,
        targets: targets.len(),
        tests: tests.len(),
    })
}

/// `test-results/<owner>/<name>/<day>/<run id>.ndjson.gz`.
fn raw_key(run: &RunRecord, day: NaiveDate) -> String {
    let safe = |text: &str| -> String {
        text.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    let repo = run.repo.split('/').map(safe).collect::<Vec<_>>().join("/");
    format!("test-results/{repo}/{day}/{}.ndjson.gz", safe(&run.run_id))
}

fn gzip(body: &[u8]) -> Bytes {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let _ = encoder.write_all(body);
    Bytes::from(encoder.finish().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> RunRecord {
        RunRecord {
            run_id: "gh:ArchAstro/firstlanding:1:1:test-ci:test-ci:-:ab".into(),
            repo: "ArchAstro/firstlanding".into(),
            sha: "abc".into(),
            branch: None,
            pr: None,
            workflow: None,
            job: None,
            attempt: None,
            lane: None,
            runner: None,
            aster_version: "0.18.0".into(),
            started_at: "2026-10-13T00:00:00Z".into(),
        }
    }

    fn oidc(git_ref: &str, event: &str) -> Identity {
        Identity {
            repository: "ArchAstro/firstlanding".into(),
            git_ref: Some(git_ref.into()),
            run_id: Some("1".into()),
            run_attempt: Some("1".into()),
            sha: Some("abc".into()),
            workflow: None,
            job_workflow: None,
            event_name: Some(event.into()),
            static_token: false,
        }
    }

    #[test]
    fn an_oidc_upload_is_bound_to_its_own_run_commit_and_ref() {
        // A pull request job: its branch and number come from `ref`, not the
        // record, which claims to be main.
        let mut claimed = run();
        claimed.branch = Some("main".into());
        claimed.pr = Some(999);
        let (bound, source) = bind(&oidc("refs/pull/7/merge", "pull_request"), claimed).unwrap();
        assert_eq!(bound.pr, Some(7));
        assert_eq!(bound.attempt, Some(1));
        assert_eq!(
            source,
            Source {
                kind: "oidc",
                git_ref: Some("refs/pull/7/merge".into()),
                event_name: Some("pull_request".into()),
            }
        );
        // A push: the branch is the ref's.
        let (bound, _) = bind(&oidc("refs/heads/main", "push"), run()).unwrap();
        assert_eq!((bound.branch.as_deref(), bound.pr), (Some("main"), None));
        // Repository case does not matter.
        let mut lower = run();
        lower.run_id = "gh:archastro/FIRSTLANDING:1:1:x".into();
        assert!(bind(&oidc("refs/heads/main", "push"), lower).is_ok());
    }

    #[test]
    fn an_oidc_upload_cannot_take_another_runs_id_or_commit() {
        let job = oidc("refs/pull/7/merge", "pull_request");
        for (run_id, why) in [
            // A later run of the repository: taking it first would make the
            // real upload a duplicate.
            ("gh:ArchAstro/firstlanding:2:1:test-ci:x", "another run"),
            ("gh:ArchAstro/firstlanding:1:2:test-ci:x", "another attempt"),
            (
                "gh:ArchAstro/firstlanding:12:1:test-ci:x",
                "a run id it prefixes",
            ),
            ("gh:ArchAstro/other:1:1:test-ci:x", "another repository"),
            ("static:ArchAstro/firstlanding:x", "a static token's"),
            ("local:test:ab", "not a GitHub run"),
        ] {
            let mut forged = run();
            forged.run_id = run_id.into();
            assert!(bind(&job, forged).is_err(), "{why}");
        }
        let mut other_commit = run();
        other_commit.sha = "def".into();
        assert!(bind(&job, other_commit).is_err());
        let mut no_run = job.clone();
        no_run.run_id = None;
        assert!(bind(&no_run, run()).is_err());
        let mut other_repo = run();
        other_repo.repo = "ArchAstro/other".into();
        assert!(bind(&job, other_repo).is_err());
    }

    #[test]
    fn a_static_tokens_run_ids_name_its_repository() {
        let token = Identity {
            repository: "ArchAstro/dev".into(),
            git_ref: None,
            run_id: None,
            run_attempt: None,
            sha: None,
            workflow: None,
            job_workflow: None,
            event_name: None,
            static_token: true,
        };
        let mut local = run();
        local.repo = "ArchAstro/dev".into();
        local.run_id = "local:test:ab".into();
        let (bound, source) = bind(&token, local.clone()).unwrap();
        assert_eq!(bound.run_id, "static:ArchAstro/dev:local:test:ab");
        assert_eq!(source.kind, "static");
        // Its own repository's GitHub ids pass; another's do not.
        local.run_id = "gh:ArchAstro/dev:5:1:x".into();
        assert_eq!(bind(&token, local.clone()).unwrap().0.run_id, local.run_id);
        local.run_id = "gh:ArchAstro/firstlanding:5:1:x".into();
        assert!(bind(&token, local).is_err());
    }

    #[test]
    fn raw_keys_are_safe_paths() {
        let day = NaiveDate::from_ymd_opt(2026, 10, 13).unwrap();
        assert_eq!(
            raw_key(&run(), day),
            "test-results/ArchAstro/firstlanding/2026-10-13/gh_ArchAstro_firstlanding_1_1_test-ci_test-ci_-_ab.ndjson.gz"
        );
    }

    #[test]
    fn gzip_bodies_are_bounded() {
        let big = vec![b'a'; 10_000];
        let upload = Upload {
            body: gzip(&big),
            gzip: true,
        };
        assert_eq!(decode(&upload, 10_000).unwrap().len(), 10_000);
        let reply = decode(&upload, 9_999).unwrap_err();
        assert_eq!(reply.status, 413);
        let garbage = Upload {
            body: Bytes::from_static(b"not gzip"),
            gzip: true,
        };
        assert_eq!(decode(&garbage, 100).unwrap_err().status, 400);
    }

    #[test]
    fn a_stream_has_exactly_one_leading_run() {
        assert!(split(vec![]).is_err());
        assert!(split(vec![Record::Run(run()), Record::Run(run())]).is_err());
        let (parsed, targets, tests) = split(vec![Record::Run(run())]).unwrap();
        assert_eq!(parsed.repo, "ArchAstro/firstlanding");
        assert!(targets.is_empty() && tests.is_empty());
    }
}
