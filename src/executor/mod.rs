//! Command execution engine
//!
//! Executes target commands on projects in dependency order with parallel
//! execution per DAG level.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};

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
static SHUTDOWN_SIGNAL: AtomicI32 = AtomicI32::new(0);
static GRACEFUL_WHEN_IDLE: AtomicBool = AtomicBool::new(false);

/// Install lightweight process signal handlers.
///
/// The handler only flips an atomic flag. Each executor owns its child process
/// group and forwards termination to that group, avoiding both orphaned
/// commands and the unsafe behavior of killing Aster's parent process group.
pub fn install_signal_handler() -> &'static AtomicBool {
    SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
    SHUTDOWN_SIGNAL.store(0, Ordering::SeqCst);
    GRACEFUL_WHEN_IDLE.store(false, Ordering::SeqCst);

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
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

        let _ = SetConsoleCtrlHandler(Some(windows_shutdown_handler), 1);
    }

    &SHUTDOWN_REQUESTED
}

pub(crate) fn shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::SeqCst)
}

pub fn shutdown_signal() -> Option<i32> {
    match SHUTDOWN_SIGNAL.load(Ordering::SeqCst) {
        0 => None,
        signal => Some(signal),
    }
}

pub(crate) fn request_graceful_signal_handling() {
    GRACEFUL_WHEN_IDLE.store(true, Ordering::SeqCst);
}

pub(crate) fn request_shutdown() {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

pub(crate) fn register_supervised_child() {
    SUPERVISED_CHILDREN.fetch_add(1, Ordering::SeqCst);
}

pub(crate) fn unregister_supervised_child() {
    SUPERVISED_CHILDREN.fetch_sub(1, Ordering::SeqCst);
}

#[cfg(unix)]
extern "C" fn shutdown_handler(signal: libc::c_int) {
    if SUPERVISED_CHILDREN.load(Ordering::SeqCst) == 0 && !GRACEFUL_WHEN_IDLE.load(Ordering::SeqCst)
    {
        unsafe {
            libc::_exit(128 + signal);
        }
    }
    SHUTDOWN_SIGNAL.store(signal, Ordering::SeqCst);
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

#[cfg(windows)]
unsafe extern "system" fn windows_shutdown_handler(control_type: u32) -> i32 {
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    };

    if !matches!(
        control_type,
        CTRL_C_EVENT
            | CTRL_BREAK_EVENT
            | CTRL_CLOSE_EVENT
            | CTRL_LOGOFF_EVENT
            | CTRL_SHUTDOWN_EVENT
    ) {
        return 0;
    }
    if SUPERVISED_CHILDREN.load(Ordering::SeqCst) == 0 && !GRACEFUL_WHEN_IDLE.load(Ordering::SeqCst)
    {
        return 0;
    }
    let signal = if control_type == CTRL_C_EVENT { 2 } else { 15 };
    SHUTDOWN_SIGNAL.store(signal, Ordering::SeqCst);
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
    1
}
