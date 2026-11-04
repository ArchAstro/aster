//! Canonical end-to-end proof: the model's scenarios drive the real
//! `aster affected --related` engine, and its answers are held to the model's.

use aster_related_model::run_scenarios;
use aster_related_model::shop::Shop;

/// Every `@conformance` run in `scenarios.qnt` lays an Elixir project out as
/// files in a scratch git repo, edits it, and runs the `aster` binary. The test
/// is red unless the engine selects exactly the test files, or a full run, that
/// the model computes for the same repo and edit.
///
/// Fixed scenarios cover each rule (named, callback, macro, unnamed, top-level,
/// route, schema shape, reflection, a change that reaches no test, comments, carve-outs);
/// 40 more lay pseudo-random references over the same definitions.
#[test]
fn selection_matches_model_for_generated_elixir_repos() {
    // Setup: the generated adapter reads the scenarios compiled from the model.
    // Each starts from the model's `init`, and `build` lays its repo out as files
    // and commits them as the baseline; nothing is shared between scenarios.
    //
    // Boundary: `change` rewrites the working tree, one line per edit, and
    // `selectNow` runs `aster affected test --base=HEAD --related --dry-run` on
    // it, as a user's CI would, and reads the plan back from its JSON.
    //
    // Outcome: after `selectNow` the checker compares the plan's test files, or
    // its absence of a replacement command (a full run), with the selection
    // Quint computed. It also compares the repo and change echoed back in the
    // state, so a scenario cannot pass by running a different repo.
    let outcomes = run_scenarios::<Shop>().expect("the scenarios run");

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
    assert!(
        diverged.is_empty(),
        "the engine departs from the model in {diverged:?}"
    );

    // The run is not vacuous: fixed and generated scenarios all ran.
    let names: Vec<&str> = outcomes.iter().map(|(name, _)| name.as_str()).collect();
    for expected in [
        "scenarios.namedChainRun",
        "scenarios.monotoneRun",
        "scenarios.generated40Run",
    ] {
        assert!(
            names.contains(&expected),
            "{expected} did not run: {names:?}"
        );
    }
    assert!(
        outcomes.len() >= 50,
        "only {} scenarios ran",
        outcomes.len()
    );
}
