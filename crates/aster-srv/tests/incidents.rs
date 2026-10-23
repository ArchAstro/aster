//! A shard that crashes mid-run is one incident, not hundreds of flaky
//! tests.
//!
//! Replays firstlanding's 2026-10-17 core shard 6: a pull request run
//! passed all 4630 tests, the next main run failed 644 of them when the
//! test harness's global Mox server died (every later test's sandbox setup
//! failed with `:already_shared`), and a main run on the same inputs passed
//! again. Before incidents, each of the 644 got two definite flake events
//! (pass to fail, then fail to pass), a score of 6, and quarantine.
//!
//! Set ASTER_SRV_TEST_DATABASE_URL to run it; without it the test skips.

mod common;

use std::io::Write;

use aster_results_model::{
    failure_hash, to_ndjson, Confidence, Record, RunRecord, TargetRecord, TargetStatus,
    TestOutcome, TestRecord,
};
use aster_srv::test_results::classifier;
use chrono::Utc;
use common::{iap_assertion, start, token, Server, IAP_AUDIENCE, REPO};
use serde_json::{json, Value};

const SHARD_6: &str = "//src/elixir/core-test-shard-6:test-ci";
const SHARD_2: &str = "//src/elixir/core-test-shard-2:test-ci";
const SHARD_6_TESTS: usize = 4630;
const SHARD_2_TESTS: usize = 100;
/// Shard 6's inputs did not change between the three runs.
const SHARD_6_INPUTS: &str = "sha256:core-shard-6";
const SHARD_2_INPUTS: &str = "sha256:core-shard-2";

/// The files the cascade hit, with how many of their tests failed.
const CASCADE: &[(&str, &str, usize)] = &[
    (
        "test/solutions/install/action/update_agent_shape_matrix_test.exs",
        "ArchAstro.Solutions.Install.Action.UpdateAgentShapeMatrixTest",
        507,
    ),
    (
        "test/custom_objects/search_test.exs",
        "ArchAstro.CustomObjects.SearchTest",
        31,
    ),
    (
        "test/workers/chat/chat_room_shutdown_worker_test.exs",
        "ArchAstro.Workers.Chat.ChatRoomShutdownWorkerTest",
        29,
    ),
    (
        "test/agents/agent_provisioner/install/attach_actions_test.exs",
        "ArchAstro.Agents.AgentProvisioner.Install.AttachActionsTest",
        25,
    ),
    (
        "test/events/custom_events_test.exs",
        "ArchAstro.Events.CustomEventsTest",
        15,
    ),
    (
        "test/chat/chat_room_push_muting_test.exs",
        "ArchAstro.Chat.ChatRoomPushMutingTest",
        11,
    ),
    (
        "test/billing/billing_plan_enrollment_backfill_integration_test.exs",
        "ArchAstro.Billing.BillingPlanEnrollmentBackfillIntegrationTest",
        10,
    ),
    (
        "test/config/identity_fields_test.exs",
        "ArchAstro.Config.IdentityFieldsTest",
        6,
    ),
    (
        "test/agents/routines/llm_config_test.exs",
        "ArchAstro.Agents.Routines.LLMConfigTest",
        6,
    ),
    (
        "test/agents/agent_knowledge_promotion_test.exs",
        "ArchAstro.Agents.AgentKnowledgePromotionTest",
        3,
    ),
    (
        "test/billing/enrollment_activation_end_to_end_test.exs",
        "ArchAstro.Billing.EnrollmentActivationEndToEndTest",
        1,
    ),
];

/// The setup failure every cascaded test reported, naming its own file.
fn already_shared(file: &str, suite: &str) -> String {
    format!(
        "** (MatchError) no match of right hand side value:\n\n    {{:error,\n     \
         {{{{:badmatch, :already_shared}},\n      [{{Ecto.Adapters.SQL.Sandbox, \
         :\"-start_owner!/2-fun-0-\", 3, [file: ~c\"lib/ecto/adapters/sql/sandbox.ex\", \
         line: 457]}}]}}}}\n\nstacktrace:\n  (archastro_core 0.1.0) \
         test/support/data_case.ex:51: ArchAstro.DataCase.setup_sandbox/1\n  {file}:3: \
         {suite}.__ex_unit__/2"
    )
}

/// One test of a shard: its id, file and suite. Shard 6's first 644 tests
/// are the cascade's files in order; the rest are other files.
fn shard_test(shard: &str, i: usize) -> (String, String, String) {
    let id = format!("{shard}#{i}");
    if shard == SHARD_6 {
        let mut first = 0;
        for (file, suite, count) in CASCADE {
            if i < first + count {
                return (id, file.to_string(), suite.to_string());
            }
            first += count;
        }
    }
    (
        id,
        format!("test/other_{}_test.exs", i / 40),
        "ArchAstro.Other".into(),
    )
}

/// `run_id`'s upload: shard 6 with its first `shard_6_failures` tests
/// failing, and shard 2 with `lone_failure` deciding whether its test 7
/// fails (an ordinary flake, unrelated to the crash).
fn upload_stream(
    run_id: &str,
    branch: &str,
    shard_6_failures: usize,
    lone_failure: bool,
) -> String {
    let mut records = vec![Record::Run(RunRecord {
        run_id: run_id.into(),
        repo: REPO.into(),
        sha: "78d67670d".into(),
        branch: Some(branch.into()),
        pr: None,
        workflow: Some("CI".into()),
        job: Some("test".into()),
        attempt: Some(1),
        lane: Some("source-elixir-core-6".into()),
        runner: Some("blacksmith-16vcpu-ubuntu-2404".into()),
        aster_version: "0.22.0".into(),
        started_at: "2026-10-17T03:58:35Z".into(),
    })];
    for (shard, inputs, tests, failures) in [
        (SHARD_6, SHARD_6_INPUTS, SHARD_6_TESTS, shard_6_failures),
        (SHARD_2, SHARD_2_INPUTS, SHARD_2_TESTS, 0),
    ] {
        let lone = |i: usize| shard == SHARD_2 && i == 7 && lone_failure;
        let failed_any = failures > 0 || (shard == SHARD_2 && lone_failure);
        records.push(Record::Target(TargetRecord {
            address: shard.into(),
            input_hash: Some(inputs.into()),
            status: if failed_any {
                TargetStatus::Failed
            } else {
                TargetStatus::Passed
            },
            cached: false,
            duration_ms: 129_000,
            capture: Some("exunit".into()),
            confidence: Some(Confidence::Complete),
            exit_code: Some(if failed_any { 2 } else { 0 }),
        }));
        for i in 0..tests {
            let (test_id, file, suite) = shard_test(shard, i);
            let message = if i < failures {
                Some(already_shared(&file, &suite))
            } else if lone(i) {
                Some("** (ExUnit.AssertionError) timed out waiting for :ready".to_string())
            } else {
                None
            };
            records.push(Record::Test(TestRecord {
                target: shard.into(),
                test_id,
                file: Some(file),
                suite: Some(suite),
                name: format!("test {i}"),
                outcome: if message.is_some() {
                    TestOutcome::Fail
                } else {
                    TestOutcome::Pass
                },
                attempt: 1,
                duration_ms: Some(5),
                failure_hash: message.as_deref().map(failure_hash),
                message,
                quarantined: false,
            }));
        }
    }
    to_ndjson(&records)
}

/// Upload a stream as CI does: gzipped NDJSON with the job's OIDC token.
async fn upload(server: &Server, ndjson: &str) {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(ndjson.as_bytes()).unwrap();
    let response = aster_srv::http_client()
        .build()
        .unwrap()
        .post(format!("{}/v1/test-results/runs", server.url))
        .header("Content-Type", "application/x-ndjson")
        .header("Content-Encoding", "gzip")
        .bearer_auth(token(REPO))
        .body(gz.finish().unwrap())
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let body: Value = response.json().await.unwrap_or(json!(null));
    assert_eq!(status, 201, "upload refused: {body}");
}

async fn classify(server: &Server) -> classifier::Report {
    classifier::run_once(&server.app, Utc::now())
        .await
        .unwrap()
        .expect("the lock is free")
}

#[tokio::test]
async fn a_crashed_shard_on_unchanged_inputs_is_one_incident_and_quarantines_nothing() {
    // Setup: production defaults (two flaking runs quarantine a test), and
    // the dashboard reachable as a person behind IAP.
    let Some(server) = start(&[("ASTER_SRV_IAP_AUDIENCE", IAP_AUDIENCE)]).await else {
        return;
    };
    let cascade: usize = CASCADE.iter().map(|(_, _, n)| n).sum();
    assert_eq!(cascade, 644);

    // 03:52Z, the pull request run: both shards pass. Shard 2's test 7 is
    // an ordinary flake: it fails once, in the main run below.
    upload(
        &server,
        &upload_stream("gh:pr:39000054301", "fix/pr", 0, false),
    )
    .await;
    classify(&server).await;

    // 04:00Z, Stable CI on main: the global Mox server dies and the 644
    // tests after it fail in sandbox setup. A classifier pass runs before
    // the next upload, as the five-minute loop did that morning.
    upload(
        &server,
        &upload_stream("gh:main:39000703969", "main", 644, true),
    )
    .await;
    let crash = classify(&server).await;
    assert_eq!(crash.incidents, 1, "the crashed shard is one incident");

    // 04:26Z, Stable CI on main again, same inputs: everything passes.
    upload(
        &server,
        &upload_stream("gh:main:39002241971", "main", 0, false),
    )
    .await;
    classify(&server).await;

    // Outcome 1: one incident row names the crashed target run and its
    // biggest shared failure (the 507 tests of one file).
    let client = server.client().await;
    let incidents = client
        .query(
            "SELECT run_id, target, failures, tests, shared FROM test_results.target_incidents",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(incidents.len(), 1);
    let row = &incidents[0];
    assert_eq!(row.get::<_, String>("run_id"), "gh:main:39000703969");
    assert_eq!(row.get::<_, String>("target"), SHARD_6);
    assert_eq!(row.get::<_, i32>("failures"), 644);
    assert_eq!(row.get::<_, i32>("tests"), SHARD_6_TESTS as i32);
    assert_eq!(row.get::<_, i32>("shared"), 507);

    // Outcome 2: no shard 6 test has a flake event or a score. The lone
    // shard 2 failure is one definite event, owned by the run that failed,
    // even though two passes on its inputs surround it: score 3, not 6.
    let events = client
        .query(
            "SELECT test_id, kind, run_id FROM test_results.test_flake_events",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect::<Vec<(String, String, String)>>();
    assert_eq!(
        events,
        vec![(
            format!("{SHARD_2}#7"),
            "definite".to_string(),
            "gh:main:39000703969".to_string()
        )]
    );
    let flaky = client
        .query(
            "SELECT test_id, flake_score FROM test_results.tests WHERE state <> 'healthy'",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect::<Vec<(String, i32)>>();
    assert_eq!(flaky, vec![(format!("{SHARD_2}#7"), 3)]);

    // Outcome 3: CI's quarantine feed, which aster fetches when it runs
    // tests with ASTER_SRV_URL set, is empty.
    let (status, feed) = server.get("/v1/test-results/quarantine", Some(REPO)).await;
    assert_eq!(status, 200, "{feed}");
    assert_eq!(feed["tests"], json!([]), "nothing is quarantined: {feed}");

    // Outcome 4: the dashboard's flaky list shows the lone flake and none
    // of the crashed shard's tests.
    let (status, _, page) = server
        .get_page("/ui", Some(&iap_assertion("ann@example.com")))
        .await;
    assert_eq!(status, 200);
    assert!(
        page.contains(SHARD_2) && page.contains("test/other_0_test.exs"),
        "{page}"
    );
    assert!(
        !page.contains(SHARD_6) && !page.contains("update_agent_shape_matrix"),
        "a crashed shard's tests are not flaky: {page}"
    );
    drop(client);
    server.stop().await;
}
