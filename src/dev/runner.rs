use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyEventKind};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use walkdir::WalkDir;

use crate::cli::OutputMode;
use crate::config::WorkspaceConfig;
use crate::discovery::DiscoveredProject;
use crate::executor::{self, Executor};
use crate::graph::TargetGraph;
use crate::watch::WorkspaceIgnore;

use super::dashboard::{Dashboard, DashboardAction, ServiceState, TerminalGuard};
#[cfg(unix)]
use super::export_vars::{ExportEndpoint, ExportEvent, EXPORT_PATH_ENV};
use super::log_files::ServiceLogFiles;
use super::plan::{DevPlan, ServicePlan};
use super::process::{LogEvent, ProcessLogSenders, ServiceProcess};
#[cfg(unix)]
use super::session::{ServiceSpec, SessionClient, SessionCommand, SessionEvent, SessionServer};

pub struct DevOptions {
    pub watch: bool,
    pub ui: bool,
    pub dry_run: bool,
    pub use_cache: bool,
}

struct Runtime {
    process: Option<ServiceProcess>,
    #[cfg(unix)]
    export_endpoint: Option<ExportEndpoint>,
    generation: u64,
    applied_exports: HashMap<String, String>,
    applied_epochs: HashMap<String, u64>,
    applied_sources: HashSet<String>,
}

#[derive(Default)]
struct ProducerState {
    generation: u64,
    epoch: u64,
    values: Option<HashMap<String, String>>,
    healthy: bool,
    waiting_since: Option<Instant>,
}

#[derive(Clone, Debug, Default)]
struct ResolvedExports {
    values: HashMap<String, String>,
    epochs: HashMap<String, u64>,
    sources: HashSet<String>,
}

#[derive(Debug)]
enum ExportResolution {
    Ready(ResolvedExports),
    Pending,
    Conflict(String),
}

const FIRST_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(20);
const READY_HANDOFF_SECS: u64 = 5;
const EXPORTER_PREREQ_BUDGET_SECS: u64 = 60;
const READY_PROGRESS_SECS: u64 =
    FIRST_SNAPSHOT_TIMEOUT.as_secs() + READY_HANDOFF_SECS + EXPORTER_PREREQ_BUDGET_SECS;
const SNAPSHOT_PROGRESS_SECS: u64 = FIRST_SNAPSHOT_TIMEOUT.as_secs() + READY_HANDOFF_SECS;

enum StartOutcome {
    Running {
        process: ServiceProcess,
        #[cfg(unix)]
        export_endpoint: Option<ExportEndpoint>,
    },
    Stopped,
}

struct StartResult {
    service: String,
    outcome: StartOutcome,
    resolved_exports: ResolvedExports,
}

struct ActiveStart {
    service: String,
    generated_before: Option<HashMap<PathBuf, OutputIdentity>>,
    handle: std::thread::JoinHandle<()>,
}

pub fn run_dev(
    workspace_root: &Path,
    projects: Vec<DiscoveredProject>,
    graph: TargetGraph,
    plan: DevPlan,
    config: &WorkspaceConfig,
    options: DevOptions,
) -> Result<()> {
    print_plan(&plan);
    if options.dry_run {
        return Ok(());
    }

    let shutdown = executor::install_signal_handler();
    executor::request_graceful_signal_handling();
    let ignore = WorkspaceIgnore::build(&config.watch)?;
    let (log_tx, log_rx) = mpsc::sync_channel(4000);
    let (system_tx, system_rx) = mpsc::channel();
    let (durable_log_tx, durable_log_rx) = mpsc::channel();
    let mut durable_logs = ServiceLogFiles::open(workspace_root, &plan.services)?;
    let durable_log_handle = std::thread::spawn(move || {
        while let Ok(event) = durable_log_rx.recv() {
            durable_logs.write(&event);
        }
    });
    let control = plan.control_port.map(ControlServer::start).transpose()?;
    #[cfg(unix)]
    let session = SessionServer::from_environment(
        plan.services
            .iter()
            .map(|service| ServiceSpec {
                name: service.name.clone(),
                port: service.port,
                open_url: service.open_url.clone(),
            })
            .collect(),
    )?;
    #[cfg(unix)]
    let has_remote_ui = session.is_some();
    #[cfg(not(unix))]
    let has_remote_ui = false;
    let (watch_rx, _watcher) = if options.watch {
        let (rx, watcher) = start_watcher(&plan)?;
        (Some(rx), Some(watcher))
    } else {
        (None, None)
    };
    let mut supervisor_registered = false;
    let mut dashboard = Dashboard::new(&plan.services);
    let mut runtimes: HashMap<String, Runtime> = plan
        .services
        .iter()
        .map(|service| {
            (
                service.name.clone(),
                Runtime {
                    process: None,
                    #[cfg(unix)]
                    export_endpoint: None,
                    generation: 0,
                    applied_exports: HashMap::new(),
                    applied_epochs: HashMap::new(),
                    applied_sources: HashSet::new(),
                },
            )
        })
        .collect();
    let mut producers = plan
        .services
        .iter()
        .filter(|service| service.target.exports_vars())
        .map(|service| (service.name.clone(), ProducerState::default()))
        .collect::<HashMap<_, _>>();
    #[cfg(unix)]
    let (export_tx, export_rx) = mpsc::channel::<ExportEvent>();
    let projects = Arc::new(projects);
    let (start_tx, start_rx) = mpsc::channel::<StartResult>();
    let mut active_start: Option<ActiveStart> = None;
    let mut pending_starts = plan
        .services
        .iter()
        .map(|service| (service.name.clone(), "initial start".to_string()))
        .collect::<VecDeque<_>>();

    let workspace_header = workspace_header(workspace_root);
    let mut terminal = options.ui.then(TerminalGuard::enter).transpose()?;
    let mut needs_draw = true;
    // Events queued while initial prerequisites start are processed after this
    // point. Suppress only configured generated paths; genuine source edits
    // remain eligible for a follow-up restart.
    let mut suppress_until = Instant::now() + Duration::from_millis(700);
    let mut generated_outputs = GeneratedOutputIdentities::default();
    let watch_debounce = Duration::from_millis(config.watch.debounce_ms.unwrap_or(300));
    let mut pending_watch_paths = Vec::new();
    let mut watch_deadline: Option<Instant> = None;
    let mut quitting = false;

    if producers.is_empty() {
        register_ready(workspace_root, &plan)?;
        supervisor_registered = true;
    }

    let run_result = (|| -> Result<()> {
        while !quitting
            && !shutdown.load(Ordering::SeqCst)
            && !control
                .as_ref()
                .is_some_and(|control| control.shutdown.load(Ordering::SeqCst))
        {
            needs_draw |= drain_logs(
                &log_rx,
                &system_rx,
                &durable_log_tx,
                &mut dashboard,
                options.ui,
                #[cfg(unix)]
                session.as_ref(),
            );

            while let Ok(result) = start_rx.try_recv() {
                if let Some(active) = active_start.take() {
                    debug_assert_eq!(active.service, result.service);
                    let _ = active.handle.join();
                    if let Some(before_start) = active.generated_before {
                        generated_outputs.record_dispatch(
                            before_start,
                            suppressed_path_snapshot(&plan, workspace_root, &ignore),
                        );
                    }
                }
                match result.outcome {
                    StartOutcome::Running {
                        mut process,
                        #[cfg(unix)]
                        export_endpoint,
                    } => {
                        let service = plan
                            .services
                            .iter()
                            .find(|service| service.name == result.service)
                            .expect("start result service exists");
                        let current = resolve_exported_environment(service, &producers);
                        let keep_running = match &current {
                            ExportResolution::Ready(resolved) => {
                                resolved.values == result.resolved_exports.values
                            }
                            ExportResolution::Pending | ExportResolution::Conflict(_) => false,
                        };
                        if !keep_running {
                            process.terminate(Duration::from_secs(3));
                            #[cfg(unix)]
                            drop(export_endpoint);
                            if let ExportResolution::Conflict(message) = current {
                                emit_system(&system_tx, &result.service, message, true);
                            }
                            queue_restart(
                                &mut pending_starts,
                                &result.service,
                                "exported variables changed during start",
                                &system_tx,
                                &mut dashboard,
                            );
                        } else {
                            let ExportResolution::Ready(current) = current else {
                                unreachable!("keep_running requires a ready export resolution");
                            };
                            let runtime = runtimes
                                .get_mut(&result.service)
                                .expect("start result service exists");
                            runtime.process = Some(process);
                            #[cfg(unix)]
                            {
                                runtime.export_endpoint = export_endpoint;
                            }
                            runtime.applied_exports = result.resolved_exports.values;
                            runtime.applied_sources = result.resolved_exports.sources;
                            runtime.applied_epochs = current.epochs;
                            if let Some(state) = producers.get_mut(&result.service) {
                                if state.values.is_none() {
                                    state.waiting_since = Some(Instant::now());
                                }
                            }
                            if !supervisor_registered && producers.contains_key(&result.service) {
                                extend_ready_deadline(
                                    workspace_root,
                                    Duration::from_secs(SNAPSHOT_PROGRESS_SECS),
                                );
                            }
                            set_service_state(
                                &mut dashboard,
                                #[cfg(unix)]
                                session.as_ref(),
                                &result.service,
                                ServiceState::Running,
                            );
                        }
                    }
                    StartOutcome::Stopped => {
                        if producers.contains_key(&result.service) {
                            if !supervisor_registered {
                                anyhow::bail!(
                                    "variable exporter '{}' exited before publishing its initial snapshot",
                                    result.service
                                );
                            }
                            stop_dependent_services(
                                &result.service,
                                &plan,
                                &mut runtimes,
                                &mut pending_starts,
                                &system_tx,
                                &mut dashboard,
                                #[cfg(unix)]
                                session.as_ref(),
                            );
                        }
                        set_service_state(
                            &mut dashboard,
                            #[cfg(unix)]
                            session.as_ref(),
                            &result.service,
                            ServiceState::Stopped,
                        );
                    }
                }
                suppress_until = Instant::now() + Duration::from_millis(700);
                needs_draw = true;
            }

            #[cfg(unix)]
            while let Ok(event) = export_rx.try_recv() {
                match event {
                    ExportEvent::Snapshot {
                        producer,
                        generation,
                        values,
                    } => {
                        let Some(state) = producers.get_mut(&producer) else {
                            continue;
                        };
                        if state.generation != generation
                            || runtimes[&producer].generation != generation
                        {
                            continue;
                        }
                        state.epoch = state.epoch.saturating_add(1);
                        state.values = Some(values);
                        state.healthy = true;
                        state.waiting_since = None;
                        emit_system(
                            &system_tx,
                            &producer,
                            format!("accepted exported variable snapshot epoch {}", state.epoch),
                            false,
                        );
                        for service in &plan.services {
                            if service.name == producer || !service.dependencies.contains(&producer)
                            {
                                continue;
                            }
                            match resolve_exported_environment(service, &producers) {
                                ExportResolution::Ready(resolved)
                                    if runtimes[&service.name].process.is_some()
                                        && resolved.values
                                            == runtimes[&service.name].applied_exports =>
                                {
                                    let runtime =
                                        runtimes.get_mut(&service.name).expect("runtime exists");
                                    runtime.applied_epochs = resolved.epochs;
                                    runtime.applied_sources = resolved.sources;
                                }
                                ExportResolution::Ready(_)
                                    if runtimes[&service.name].process.is_some() =>
                                {
                                    queue_restart(
                                        &mut pending_starts,
                                        &service.name,
                                        "effective exported environment changed",
                                        &system_tx,
                                        &mut dashboard,
                                    );
                                }
                                ExportResolution::Ready(_) | ExportResolution::Pending => {}
                                ExportResolution::Conflict(message) => {
                                    emit_system(&system_tx, &service.name, message, true);
                                    if runtimes[&service.name].process.is_some() {
                                        hold_service(
                                            &service.name,
                                            "exported variable sources conflict",
                                            &mut runtimes,
                                            &mut pending_starts,
                                            &system_tx,
                                            &mut dashboard,
                                            #[cfg(unix)]
                                            session.as_ref(),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    ExportEvent::Invalid {
                        producer,
                        generation,
                        reason,
                    } => {
                        let Some(state) = producers.get_mut(&producer) else {
                            continue;
                        };
                        if state.generation != generation {
                            continue;
                        }
                        state.epoch = state.epoch.saturating_add(1);
                        state.healthy = false;
                        emit_system(
                            &system_tx,
                            &producer,
                            format!("rejected exported variable update: {reason}"),
                            true,
                        );
                        stop_dependent_services(
                            &producer,
                            &plan,
                            &mut runtimes,
                            &mut pending_starts,
                            &system_tx,
                            &mut dashboard,
                            #[cfg(unix)]
                            session.as_ref(),
                        );
                    }
                }
                needs_draw = true;
            }

            if !supervisor_registered
                && producers
                    .values()
                    .all(|producer| producer.healthy && producer.values.is_some())
            {
                register_ready(workspace_root, &plan)?;
                supervisor_registered = true;
            }

            if !supervisor_registered {
                if let Some((name, _)) = producers.iter().find(|(_, producer)| {
                    producer.waiting_since.is_some_and(|started| {
                        started.elapsed() >= FIRST_SNAPSHOT_TIMEOUT && producer.values.is_none()
                    })
                }) {
                    anyhow::bail!(
                        "variable exporter '{name}' did not publish an initial snapshot within 20 seconds"
                    );
                }
            }
            if active_start.is_none() {
                let mut startable = None;
                for (index, (name, _)) in pending_starts.iter().enumerate() {
                    let service = plan
                        .services
                        .iter()
                        .find(|service| service.name == *name)
                        .expect("queued service exists");
                    if service_is_startable(service, &runtimes, &producers) {
                        startable = Some(index);
                        break;
                    }
                }
                if let Some(index) = startable {
                    let (name, reason) = pending_starts
                        .remove(index)
                        .expect("startable queue position exists");
                    if reason != "initial start"
                        && producers
                            .get(&name)
                            .is_some_and(|producer| producer.values.is_some())
                    {
                        stop_dependent_services(
                            &name,
                            &plan,
                            &mut runtimes,
                            &mut pending_starts,
                            &system_tx,
                            &mut dashboard,
                            #[cfg(unix)]
                            session.as_ref(),
                        );
                    }
                    let service = plan
                        .services
                        .iter()
                        .find(|service| service.name == name)
                        .expect("queued service exists");
                    let before_start = options
                        .watch
                        .then(|| suppressed_path_snapshot(&plan, workspace_root, &ignore));
                    let resolved_exports = match resolve_exported_environment(service, &producers) {
                        ExportResolution::Ready(resolved) => resolved,
                        ExportResolution::Conflict(message) => {
                            emit_system(&system_tx, &name, message, true);
                            pending_starts.push_front((name, reason));
                            continue;
                        }
                        ExportResolution::Pending => {
                            pending_starts.push_front((name, reason));
                            continue;
                        }
                    };
                    if !supervisor_registered {
                        extend_ready_deadline(
                            workspace_root,
                            Duration::from_secs(READY_PROGRESS_SECS),
                        );
                    }
                    let handle = begin_start_service(
                        service,
                        workspace_root,
                        projects.clone(),
                        &graph,
                        options.use_cache,
                        options.ui || has_remote_ui,
                        &log_tx,
                        &system_tx,
                        &durable_log_tx,
                        &mut dashboard,
                        #[cfg(unix)]
                        session.as_ref(),
                        runtimes.get_mut(&name).expect("runtime exists"),
                        &reason,
                        resolved_exports,
                        #[cfg(unix)]
                        export_tx.clone(),
                        producers.get_mut(&name),
                        start_tx.clone(),
                    );
                    active_start = Some(ActiveStart {
                        service: name,
                        generated_before: before_start,
                        handle,
                    });
                    needs_draw = true;
                }
            }

            for service in &plan.services {
                let exited = runtimes
                    .get_mut(&service.name)
                    .expect("runtime exists")
                    .process
                    .as_mut()
                    .and_then(|process| process.poll().ok().flatten());
                if let Some(code) = exited {
                    let runtime = runtimes.get_mut(&service.name).expect("runtime exists");
                    runtime.process.take();
                    #[cfg(unix)]
                    runtime.export_endpoint.take();
                    set_service_state(
                        &mut dashboard,
                        #[cfg(unix)]
                        session.as_ref(),
                        &service.name,
                        ServiceState::Stopped,
                    );
                    emit_system(
                        &system_tx,
                        &service.name,
                        format!("process exited with code {code}"),
                        true,
                    );
                    if let Some(producer) = producers.get_mut(&service.name) {
                        let had_snapshot = producer.values.is_some();
                        producer.epoch = producer.epoch.saturating_add(1);
                        producer.values = None;
                        producer.healthy = false;
                        producer.waiting_since = None;
                        if !had_snapshot && !supervisor_registered {
                            anyhow::bail!(
                                "variable exporter '{}' exited before publishing its initial snapshot",
                                service.name
                            );
                        }
                        stop_dependent_services(
                            &service.name,
                            &plan,
                            &mut runtimes,
                            &mut pending_starts,
                            &system_tx,
                            &mut dashboard,
                            #[cfg(unix)]
                            session.as_ref(),
                        );
                    }
                    needs_draw = true;
                }
            }

            #[cfg(unix)]
            if let Some(session) = session.as_ref() {
                while let Some(command) = session.try_command() {
                    match command {
                        SessionCommand::Restart { service } => {
                            if plan.services.iter().any(|item| item.name == service) {
                                queue_restart(
                                    &mut pending_starts,
                                    &service,
                                    "attached dashboard restart",
                                    &system_tx,
                                    &mut dashboard,
                                );
                                set_service_state(
                                    &mut dashboard,
                                    Some(session),
                                    &service,
                                    ServiceState::Restarting,
                                );
                                needs_draw = true;
                            }
                        }
                    }
                }
            }

            if let Some(rx) = &watch_rx {
                while let Ok(event) = rx.try_recv() {
                    match event {
                        Ok(event) if is_meaningful_event(&event.kind) => {
                            pending_watch_paths.extend(event.paths);
                            watch_deadline.get_or_insert_with(|| Instant::now() + watch_debounce);
                        }
                        Ok(_) => {}
                        Err(error) => eprintln!("[services] watcher error: {error}"),
                    }
                }
                if watch_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    let changed = std::mem::take(&mut pending_watch_paths);
                    watch_deadline = None;
                    let affected = affected_services(
                        &plan,
                        &graph,
                        workspace_root,
                        &ignore,
                        &changed,
                        active_start.is_some() || Instant::now() < suppress_until,
                        &mut generated_outputs,
                    );
                    for name in affected {
                        if plan.services.iter().any(|service| service.name == name) {
                            queue_restart(
                                &mut pending_starts,
                                &name,
                                "watched dependency changed",
                                &system_tx,
                                &mut dashboard,
                            );
                            suppress_until = Instant::now() + Duration::from_millis(700);
                            needs_draw = true;
                        }
                    }
                }
            }

            if let Some(rx) = control.as_ref().map(|control| &control.rx) {
                while let Ok(request) = rx.try_recv() {
                    let response = match request.command.as_str() {
                        "status" => {
                            let services = plan
                                .services
                                .iter()
                                .map(|service| {
                                    let running = runtimes
                                        .get(&service.name)
                                        .and_then(|runtime| runtime.process.as_ref())
                                        .is_some();
                                    (
                                        service.name.clone(),
                                        serde_json::Value::String(
                                            if running { "running" } else { "stopped" }.to_string(),
                                        ),
                                    )
                                })
                                .collect::<serde_json::Map<_, _>>();
                            serde_json::json!({"ok": true, "services": services})
                        }
                        "list_services" => serde_json::json!({
                            "ok": true,
                            "services": plan.services.iter().map(|service| &service.name).collect::<Vec<_>>()
                        }),
                        "restart" => match request.service.as_deref() {
                            Some(name) => {
                                match plan.services.iter().find(|service| service.name == name) {
                                    Some(_) => {
                                        queue_restart(
                                            &mut pending_starts,
                                            name,
                                            "control socket restart",
                                            &system_tx,
                                            &mut dashboard,
                                        );
                                        needs_draw = true;
                                        serde_json::json!({"ok": true, "queued": true})
                                    }
                                    None => serde_json::json!({
                                        "ok": false,
                                        "error": format!("unknown service: {name}")
                                    }),
                                }
                            }
                            None => serde_json::json!({
                                "ok": false,
                                "error": "restart requires 'service' field"
                            }),
                        },
                        "restart_all" => {
                            for service in &plan.services {
                                if shutdown.load(Ordering::SeqCst) {
                                    break;
                                }
                                queue_restart(
                                    &mut pending_starts,
                                    &service.name,
                                    "control socket restart_all",
                                    &system_tx,
                                    &mut dashboard,
                                );
                            }
                            needs_draw = true;
                            serde_json::json!({"ok": true, "queued": true})
                        }
                        "shutdown" => {
                            quitting = true;
                            serde_json::json!({"ok": true})
                        }
                        other => serde_json::json!({
                            "ok": false,
                            "error": format!("unknown command: {other}")
                        }),
                    };
                    let _ = request.reply.send(response);
                }
            }

            if let Some(guard) = terminal.as_mut() {
                if needs_draw {
                    dashboard.draw(
                        &mut guard.terminal,
                        &workspace_header,
                        control
                            .as_ref()
                            .and_then(|control| control.token_path.to_str()),
                    )?;
                    needs_draw = false;
                }
                if event::poll(Duration::from_millis(75))? {
                    match event::read()? {
                        Event::Key(key)
                            if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                        {
                            match dashboard.handle_key(key) {
                                DashboardAction::Quit => break,
                                DashboardAction::Restart(name) => {
                                    queue_restart(
                                        &mut pending_starts,
                                        &name,
                                        "manual restart",
                                        &system_tx,
                                        &mut dashboard,
                                    );
                                    suppress_until = Instant::now() + Duration::from_millis(700);
                                    needs_draw = true;
                                }
                                DashboardAction::Open => {
                                    if let Some(url) = dashboard.active_url() {
                                        if let Err(error) = open_url(url) {
                                            let active = dashboard.active_name().to_string();
                                            dashboard.push_system(
                                                &active,
                                                format!("failed to open browser: {error}"),
                                            );
                                        }
                                    }
                                    needs_draw = true;
                                }
                                DashboardAction::ToggleMouse(enabled) => {
                                    guard.set_mouse_capture(enabled)?;
                                    needs_draw = true;
                                }
                                DashboardAction::Draw => needs_draw = true,
                                DashboardAction::None => {}
                            }
                        }
                        Event::Mouse(mouse) => {
                            let size = guard.terminal.size()?;
                            let control_token_path = control
                                .as_ref()
                                .and_then(|control| control.token_path.to_str());
                            match dashboard.handle_mouse(
                                mouse,
                                ratatui::layout::Rect::new(0, 0, size.width, size.height),
                                control_token_path,
                            ) {
                                DashboardAction::Open => {
                                    if let Some(url) = dashboard.active_url() {
                                        if let Err(error) = open_url(url) {
                                            let active = dashboard.active_name().to_string();
                                            dashboard.push_system(
                                                &active,
                                                format!("failed to open browser: {error}"),
                                            );
                                        }
                                    }
                                    needs_draw = true;
                                }
                                DashboardAction::Draw => needs_draw = true,
                                DashboardAction::ToggleMouse(enabled) => {
                                    guard.set_mouse_capture(enabled)?;
                                    needs_draw = true;
                                }
                                DashboardAction::Restart(name) => {
                                    queue_restart(
                                        &mut pending_starts,
                                        &name,
                                        "manual restart",
                                        &system_tx,
                                        &mut dashboard,
                                    );
                                    suppress_until = Instant::now() + Duration::from_millis(700);
                                    needs_draw = true;
                                }
                                DashboardAction::Quit => quitting = true,
                                DashboardAction::None => {}
                            }
                        }
                        Event::Resize(_, _) => needs_draw = true,
                        _ => {}
                    }
                }
            } else {
                std::thread::sleep(Duration::from_millis(75));
            }
        }
        Ok(())
    })();

    drop(terminal);
    eprintln!("[services] shutting down services...");
    executor::request_shutdown();
    pending_starts.clear();
    if let Some(active) = active_start.take() {
        let _ = active.handle.join();
    }
    let mut shutdown_processes = Vec::new();
    while let Ok(result) = start_rx.try_recv() {
        if let StartOutcome::Running {
            process,
            #[cfg(unix)]
            export_endpoint,
        } = result.outcome
        {
            #[cfg(unix)]
            drop(export_endpoint);
            shutdown_processes.push(process);
        }
    }
    for service in plan.services.iter().rev() {
        #[cfg(unix)]
        runtimes
            .get_mut(&service.name)
            .and_then(|runtime| runtime.export_endpoint.take());
        if let Some(process) = runtimes
            .get_mut(&service.name)
            .and_then(|runtime| runtime.process.take())
        {
            shutdown_processes.push(process);
        }
    }
    let shutdown_deadline = Instant::now() + Duration::from_secs(3);
    for process in &mut shutdown_processes {
        process.request_terminate();
    }
    for process in &mut shutdown_processes {
        process.finish_terminate(shutdown_deadline);
    }
    drain_logs(
        &log_rx,
        &system_rx,
        &durable_log_tx,
        &mut dashboard,
        false,
        #[cfg(unix)]
        session.as_ref(),
    );
    drop(durable_log_tx);
    let _ = durable_log_handle.join();
    drop(control);
    run_result?;
    Ok(())
}

fn register_ready(workspace_root: &Path, plan: &DevPlan) -> Result<()> {
    super::daemon::register_supervisor_ready(
        workspace_root,
        plan.services
            .iter()
            .map(|service| service.name.clone())
            .collect(),
        plan.ports
            .iter()
            .map(|(name, port)| (name.clone(), *port))
            .collect(),
    )?;
    Ok(())
}

fn extend_ready_deadline(workspace_root: &Path, extra: Duration) {
    let _ = super::daemon::register_supervisor_extend_deadline(workspace_root, extra);
}

fn resolve_exported_environment(
    service: &ServicePlan,
    producers: &HashMap<String, ProducerState>,
) -> ExportResolution {
    let mut environment = HashMap::new();
    let mut epochs = HashMap::new();
    let mut sources = HashMap::<String, String>::new();
    let mut used_sources = HashSet::new();
    for dependency in &service.dependencies {
        let Some(producer) = producers.get(dependency) else {
            continue;
        };
        if !producer.healthy {
            return ExportResolution::Pending;
        }
        let Some(values) = producer.values.as_ref() else {
            return ExportResolution::Pending;
        };
        for name in &service.inherit_env {
            if service.protected_env.contains(name) {
                continue;
            }
            let Some(value) = values.get(name) else {
                continue;
            };
            if let Some(existing) = sources.insert(name.clone(), dependency.clone()) {
                return ExportResolution::Conflict(format!(
                    "service '{}' consumes exported variable '{name}' from both '{existing}' and '{dependency}'",
                    service.name
                ));
            }
            environment.insert(name.clone(), value.clone());
            used_sources.insert(dependency.clone());
        }
        epochs.insert(dependency.clone(), producer.epoch);
    }

    ExportResolution::Ready(ResolvedExports {
        values: environment,
        epochs,
        sources: used_sources,
    })
}

fn service_is_startable(
    service: &ServicePlan,
    runtimes: &HashMap<String, Runtime>,
    producers: &HashMap<String, ProducerState>,
) -> bool {
    if service.dependencies.iter().any(|dependency| {
        runtimes
            .get(dependency)
            .is_none_or(|runtime| runtime.process.is_none())
    }) {
        return false;
    }
    matches!(
        resolve_exported_environment(service, producers),
        ExportResolution::Ready(_)
    )
}

#[allow(clippy::too_many_arguments)]
fn hold_service(
    name: &str,
    reason: &str,
    runtimes: &mut HashMap<String, Runtime>,
    pending: &mut VecDeque<(String, String)>,
    system_tx: &std::sync::mpsc::Sender<LogEvent>,
    dashboard: &mut Dashboard,
    #[cfg(unix)] session: Option<&SessionServer>,
) {
    let runtime = runtimes.get_mut(name).expect("runtime exists");
    #[cfg(unix)]
    runtime.export_endpoint.take();
    if let Some(mut process) = runtime.process.take() {
        process.terminate(Duration::from_secs(3));
    }
    runtime.applied_exports.clear();
    runtime.applied_epochs.clear();
    runtime.applied_sources.clear();
    set_service_state(
        dashboard,
        #[cfg(unix)]
        session,
        name,
        ServiceState::Restarting,
    );
    queue_restart(pending, name, reason, system_tx, dashboard);
}

#[allow(clippy::too_many_arguments)]
fn stop_dependent_services(
    producer: &str,
    plan: &DevPlan,
    runtimes: &mut HashMap<String, Runtime>,
    pending: &mut VecDeque<(String, String)>,
    system_tx: &std::sync::mpsc::Sender<LogEvent>,
    dashboard: &mut Dashboard,
    #[cfg(unix)] session: Option<&SessionServer>,
) {
    let mut affected = HashSet::new();
    let mut frontier = vec![producer.to_string()];
    while let Some(current) = frontier.pop() {
        for service in &plan.services {
            if affected.contains(&service.name)
                || !runtimes[&service.name].applied_sources.contains(&current)
            {
                continue;
            }
            affected.insert(service.name.clone());
            if service.target.exports_vars() {
                frontier.push(service.name.clone());
            }
        }
    }

    for service in plan.services.iter().rev() {
        if !affected.contains(&service.name) {
            continue;
        }
        hold_service(
            &service.name,
            "required exported variables unavailable",
            runtimes,
            pending,
            system_tx,
            dashboard,
            #[cfg(unix)]
            session,
        );
    }
}

fn set_service_state(
    dashboard: &mut Dashboard,
    #[cfg(unix)] session: Option<&SessionServer>,
    service: &str,
    state: ServiceState,
) {
    dashboard.set_state(service, state);
    #[cfg(unix)]
    if let Some(session) = session {
        session.publish(SessionEvent::State {
            service: service.to_string(),
            state,
        });
    }
}

#[cfg(unix)]
pub fn attach_dashboard(workspace_root: &Path, socket: &Path) -> Result<()> {
    executor::request_graceful_signal_handling();
    let mut client = SessionClient::connect(socket)?;
    let services = loop {
        match client.events.recv_timeout(Duration::from_secs(5)) {
            Ok(SessionEvent::Snapshot { services }) => break services,
            Ok(_) => continue,
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "supervisor did not provide a dashboard snapshot: {error}"
                ))
            }
        }
    };
    if services.is_empty() {
        return Err(anyhow::anyhow!(
            "service supervisor reported an empty dashboard"
        ));
    }
    let mut dashboard = Dashboard::from_specs(
        services
            .into_iter()
            .map(|service| (service.name, service.port, service.open_url))
            .collect(),
    );
    let mut guard = TerminalGuard::enter()?;
    let header = workspace_header(workspace_root);
    let mut needs_draw = true;
    loop {
        if executor::shutdown_requested() {
            break;
        }
        loop {
            match client.events.try_recv() {
                Ok(SessionEvent::Snapshot { .. }) => {}
                Ok(SessionEvent::State { service, state }) => dashboard.set_state(&service, state),
                Ok(SessionEvent::Log(event)) => dashboard.push_log(event),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(anyhow::anyhow!("service supervisor disconnected"));
                }
            }
            needs_draw = true;
        }
        if needs_draw {
            dashboard.draw(&mut guard.terminal, &header, None)?;
            needs_draw = false;
        }
        if event::poll(Duration::from_millis(75))? {
            match event::read()? {
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    match dashboard.handle_key(key) {
                        DashboardAction::Quit => break,
                        DashboardAction::Restart(service) => {
                            client.send(&SessionCommand::Restart { service })?;
                            needs_draw = true;
                        }
                        DashboardAction::Open => {
                            if let Some(url) = dashboard.active_url() {
                                if let Err(error) = open_url(url) {
                                    let active = dashboard.active_name().to_string();
                                    dashboard.push_system(
                                        &active,
                                        format!("failed to open browser: {error}"),
                                    );
                                }
                            }
                            needs_draw = true;
                        }
                        DashboardAction::ToggleMouse(enabled) => {
                            guard.set_mouse_capture(enabled)?;
                            needs_draw = true;
                        }
                        DashboardAction::Draw => needs_draw = true,
                        DashboardAction::None => {}
                    }
                }
                Event::Resize(_, _) => needs_draw = true,
                _ => {}
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn begin_start_service(
    service: &ServicePlan,
    workspace_root: &Path,
    projects: Arc<Vec<DiscoveredProject>>,
    graph: &TargetGraph,
    use_cache: bool,
    ui: bool,
    log_tx: &std::sync::mpsc::SyncSender<LogEvent>,
    system_tx: &std::sync::mpsc::Sender<LogEvent>,
    durable_log_tx: &std::sync::mpsc::Sender<LogEvent>,
    dashboard: &mut Dashboard,
    #[cfg(unix)] session: Option<&SessionServer>,
    runtime: &mut Runtime,
    reason: &str,
    resolved_exports: ResolvedExports,
    #[cfg(unix)] export_tx: std::sync::mpsc::Sender<ExportEvent>,
    producer_state: Option<&mut ProducerState>,
    result_tx: std::sync::mpsc::Sender<StartResult>,
) -> std::thread::JoinHandle<()> {
    #[cfg(unix)]
    runtime.export_endpoint.take();
    if let Some(mut process) = runtime.process.take() {
        process.terminate(Duration::from_secs(3));
    }
    runtime.generation = runtime.generation.saturating_add(1);
    let generation = runtime.generation;
    if let Some(producer) = producer_state {
        producer.generation = generation;
        producer.epoch = producer.epoch.saturating_add(1);
        producer.values = None;
        producer.healthy = false;
        producer.waiting_since = None;
    }
    let state = if reason == "initial start" {
        ServiceState::Starting
    } else {
        ServiceState::Restarting
    };
    set_service_state(
        dashboard,
        #[cfg(unix)]
        session,
        &service.name,
        state,
    );
    if reason != "initial start" {
        emit_system(
            system_tx,
            &service.name,
            format!("restart requested: {reason}"),
            false,
        );
    }
    emit_system(
        system_tx,
        &service.name,
        format!("running prerequisites for {}", service.target_address),
        false,
    );
    let mut prerequisites = HashSet::new();
    collect_non_stream_dependencies(
        &service.target_address,
        graph,
        &service.watch,
        &mut prerequisites,
    );
    let service_name = service.name.clone();
    let target_address = service.target_address.clone();
    let target = service.target.clone();
    let project_root = service.project_root.clone();
    let mut env = service.env.clone();
    env.extend(resolved_exports.values.clone());
    let workspace_root = workspace_root.to_path_buf();
    let log_tx = log_tx.clone();
    let system_tx = system_tx.clone();
    let durable_log_tx = durable_log_tx.clone();
    std::thread::spawn(move || {
        let prerequisite_run =
            run_prerequisites(&prerequisites, &workspace_root, &projects, use_cache);
        for (stderr, line) in prerequisite_run.lines {
            let _ = system_tx.send(LogEvent {
                service: service_name.clone(),
                line,
                stderr,
            });
        }
        if !prerequisite_run.failures.is_empty() {
            emit_system(
                &system_tx,
                &service_name,
                format!(
                    "prerequisite failed: {}",
                    prerequisite_run.failures.join(", ")
                ),
                true,
            );
            let _ = result_tx.send(StartResult {
                service: service_name,
                outcome: StartOutcome::Stopped,
                resolved_exports,
            });
            return;
        }
        if executor::shutdown_requested() {
            emit_system(
                &system_tx,
                &service_name,
                "shutdown requested; service start skipped".to_string(),
                false,
            );
            let _ = result_tx.send(StartResult {
                service: service_name,
                outcome: StartOutcome::Stopped,
                resolved_exports,
            });
            return;
        }
        emit_system(
            &system_tx,
            &service_name,
            format!("starting {target_address}"),
            false,
        );
        #[cfg(unix)]
        let export_endpoint = if target.exports_vars() {
            match ExportEndpoint::create(&service_name, generation, export_tx) {
                Ok(endpoint) => {
                    env.insert(
                        EXPORT_PATH_ENV.to_string(),
                        endpoint.path().to_string_lossy().into_owned(),
                    );
                    Some(endpoint)
                }
                Err(error) => {
                    emit_system(
                        &system_tx,
                        &service_name,
                        format!("failed to create variable export endpoint: {error:#}"),
                        true,
                    );
                    let _ = result_tx.send(StartResult {
                        service: service_name,
                        outcome: StartOutcome::Stopped,
                        resolved_exports,
                    });
                    return;
                }
            }
        } else {
            None
        };
        let outcome = match ServiceProcess::spawn(
            &service_name,
            &target,
            &project_root,
            &env,
            ProcessLogSenders::new(log_tx, system_tx.clone(), durable_log_tx, ui),
        ) {
            Ok(process) => StartOutcome::Running {
                process,
                #[cfg(unix)]
                export_endpoint,
            },
            Err(error) => {
                emit_system(
                    &system_tx,
                    &service_name,
                    format!("start failed: {error:#}"),
                    true,
                );
                StartOutcome::Stopped
            }
        };
        let _ = result_tx.send(StartResult {
            service: service_name,
            outcome,
            resolved_exports,
        });
    })
}

fn queue_restart(
    pending: &mut VecDeque<(String, String)>,
    service: &str,
    reason: &str,
    system_tx: &std::sync::mpsc::Sender<LogEvent>,
    dashboard: &mut Dashboard,
) {
    if let Some((_, queued_reason)) = pending.iter_mut().find(|(name, _)| name == service) {
        *queued_reason = reason.to_string();
        return;
    }
    pending.push_back((service.to_string(), reason.to_string()));
    dashboard.set_state(service, ServiceState::Restarting);
    emit_system(
        system_tx,
        service,
        format!("restart queued: {reason}"),
        false,
    );
}

struct PrerequisiteRun {
    lines: Vec<(bool, String)>,
    failures: Vec<String>,
}

fn run_prerequisites(
    prerequisites: &HashSet<String>,
    workspace_root: &Path,
    projects: &[DiscoveredProject],
    use_cache: bool,
) -> PrerequisiteRun {
    if prerequisites.is_empty() {
        return PrerequisiteRun {
            lines: Vec::new(),
            failures: Vec::new(),
        };
    }

    let refs = projects.iter().collect::<Vec<_>>();
    let results = Executor::with_all_options(workspace_root, OutputMode::Quiet, true, use_cache)
        .with_null_stdin()
        .execute_targets(prerequisites, &refs, true);
    let mut lines = Vec::new();
    for result in &results {
        let stderr = !result.success;
        lines.push((
            stderr,
            format!(
                "[{}] {}",
                if result.cached {
                    "cached"
                } else if result.success {
                    "ok"
                } else {
                    "failed"
                },
                result.address
            ),
        ));
        lines.extend(
            result
                .output
                .lines()
                .map(|line| (stderr, format!("[{}] {line}", result.address))),
        );
    }
    let failures = results
        .iter()
        .filter(|result| !result.success && !result.skipped)
        .map(|result| result.address.clone())
        .collect::<Vec<_>>();
    PrerequisiteRun { lines, failures }
}

fn collect_non_stream_dependencies(
    address: &str,
    graph: &TargetGraph,
    plan: &crate::watch::WatchPlan,
    output: &mut HashSet<String>,
) {
    for dependency in graph.dependencies(address) {
        let is_stream = plan
            .targets
            .iter()
            .any(|target| target.address == dependency.address && target.stream);
        let should_recurse = is_stream || output.insert(dependency.address.clone());
        if should_recurse {
            collect_non_stream_dependencies(&dependency.address, graph, plan, output);
        }
    }
}

fn start_watcher(
    plan: &DevPlan,
) -> Result<(Receiver<notify::Result<notify::Event>>, RecommendedWatcher)> {
    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = tx.send(event);
    })
    .context("failed to create services file watcher")?;
    let mut roots = plan
        .services
        .iter()
        .flat_map(|service| service.watch.watch_roots.iter().cloned())
        .collect::<Vec<_>>();
    roots.sort();
    roots.dedup();
    for root in roots {
        watcher
            .watch(&root, RecursiveMode::Recursive)
            .with_context(|| format!("failed to watch {}", root.display()))?;
    }
    Ok((rx, watcher))
}

fn affected_services(
    plan: &DevPlan,
    graph: &TargetGraph,
    workspace_root: &Path,
    ignore: &WorkspaceIgnore,
    paths: &[PathBuf],
    suppress_generated: bool,
    generated_outputs: &mut GeneratedOutputIdentities,
) -> Vec<String> {
    let mut affected = HashSet::new();
    for service in &plan.services {
        for path in paths {
            let relative = path.strip_prefix(workspace_root).unwrap_or(path);
            if ignore.is_ignored(relative) {
                continue;
            }
            if ignore.is_suppressed(relative)
                && should_suppress_generated_path(path, suppress_generated, generated_outputs)
            {
                continue;
            }
            let owners = service.watch.owners_of(path);
            let primary = service.watch.primary_set(&owners, graph);
            if primary.contains(&service.target_address) {
                affected.insert(service.name.clone());
            }
        }
    }
    let mut affected = affected.into_iter().collect::<Vec<_>>();
    affected.sort_by_key(|name| {
        plan.services
            .iter()
            .position(|service| &service.name == name)
            .unwrap_or(usize::MAX)
    });
    affected
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum OutputIdentity {
    Missing,
    File {
        len: u64,
        modified: Option<std::time::SystemTime>,
    },
}

#[derive(Default)]
struct GeneratedOutputIdentities {
    by_path: HashMap<PathBuf, OutputIdentity>,
}

impl GeneratedOutputIdentities {
    fn record_dispatch(
        &mut self,
        before: HashMap<PathBuf, OutputIdentity>,
        after: HashMap<PathBuf, OutputIdentity>,
    ) {
        let paths: HashSet<PathBuf> = before.keys().chain(after.keys()).cloned().collect();
        for path in paths {
            let old = before
                .get(&path)
                .cloned()
                .unwrap_or(OutputIdentity::Missing);
            let new = after.get(&path).cloned().unwrap_or(OutputIdentity::Missing);
            if old != new {
                self.by_path.insert(path, new);
            }
        }
    }

    fn matches_or_forget(&mut self, path: &Path) -> bool {
        let Some(expected) = self.by_path.get(path) else {
            return false;
        };
        if *expected == output_identity(path) {
            return true;
        }
        self.by_path.remove(path);
        false
    }
}

fn suppressed_path_snapshot(
    plan: &DevPlan,
    workspace_root: &Path,
    ignore: &WorkspaceIgnore,
) -> HashMap<PathBuf, OutputIdentity> {
    let mut snapshot = HashMap::new();
    let mut roots = plan
        .services
        .iter()
        .flat_map(|service| service.watch.watch_roots.iter().cloned())
        .collect::<Vec<_>>();
    roots.sort();
    roots.dedup();
    for root in roots {
        let entries = WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| {
                let relative = entry
                    .path()
                    .strip_prefix(workspace_root)
                    .unwrap_or(entry.path());
                !ignore.is_ignored(relative)
            });
        for entry in entries
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_type().is_file())
        {
            let path = entry.into_path();
            let relative = path.strip_prefix(workspace_root).unwrap_or(&path);
            if ignore.is_suppressed(relative) {
                snapshot.insert(path.clone(), output_identity(&path));
            }
        }
    }
    snapshot
}

fn output_identity(path: &Path) -> OutputIdentity {
    let Ok(metadata) = fs::metadata(path) else {
        return OutputIdentity::Missing;
    };
    OutputIdentity::File {
        len: metadata.len(),
        modified: metadata.modified().ok(),
    }
}

fn should_suppress_generated_path(
    path: &Path,
    cooldown_active: bool,
    generated_outputs: &mut GeneratedOutputIdentities,
) -> bool {
    cooldown_active || generated_outputs.matches_or_forget(path)
}

fn is_meaningful_event(kind: &notify::EventKind) -> bool {
    matches!(
        kind,
        notify::EventKind::Create(_) | notify::EventKind::Modify(_) | notify::EventKind::Remove(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn export_consumer(dependencies: &[&str], inherit: &[&str], protected: &[&str]) -> ServicePlan {
        ServicePlan {
            name: "consumer".to_string(),
            target_address: "//consumer:dev".to_string(),
            target: crate::plugins::Target::default(),
            project_root: PathBuf::new(),
            port: None,
            open_url: None,
            env: HashMap::new(),
            dependencies: dependencies
                .iter()
                .map(|value| (*value).to_string())
                .collect(),
            inherit_env: inherit.iter().map(|value| (*value).to_string()).collect(),
            protected_env: protected.iter().map(|value| (*value).to_string()).collect(),
            watch: crate::watch::WatchPlan::empty(),
        }
    }

    fn ready_producer(epoch: u64, values: &[(&str, &str)]) -> ProducerState {
        ProducerState {
            generation: 1,
            epoch,
            values: Some(
                values
                    .iter()
                    .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
                    .collect(),
            ),
            healthy: true,
            waiting_since: None,
        }
    }

    #[test]
    fn exported_environment_applies_allowlist_and_precedence() {
        let service = export_consumer(&["producer"], &["TOKEN", "PRIVATE"], &["TOKEN"]);
        let producers = HashMap::from([(
            "producer".to_string(),
            ready_producer(4, &[("TOKEN", "masked"), ("PRIVATE", "selected")]),
        )]);

        let ExportResolution::Ready(resolved) = resolve_exported_environment(&service, &producers)
        else {
            panic!("expected a ready exported environment");
        };
        assert_eq!(
            resolved.values,
            HashMap::from([("PRIVATE".to_string(), "selected".to_string())])
        );
        assert_eq!(resolved.epochs["producer"], 4);
        assert_eq!(resolved.sources, HashSet::from(["producer".to_string()]));
    }

    #[test]
    fn exported_environment_waits_for_health_and_rejects_collisions() {
        let service = export_consumer(&["first", "second"], &["TOKEN"], &[]);
        let mut producers = HashMap::from([
            ("first".to_string(), ready_producer(1, &[("TOKEN", "one")])),
            ("second".to_string(), ready_producer(1, &[("TOKEN", "two")])),
        ]);
        let ExportResolution::Conflict(error) = resolve_exported_environment(&service, &producers)
        else {
            panic!("expected a colliding exported environment");
        };
        assert!(error.contains("from both 'first' and 'second'"));

        producers.get_mut("second").unwrap().healthy = false;
        assert!(matches!(
            resolve_exported_environment(&service, &producers),
            ExportResolution::Pending
        ));
    }

    #[test]
    fn delayed_generated_event_is_ignored_but_changed_identity_restarts() {
        let temp = tempfile::tempdir().unwrap();
        let generated = temp.path().join("generated.js");
        let before = HashMap::from([(generated.clone(), OutputIdentity::Missing)]);
        fs::write(&generated, "generated by prerequisite").unwrap();

        let mut identities = GeneratedOutputIdentities::default();
        identities.record_dispatch(
            before,
            HashMap::from([(generated.clone(), output_identity(&generated))]),
        );

        // This models an FSEvent delivered after the fixed cooldown elapsed.
        assert!(should_suppress_generated_path(
            &generated,
            false,
            &mut identities
        ));

        fs::write(&generated, "subsequent manual edit with a changed identity").unwrap();
        assert!(!should_suppress_generated_path(
            &generated,
            false,
            &mut identities
        ));
    }
}

fn drain_logs(
    process_rx: &Receiver<LogEvent>,
    system_rx: &Receiver<LogEvent>,
    durable_log_tx: &std::sync::mpsc::Sender<LogEvent>,
    dashboard: &mut Dashboard,
    ui: bool,
    #[cfg(unix)] session: Option<&SessionServer>,
) -> bool {
    let mut consumed = false;
    while let Ok(event) = system_rx.try_recv() {
        consumed = true;
        let _ = durable_log_tx.send(event.clone());
        if !ui {
            let stream = if event.stderr { "!" } else { "|" };
            println!("[{}] {stream} {}", event.service, event.line);
        }
        #[cfg(unix)]
        if let Some(session) = session {
            session.publish(SessionEvent::Log(event.clone()));
        }
        dashboard.push_log(event);
    }
    while let Ok(event) = process_rx.try_recv() {
        consumed = true;
        #[cfg(unix)]
        if let Some(session) = session {
            session.publish(SessionEvent::Log(event.clone()));
        }
        dashboard.push_log(event);
    }
    consumed
}

fn emit_system(
    tx: &std::sync::mpsc::Sender<LogEvent>,
    service: &str,
    line: impl Into<String>,
    stderr: bool,
) {
    let _ = tx.send(LogEvent {
        service: service.to_string(),
        line: line.into(),
        stderr,
    });
}

fn print_plan(plan: &DevPlan) {
    eprintln!("[services] {} service(s)", plan.services.len());
    for service in &plan.services {
        let port = service
            .port
            .map(|port| format!(" :{port}"))
            .unwrap_or_default();
        eprintln!(
            "[services]   {}{port} -> {}{}",
            service.name,
            service.target_address,
            service
                .open_url
                .as_ref()
                .map(|url| format!(" [open {url}]"))
                .unwrap_or_default()
        );
    }
    if let Some(port) = plan.control_port {
        eprintln!("[services]   control :{port}");
    }
}

#[derive(serde::Deserialize)]
struct WireControlRequest {
    command: String,
    service: Option<String>,
    token: Option<String>,
}

struct ControlRequest {
    command: String,
    service: Option<String>,
    reply: SyncSender<serde_json::Value>,
}

struct ControlServer {
    rx: Receiver<ControlRequest>,
    stop: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    handles: Vec<std::thread::JoinHandle<()>>,
    token_path: PathBuf,
}

impl ControlServer {
    fn start(port: u16) -> Result<Self> {
        use std::net::TcpListener;

        let listener = TcpListener::bind(("127.0.0.1", port)).with_context(|| {
            format!("failed to bind services control socket on 127.0.0.1:{port}")
        })?;
        listener.set_nonblocking(true)?;
        let (token, token_path) = create_control_token(port)?;
        eprintln!("[services]   control token {}", token_path.display());
        let stop = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let (connection_tx, connection_rx) = crossbeam_channel::bounded::<std::net::TcpStream>(32);
        let mut handles = Vec::new();
        for _ in 0..4 {
            let connections = connection_rx.clone();
            let tx = tx.clone();
            let token = token.clone();
            let stop = stop.clone();
            let shutdown = shutdown.clone();
            handles.push(std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match connections.recv_timeout(Duration::from_millis(100)) {
                        Ok(stream) => {
                            handle_control_connection(stream, &token, &tx, &stop, &shutdown);
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    }
                }
            }));
        }
        let thread_stop = stop.clone();
        let handle = std::thread::spawn(move || {
            use std::io::Write;

            while !thread_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => match connection_tx.try_send(stream) {
                        Ok(()) => {}
                        Err(crossbeam_channel::TrySendError::Full(mut stream)) => {
                            let _ = writeln!(
                                stream,
                                "{}",
                                serde_json::json!({
                                    "ok": false,
                                    "error": "control server busy"
                                })
                            );
                        }
                        Err(crossbeam_channel::TrySendError::Disconnected(_)) => break,
                    },
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(_) => break,
                }
            }
        });
        handles.push(handle);
        Ok(Self {
            rx,
            stop,
            shutdown,
            handles,
            token_path,
        })
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
        let _ = std::fs::remove_file(&self.token_path);
    }
}

fn handle_control_connection(
    mut stream: std::net::TcpStream,
    token: &str,
    tx: &std::sync::mpsc::Sender<ControlRequest>,
    stop: &AtomicBool,
    shutdown: &AtomicBool,
) {
    use std::io::{BufRead, BufReader, Read, Write};

    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let mut line = String::new();
    let parsed = BufReader::new(&stream)
        .take(64 * 1024 + 1)
        .read_line(&mut line)
        .ok()
        .filter(|bytes| *bytes <= 64 * 1024)
        .and_then(|_| serde_json::from_str::<WireControlRequest>(&line).ok());
    let response = match parsed {
        Some(parsed)
            if is_state_changing_control_command(&parsed.command)
                && parsed.token.as_deref() != Some(token) =>
        {
            serde_json::json!({
                "ok": false,
                "error": "valid control token required"
            })
        }
        Some(parsed) if parsed.command == "shutdown" => {
            shutdown.store(true, Ordering::SeqCst);
            executor::request_shutdown();
            serde_json::json!({"ok": true})
        }
        Some(parsed) => {
            let (reply_tx, reply_rx) = mpsc::sync_channel(1);
            let request = ControlRequest {
                command: parsed.command,
                service: parsed.service,
                reply: reply_tx,
            };
            if tx.send(request).is_err() {
                serde_json::json!({"ok": false, "error": "launcher stopped"})
            } else {
                loop {
                    if stop.load(Ordering::SeqCst) {
                        break serde_json::json!({
                            "ok": false,
                            "error": "launcher stopped"
                        });
                    }
                    match reply_rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(response) => break response,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            break serde_json::json!({
                                "ok": false,
                                "error": "launcher stopped"
                            });
                        }
                    }
                }
            }
        }
        None => serde_json::json!({"ok": false, "error": "invalid json"}),
    };
    let _ = writeln!(stream, "{response}");
}

fn is_state_changing_control_command(command: &str) -> bool {
    matches!(command, "restart" | "restart_all" | "shutdown")
}

fn create_control_token(port: u16) -> Result<(String, PathBuf)> {
    use std::fs::OpenOptions;
    use std::io::Write;

    let mut random = [0u8; 32];
    getrandom::fill(&mut random)
        .map_err(|error| anyhow::anyhow!("failed to generate services control token: {error}"))?;
    let token = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let unique = random[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let path = std::env::temp_dir().join(format!(
        "aster-services-{port}-{}-{unique}.token",
        std::process::id()
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("failed to create control token file {}", path.display()))?;
    file.write_all(token.as_bytes())?;
    Ok((token, path))
}

fn workspace_header(root: &Path) -> String {
    let branch = Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|branch| !branch.is_empty())
        .unwrap_or_else(|| "detached".to_string());
    format!("{}  ·  {branch}", root.display())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn open_url(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(target_os = "linux")]
    let program = "xdg-open";
    let mut child = Command::new(program)
        .arg(url)
        .spawn()
        .with_context(|| format!("failed to open {url}"))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(target_os = "windows")]
fn open_url(url: &str) -> Result<()> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let operation = "open\0".encode_utf16().collect::<Vec<_>>();
    let url = url
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            url.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if result as usize <= 32 {
        anyhow::bail!("failed to open URL (ShellExecuteW returned {result:?})");
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn open_url(_url: &str) -> Result<()> {
    Err(anyhow::anyhow!(
        "opening a browser is unsupported on this platform"
    ))
}
