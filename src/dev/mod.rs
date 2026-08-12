//! Config-driven local service supervision for `aster services up`.

mod dashboard;
mod log_files;
mod plan;
mod port_allocator;
mod port_cleanup;
mod process;
mod runner;
mod tls;

pub use log_files::show_service_logs;
pub use plan::{
    resolve_dev_plan, resolve_dev_ports, resolve_static_dev_ports, DevPlan, ServicePlan,
};
pub use port_cleanup::{kill_ports, resolve_port_selection, KillPortsOptions};
pub use runner::{run_dev, DevOptions};
pub use tls::{serve_tls, setup_tls};
