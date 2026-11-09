#![allow(clippy::disallowed_macros)] // test diagnostics may use std print macros
//! Canonical end-to-end proof: the model's scenarios drive a real aster-srv,
//! and its bucket, rows, manifests and replies are held to the model's.

use aster_dir_cache_model::live::LiveCache;
use aster_dir_cache_model::run_scenarios;

/// Every `@conformance` run in `scenarios.qnt` runs on its own database. The
/// test is red if the server's state differs from the model's after any step.
#[test]
fn the_server_follows_the_model_through_every_scenario() {
    // Setup: each scenario starts a server with its migrations applied, a real
    // Postgres behind it and a 25-byte cap on directory entries. Nothing is
    // shared between scenarios.
    //
    // Boundary: `commitEntry` and `dirLookup` are HTTP requests to the routes a
    // CI job calls. `runRetention` backdates the expired keys and runs the
    // retention pass. Chunks reach the bucket with no row, like a signed PUT.
    //
    // Outcome: after every step the checker compares what Postgres and the
    // bucket hold now, and the last replies, with the state Quint computed.
    let outcomes = run_scenarios::<LiveCache>().expect("the scenarios run");

    // Report every scenario, not only the first divergence.
    let mut diverged = Vec::new();
    for (scenario, outcome) in &outcomes {
        match outcome {
            Ok(obligations) => eprintln!("ok   {scenario}: {obligations} obligations"),
            Err(divergence) => {
                eprintln!("FAIL {scenario}: {divergence}");
                diverged.push(scenario.as_str());
            }
        }
    }
    assert!(diverged.is_empty(), "scenarios diverged: {diverged:?}");
    assert!(!outcomes.is_empty(), "the model has no scenarios");
}
