use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyEventKind};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use crate::cli::OutputMode;
use crate::config::WorkspaceConfig;
use crate::discovery::DiscoveredProject;
use crate::executor::{self, Executor};
use crate::graph::TargetGraph;
use crate::watch::WorkspaceIgnore;

use super::dashboard::{Dashboard, DashboardAction, ServiceState, TerminalGuard};
use super::log_files::ServiceLogFiles;
use super::plan::{DevPlan, ServicePlan};
use super::process::{LogEvent, ProcessLogSenders, ServiceProcess};

pub struct DevOptions {
    pub watch: bool,
    pub ui: bool,
    pub dry_run: bool,
    pub use_cache: bool,
}

struct Runtime {
    process: Option<ServiceProcess>,
}

enum StartOutcome {
    Running(ServiceProcess),
    Stopped,
}

struct StartResult {
    service: String,
    outcome: StartOutcome,
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
    let (watch_rx, _watcher) = if options.watch {
        let (rx, watcher) = start_watcher(&plan)?;
        (Some(rx), Some(watcher))
    } else {
        (None, None)
    };
    let mut dashboard = Dashboard::new(&plan.services);
    let mut runtimes: HashMap<String, Runtime> = plan
        .services
        .iter()
        .map(|service| (service.name.clone(), Runtime { process: None }))
        .collect();
    let projects = Arc::new(projects);
    let (start_tx, start_rx) = mpsc::channel::<StartResult>();
    let mut active_start: Option<(String, std::thread::JoinHandle<()>)> = None;
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
    let watch_debounce = Duration::from_millis(config.watch.debounce_ms.unwrap_or(300));
    let mut pending_watch_paths = Vec::new();
    let mut watch_deadline: Option<Instant> = None;
    let mut quitting = false;

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
            );

            while let Ok(result) = start_rx.try_recv() {
                if let Some((name, handle)) = active_start.take() {
                    debug_assert_eq!(name, result.service);
                    let _ = handle.join();
                }
                let runtime = runtimes
                    .get_mut(&result.service)
                    .expect("start result service exists");
                match result.outcome {
                    StartOutcome::Running(process) => {
                        runtime.process = Some(process);
                        dashboard.set_state(&result.service, ServiceState::Running);
                    }
                    StartOutcome::Stopped => {
                        dashboard.set_state(&result.service, ServiceState::Stopped);
                    }
                }
                suppress_until = Instant::now() + Duration::from_millis(700);
                needs_draw = true;
            }
            if active_start.is_none() {
                if let Some((name, reason)) = pending_starts.pop_front() {
                    let service = plan
                        .services
                        .iter()
                        .find(|service| service.name == name)
                        .expect("queued service exists");
                    let handle = begin_start_service(
                        service,
                        workspace_root,
                        projects.clone(),
                        &graph,
                        options.use_cache,
                        options.ui,
                        &log_tx,
                        &system_tx,
                        &durable_log_tx,
                        &mut dashboard,
                        runtimes.get_mut(&name).expect("runtime exists"),
                        &reason,
                        start_tx.clone(),
                    );
                    active_start = Some((name, handle));
                    needs_draw = true;
                }
            }

            for service in &plan.services {
                let runtime = runtimes.get_mut(&service.name).expect("runtime exists");
                let exited = runtime
                    .process
                    .as_mut()
                    .and_then(|process| process.poll().ok().flatten());
                if let Some(code) = exited {
                    runtime.process.take();
                    dashboard.set_state(&service.name, ServiceState::Stopped);
                    emit_system(
                        &system_tx,
                        &service.name,
                        format!("process exited with code {code}"),
                        true,
                    );
                    needs_draw = true;
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
    if let Some((_, handle)) = active_start.take() {
        let _ = handle.join();
    }
    let mut shutdown_processes = Vec::new();
    while let Ok(result) = start_rx.try_recv() {
        if let StartOutcome::Running(process) = result.outcome {
            shutdown_processes.push(process);
        }
    }
    for service in plan.services.iter().rev() {
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
    drain_logs(&log_rx, &system_rx, &durable_log_tx, &mut dashboard, false);
    drop(durable_log_tx);
    let _ = durable_log_handle.join();
    drop(control);
    if let Some(signal) = executor::shutdown_signal() {
        std::process::exit(128 + signal);
    }
    run_result?;
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
    runtime: &mut Runtime,
    reason: &str,
    result_tx: std::sync::mpsc::Sender<StartResult>,
) -> std::thread::JoinHandle<()> {
    if let Some(mut process) = runtime.process.take() {
        process.terminate(Duration::from_secs(3));
    }
    let state = if reason == "initial start" {
        ServiceState::Starting
    } else {
        ServiceState::Restarting
    };
    dashboard.set_state(&service.name, state);
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
    let env = service.env.clone();
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
            });
            return;
        }
        emit_system(
            &system_tx,
            &service_name,
            format!("starting {target_address}"),
            false,
        );
        let outcome = match ServiceProcess::spawn(
            &service_name,
            &target,
            &project_root,
            &env,
            ProcessLogSenders::new(log_tx, system_tx.clone(), durable_log_tx, ui),
        ) {
            Ok(process) => StartOutcome::Running(process),
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
) -> Vec<String> {
    let mut affected = HashSet::new();
    for service in &plan.services {
        for path in paths {
            let relative = path.strip_prefix(workspace_root).unwrap_or(path);
            if ignore.is_ignored(relative) {
                continue;
            }
            if suppress_generated && ignore.is_suppressed(relative) {
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

fn is_meaningful_event(kind: &notify::EventKind) -> bool {
    matches!(
        kind,
        notify::EventKind::Create(_) | notify::EventKind::Modify(_) | notify::EventKind::Remove(_)
    )
}

fn drain_logs(
    process_rx: &Receiver<LogEvent>,
    system_rx: &Receiver<LogEvent>,
    durable_log_tx: &std::sync::mpsc::Sender<LogEvent>,
    dashboard: &mut Dashboard,
    ui: bool,
) -> bool {
    let mut consumed = false;
    while let Ok(event) = system_rx.try_recv() {
        consumed = true;
        let _ = durable_log_tx.send(event.clone());
        if !ui {
            let stream = if event.stderr { "!" } else { "|" };
            println!("[{}] {stream} {}", event.service, event.line);
        }
        dashboard.push_log(event);
    }
    while let Ok(event) = process_rx.try_recv() {
        consumed = true;
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
