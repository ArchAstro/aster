//! Command execution engine
//!
//! Executes target commands on projects in dependency order with parallel
//! execution per DAG level.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

pub(crate) mod command;
pub mod logs;
pub mod runner;

pub use logs::{LogStore, RunLog, TargetLog};
pub use runner::{
    collect_target_deps, compute_target_levels, parse_target_address, ExecutionResult, Executor,
};

/// Quote one argument for use in an Aster target command string.
pub fn quote_command_argument(value: &str) -> String {
    command::quote_argument(value)
}

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);
static SUPERVISED_CHILDREN: AtomicUsize = AtomicUsize::new(0);

/// Install lightweight process signal handlers.
///
/// The handler only flips an atomic flag. Each executor owns its child process
/// group and forwards termination to that group, avoiding both orphaned
/// commands and the unsafe behavior of killing Aster's parent process group.
pub fn install_signal_handler() -> &'static AtomicBool {
    SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);

    #[cfg(unix)]
    unsafe {
        libc::signal(
            libc::SIGINT,
            shutdown_handler as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            shutdown_handler as *const () as libc::sighandler_t,
        );
    }

    &SHUTDOWN_REQUESTED
}

pub(crate) fn shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::SeqCst)
}

pub(crate) fn register_supervised_child() {
    SUPERVISED_CHILDREN.fetch_add(1, Ordering::SeqCst);
}

pub(crate) fn unregister_supervised_child() {
    SUPERVISED_CHILDREN.fetch_sub(1, Ordering::SeqCst);
}

#[cfg(unix)]
extern "C" fn shutdown_handler(signal: libc::c_int) {
    if SUPERVISED_CHILDREN.load(Ordering::SeqCst) == 0 {
        unsafe {
            libc::_exit(128 + signal);
        }
    }
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}
