//! Runs aster-srv's directory cache through the histories in its Quint model,
//! so that the model and the server are checked against each other.
//!
//! * `model.qnt` and `scenarios.qnt` are the model.
//! * `generated_refinement.rs` is generated from them by `quint-refinements`.
//! * `live` runs each action against a real server and reads its state back.

// The server, database and token helpers aster-srv's own tests start from.
#[path = "../../../crates/aster-srv/tests/common/mod.rs"]
mod common;
#[rustfmt::skip] // `npm run check` compares it byte for byte with the compiler's output.
pub mod generated_refinement;
pub mod live;

use generated_refinement::{artifact, Driver, Implementation, OWNERSHIP, RETRIEVE};
use quint_refinements::{collect_ownership_records, refine_scenario, RuntimeValue};

/// The outcome of one scenario: obligations checked, or the first divergence.
pub type Outcome = (String, Result<usize, String>);

/// Run every generated scenario against a fresh implementation, and report
/// each one. (`generated_refinement::refine_all` stops at the first divergence;
/// this keeps going so that every divergence in a run is visible.)
pub fn run_scenarios<I: Implementation>() -> Result<Vec<Outcome>, String> {
    let artifact = artifact()?;
    let ownership = collect_ownership_records(&[OWNERSHIP]).map_err(|error| error.to_string())?;
    let fixtures = I::fixtures(&artifact)?;
    fixtures
        .validate(&artifact)
        .map_err(|error| error.to_string())?;
    let mut outcomes = Vec::new();
    for scenario in &artifact.scenarios {
        let initial_json = scenario
            .initial_state
            .as_ref()
            .ok_or_else(|| format!("{} has no generated initial state", scenario.id()))?;
        let initial_state = RuntimeValue::from_itf_json(initial_json)?;
        let implementation = I::from_initial_state(&initial_state)?;
        let initial_evidence = implementation.snapshot();
        let mut driver = Driver { implementation };
        let outcome = refine_scenario(
            scenario,
            initial_evidence,
            &ownership,
            RETRIEVE,
            &fixtures,
            &mut driver,
        );
        outcomes.push((scenario.id(), outcome));
    }
    Ok(outcomes)
}
