//! Config-driven local service supervision for `aster services up`.

mod dashboard;
mod plan;
mod process;
mod runner;

pub use plan::{resolve_dev_plan, DevPlan, ServicePlan};
pub use runner::{run_dev, DevOptions};
