//! Runs the real `aster affected --related` engine on repos laid out from the
//! Quint model's scenarios, so that the model and the engine are checked
//! against each other.
//!
//! * `model.qnt`, `repos.qnt`, `scenarios.qnt` are the model.
//! * `generated_refinement.rs` is generated from them by `quint-refinements`.
//! * `repo` decodes the model's repo value, `render` lays it out as Elixir
//!   files, and `engine` runs `aster` over the result.

pub mod engine;
pub mod generated_refinement;
pub mod render;
pub mod repo;
pub mod shop;

use generated_refinement::{Driver, Implementation, OWNERSHIP, RETRIEVE, artifact};
use quint_refinements::{RuntimeValue, collect_ownership_records, refine_scenario};

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
