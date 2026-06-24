//! Watch event loop: debounces fs events, maps them to targets via the
//! [`WatchPlan`], and drives the executor + stream supervisor.

use anyhow::{Context, Result};
use console::style;
use crossbeam_channel::{select, tick, unbounded};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::discovery::DiscoveredProject;
use crate::executor::Executor;
use crate::graph::TargetGraph;

use super::ignore::WorkspaceIgnore;
use super::plan::WatchPlan;
use super::stream::StreamSupervisor;

/// Options controlling watch-loop behavior.
pub struct WatchOpts {
    pub debounce: Duration,
    pub cooldown: Duration,
    pub stream_grace: Duration,
    pub run_initial: bool,
}

impl Default for WatchOpts {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(300),
            cooldown: Duration::from_millis(500),
            stream_grace: Duration::from_secs(3),
            run_initial: true,
        }
    }
}

pub fn run_watch(
    plan: WatchPlan,
    workspace_root: PathBuf,
    projects: Vec<DiscoveredProject>,
    graph: TargetGraph,
    executor: Executor<'_>,
    ignore: WorkspaceIgnore,
    opts: WatchOpts,
) -> Result<()> {
    let (tx, rx) = unbounded::<notify::Result<notify::Event>>();
    let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })
    .context("failed to create fs watcher")?;

    for root in &plan.watch_roots {
        watcher
            .watch(root, RecursiveMode::Recursive)
            .with_context(|| format!("failed to watch {}", root.display()))?;
    }

    emit_startup_banner(&plan);

    let shutdown = install_signal_handler();

    let mut supervisor = StreamSupervisor::new(opts.stream_grace);

    // Split R into stream vs non-stream.
    let requested_stream: Vec<&super::plan::WatchTarget> = plan
        .targets
        .iter()
        .filter(|t| t.stream && plan.requested.contains(&t.address))
        .collect();
    let requested_non_stream: HashSet<String> = plan
        .requested
        .iter()
        .filter(|addr| !plan.targets.iter().any(|t| t.address == **addr && t.stream))
        .cloned()
        .collect();

    let project_refs: Vec<&DiscoveredProject> = projects.iter().collect();

    // Initial run.
    if opts.run_initial {
        if !requested_non_stream.is_empty() {
            eprintln!("{} initial build…", style("[watch]").cyan().bold());
            let _ = executor.execute_targets(&requested_non_stream, &project_refs, false);
        }
        for t in &requested_stream {
            if let Err(e) = start_stream_target(&mut supervisor, t, &projects) {
                eprintln!(
                    "{} failed to start {}: {e:#}",
                    style("[watch]").red().bold(),
                    t.address
                );
            }
        }
    }

    let supervisor_cell = std::cell::RefCell::new(supervisor);

    let dispatch = |pending: HashSet<String>, _triggers: Vec<PathBuf>| {
        // Split primary by stream vs non-stream.
        let mut stream_addrs: Vec<&super::plan::WatchTarget> = Vec::new();
        let mut non_stream = HashSet::new();
        for addr in &pending {
            let t = plan.targets.iter().find(|t| &t.address == addr);
            match t {
                Some(t) if t.stream => stream_addrs.push(t),
                _ => {
                    non_stream.insert(addr.clone());
                }
            }
        }

        if !non_stream.is_empty() {
            let _ = executor.execute_targets(&non_stream, &project_refs, false);
        }

        for t in stream_addrs {
            eprintln!(
                "{} restart {}",
                style("[watch]").cyan().bold(),
                style(&t.address).yellow()
            );
            let mut sup = supervisor_cell.borrow_mut();
            if let Err(e) = restart_stream_target(&mut sup, t, &projects) {
                eprintln!(
                    "{} failed to restart {}: {e:#}",
                    style("[watch]").red().bold(),
                    t.address
                );
            }
        }
    };

    let idle_tick = || {
        let mut sup = supervisor_cell.borrow_mut();
        for (addr, code) in sup.reap() {
            eprintln!(
                "{} stream target {addr} exited (code {code})",
                style("[watch]").yellow().bold()
            );
        }
    };

    let result = run_event_loop(
        &plan,
        &workspace_root,
        &graph,
        &ignore,
        &opts,
        &rx,
        shutdown,
        Some(&idle_tick),
        dispatch,
    );

    supervisor_cell.borrow_mut().shutdown_all();
    result
}

/// Inner event-loop body — debounces events, partitions via the plan, and
/// calls `dispatch(primary_set, trigger_paths)` once per cycle.
///
/// Separated from [`run_watch`] so tests can push synthetic events through
/// the channel, control timing, and observe dispatch calls without touching
/// the filesystem, the executor, or the stream supervisor.
#[allow(clippy::too_many_arguments, unused_assignments)]
pub fn run_event_loop<F>(
    plan: &WatchPlan,
    workspace_root: &Path,
    graph: &TargetGraph,
    ignore: &WorkspaceIgnore,
    opts: &WatchOpts,
    event_rx: &crossbeam_channel::Receiver<notify::Result<notify::Event>>,
    shutdown: &AtomicBool,
    on_idle_tick: Option<&dyn Fn()>,
    mut dispatch: F,
) -> Result<()>
where
    F: FnMut(HashSet<String>, Vec<PathBuf>),
{
    // Track cooldown window for self-build event suppression.
    // An `Instant` already in the past means "not suppressed".
    let mut suppress_until: Instant = Instant::now();
    let poll_stream = tick(Duration::from_millis(250));

    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }

        let first = select! {
            recv(event_rx) -> msg => match msg {
                Ok(Ok(ev)) => Some(ev),
                Ok(Err(e)) => {
                    eprintln!("{} watcher error: {e}", style("[watch]").red().bold());
                    continue;
                }
                Err(_) => break,
            },
            recv(poll_stream) -> _ => {
                if let Some(cb) = on_idle_tick { cb(); }
                continue;
            }
        };

        let Some(first) = first else { continue };

        let mut pending: HashSet<String> = HashSet::new();
        let mut trigger_paths: Vec<PathBuf> = Vec::new();

        process_event(
            &first,
            plan,
            workspace_root,
            ignore,
            suppress_until,
            graph,
            &mut pending,
            &mut trigger_paths,
        );

        // Drain additional events within the debounce window.
        let deadline = Instant::now() + opts.debounce;
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            match event_rx.recv_timeout(remaining) {
                Ok(Ok(ev)) => process_event(
                    &ev,
                    plan,
                    workspace_root,
                    ignore,
                    suppress_until,
                    graph,
                    &mut pending,
                    &mut trigger_paths,
                ),
                Ok(Err(e)) => eprintln!("{} watcher error: {e}", style("[watch]").red().bold()),
                Err(_) => break,
            }
        }

        if pending.is_empty() {
            continue;
        }

        let mut seen_samples: HashSet<String> = HashSet::new();
        let samples: Vec<String> = trigger_paths
            .iter()
            .map(|p| {
                p.strip_prefix(workspace_root)
                    .unwrap_or(p)
                    .display()
                    .to_string()
            })
            .filter(|s| seen_samples.insert(s.clone()))
            .take(3)
            .collect();
        eprintln!(
            "{} change: {}",
            style("[watch]").cyan().bold(),
            samples.join(", "),
        );

        // Open suppression window before dispatch so events generated by the
        // rebuild itself fall inside it.
        suppress_until = Instant::now() + opts.cooldown;

        dispatch(pending, trigger_paths);

        // Reset to cover stragglers that land after dispatch returns.
        suppress_until = Instant::now() + opts.cooldown;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn process_event(
    ev: &notify::Event,
    plan: &WatchPlan,
    workspace_root: &Path,
    ignore: &WorkspaceIgnore,
    suppress_until: Instant,
    graph: &TargetGraph,
    pending: &mut HashSet<String>,
    trigger_paths: &mut Vec<PathBuf>,
) {
    if !is_meaningful_kind(&ev.kind) {
        return;
    }

    let suppressed = Instant::now() < suppress_until;

    for path in &ev.paths {
        // Relative to workspace root for ignore check.
        let rel = match path.strip_prefix(workspace_root) {
            Ok(r) => r,
            Err(_) => path.as_path(),
        };

        if ignore.is_ignored(rel) || ignore.is_suppressed(rel) {
            continue;
        }

        if suppressed {
            continue;
        }

        let owners = plan.owners_of(path);
        if owners.is_empty() {
            continue;
        }

        trigger_paths.push(path.clone());
        let primary = plan.primary_set(&owners, graph);
        pending.extend(primary);
    }
}

fn is_meaningful_kind(kind: &notify::EventKind) -> bool {
    use notify::EventKind::*;
    matches!(kind, Create(_) | Modify(_) | Remove(_))
}

fn start_stream_target(
    supervisor: &mut StreamSupervisor,
    t: &super::plan::WatchTarget,
    projects: &[DiscoveredProject],
) -> Result<()> {
    let project = projects
        .iter()
        .find(|p| format!("//{}", p.relative_path.display()) == t.project_address)
        .context("project not found for stream target")?;
    let target = project
        .targets
        .get(&t.target_name)
        .context("target def not found")?;
    eprintln!(
        "{} start {}",
        style("[watch]").cyan().bold(),
        style(&t.address).yellow()
    );
    supervisor.spawn(&t.address, target, &project.root)
}

fn restart_stream_target(
    supervisor: &mut StreamSupervisor,
    t: &super::plan::WatchTarget,
    projects: &[DiscoveredProject],
) -> Result<()> {
    let project = projects
        .iter()
        .find(|p| format!("//{}", p.relative_path.display()) == t.project_address)
        .context("project not found for stream target")?;
    let target = project
        .targets
        .get(&t.target_name)
        .context("target def not found")?;
    supervisor.restart(&t.address, target, &project.root)
}

fn emit_startup_banner(plan: &WatchPlan) {
    let fallbacks: Vec<&str> = plan
        .targets
        .iter()
        .filter(|t| t.uses_fallback && plan.requested.contains(&t.address))
        .map(|t| t.address.as_str())
        .collect();

    eprintln!(
        "{} watching {} target(s) across {} project(s)",
        style("[watch]").cyan().bold(),
        plan.watched.len(),
        plan.watch_roots.len(),
    );

    for addr in &fallbacks {
        eprintln!(
            "{} {} has no cache_inputs; watching whole project dir (set [targets.X.cache] to refine)",
            style("[watch]").yellow().bold(),
            addr,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WatchWorkspaceConfig;
    use crate::discovery::DiscoveredProject;
    use crate::graph::build_target_graph;
    use crate::plugins::{NodeJsPlugin, PluginRegistry, ProjectMetadata, Target};
    use notify::event::{CreateKind, DataChange, ModifyKind};
    use notify::{Event, EventKind};
    use std::collections::HashMap as StdHashMap;
    use std::path::PathBuf;

    fn mk_project(rel: &str, abs_root: &str, targets: &[(&str, Vec<&str>)]) -> DiscoveredProject {
        let mut target_map = StdHashMap::new();
        for (name, deps) in targets {
            target_map.insert(
                name.to_string(),
                Target {
                    command: format!("echo {name}"),
                    depends_on: deps.iter().map(|d| d.to_string()).collect(),
                    ..Default::default()
                },
            );
        }
        DiscoveredProject {
            root: PathBuf::from(abs_root),
            config_path: PathBuf::from(format!("{abs_root}/package.json")),
            metadata: ProjectMetadata {
                name: rel.split('/').next_back().unwrap().to_string(),
                version: None,
            },
            dependencies: vec![],
            targets: target_map,
            plugin_name: "nodejs".to_string(),
            relative_path: PathBuf::from(rel),
        }
    }

    fn registry() -> PluginRegistry {
        let mut r = PluginRegistry::new();
        r.register(Box::new(NodeJsPlugin));
        r
    }

    fn ev(kind: EventKind, path: impl Into<PathBuf>) -> Event {
        Event {
            kind,
            paths: vec![path.into()],
            attrs: Default::default(),
        }
    }

    fn modify(path: &str) -> Event {
        ev(
            EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            path,
        )
    }

    struct Fixture {
        plan: WatchPlan,
        graph: crate::graph::TargetGraph,
        workspace_root: PathBuf,
        ignore: WorkspaceIgnore,
    }

    fn two_project_fixture() -> Fixture {
        let workspace_root = PathBuf::from("/repo");
        let projects = vec![
            mk_project(
                "services/api",
                "/repo/services/api",
                &[("build", vec!["//libs/core:build"])],
            ),
            mk_project("libs/core", "/repo/libs/core", &[("build", vec![])]),
        ];
        let graph = build_target_graph(&projects);
        let plan = WatchPlan::build(
            &["//services/api:build".to_string()],
            &projects,
            &graph,
            &registry(),
        )
        .unwrap();
        let ignore = WorkspaceIgnore::build(&WatchWorkspaceConfig::default()).unwrap();
        Fixture {
            plan,
            graph,
            workspace_root,
            ignore,
        }
    }

    fn run_event(
        f: &Fixture,
        event: &Event,
        suppress_until: Instant,
    ) -> (HashSet<String>, Vec<PathBuf>) {
        let mut pending = HashSet::new();
        let mut trigger_paths = Vec::new();
        process_event(
            event,
            &f.plan,
            &f.workspace_root,
            &f.ignore,
            suppress_until,
            &f.graph,
            &mut pending,
            &mut trigger_paths,
        );
        (pending, trigger_paths)
    }

    fn not_suppressed() -> Instant {
        Instant::now() - Duration::from_secs(10)
    }

    #[test]
    fn input_file_change_triggers_owner_and_dependents() {
        let f = two_project_fixture();
        let e = modify("/repo/libs/core/src/index.ts");
        let (pending, triggers) = run_event(&f, &e, not_suppressed());

        assert!(pending.contains("//libs/core:build"));
        assert!(pending.contains("//services/api:build"));
        assert_eq!(pending.len(), 2);
        assert_eq!(triggers.len(), 1);
    }

    #[test]
    fn non_input_file_is_noop() {
        let f = two_project_fixture();
        let e = modify("/repo/libs/core/README.md");
        let (pending, triggers) = run_event(&f, &e, not_suppressed());

        assert!(pending.is_empty());
        assert!(triggers.is_empty());
    }

    #[test]
    fn workspace_ignored_path_is_noop() {
        let f = two_project_fixture();
        // .git/ is in the default ignore set, even though the path is inside
        // the workspace.
        let e = modify("/repo/.git/HEAD");
        let (pending, triggers) = run_event(&f, &e, not_suppressed());

        assert!(pending.is_empty());
        assert!(triggers.is_empty());
    }

    #[test]
    fn node_modules_is_ignored_by_default() {
        let f = two_project_fixture();
        // node_modules files are very noisy during `npm install` — must not
        // trigger any rebuild.
        let e = modify("/repo/libs/core/node_modules/foo/index.js");
        let (pending, _) = run_event(&f, &e, not_suppressed());
        assert!(pending.is_empty());
    }

    #[test]
    fn suppression_window_drops_events() {
        let f = two_project_fixture();
        let e = modify("/repo/libs/core/src/index.ts");
        let future = Instant::now() + Duration::from_secs(60);
        let (pending, triggers) = run_event(&f, &e, future);

        assert!(pending.is_empty());
        assert!(triggers.is_empty());
    }

    #[test]
    fn access_event_kind_is_dropped() {
        let f = two_project_fixture();
        // Access events are noise; `stat`, reading a file, etc.
        let e = ev(
            EventKind::Access(notify::event::AccessKind::Read),
            "/repo/libs/core/src/index.ts",
        );
        let (pending, triggers) = run_event(&f, &e, not_suppressed());

        assert!(pending.is_empty());
        assert!(triggers.is_empty());
    }

    #[test]
    fn create_event_is_processed() {
        let f = two_project_fixture();
        let e = ev(
            EventKind::Create(CreateKind::File),
            "/repo/libs/core/src/new.ts",
        );
        let (pending, _) = run_event(&f, &e, not_suppressed());
        assert!(pending.contains("//libs/core:build"));
    }

    #[test]
    fn path_outside_all_projects_is_noop() {
        let f = two_project_fixture();
        let e = modify("/somewhere/else/foo.ts");
        let (pending, triggers) = run_event(&f, &e, not_suppressed());

        assert!(pending.is_empty());
        assert!(triggers.is_empty());
    }

    #[test]
    fn event_with_multiple_paths_accumulates_owners() {
        let f = two_project_fixture();
        let event = Event {
            kind: EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            paths: vec![
                PathBuf::from("/repo/libs/core/src/a.ts"),
                PathBuf::from("/repo/services/api/src/main.ts"),
                PathBuf::from("/repo/libs/core/README.md"), // filtered
            ],
            attrs: Default::default(),
        };
        let (pending, triggers) = run_event(&f, &event, not_suppressed());

        // Both input files own targets; README is filtered out. api:build
        // appears both as its own owner and as core:build's dependent.
        assert!(pending.contains("//libs/core:build"));
        assert!(pending.contains("//services/api:build"));
        assert_eq!(pending.len(), 2);
        assert_eq!(triggers.len(), 2); // 2 kept, 1 filtered
    }

    #[test]
    fn multiple_targets_in_same_project_both_fire_when_both_watched() {
        // Watching both build + test of the same project. A source file matches
        // both targets' cache_inputs.
        let workspace_root = PathBuf::from("/repo");
        let projects = vec![mk_project(
            "libs/core",
            "/repo/libs/core",
            &[("build", vec![]), ("test", vec![])],
        )];
        let graph = build_target_graph(&projects);
        let plan = WatchPlan::build(
            &[
                "//libs/core:build".to_string(),
                "//libs/core:test".to_string(),
            ],
            &projects,
            &graph,
            &registry(),
        )
        .unwrap();
        let ignore = WorkspaceIgnore::build(&WatchWorkspaceConfig::default()).unwrap();
        let f = Fixture {
            plan,
            graph,
            workspace_root,
            ignore,
        };

        let e = modify("/repo/libs/core/src/a.ts");
        let (pending, _) = run_event(&f, &e, not_suppressed());

        assert!(pending.contains("//libs/core:build"));
        assert!(pending.contains("//libs/core:test"));
    }

    #[test]
    fn test_only_target_fires_for_test_file() {
        // A file only matched by :test (e.g. *.test.ts) should not fire :build.
        let workspace_root = PathBuf::from("/repo");
        let projects = vec![mk_project(
            "libs/core",
            "/repo/libs/core",
            &[("build", vec![]), ("test", vec![])],
        )];
        let graph = build_target_graph(&projects);
        let plan = WatchPlan::build(
            &[
                "//libs/core:build".to_string(),
                "//libs/core:test".to_string(),
            ],
            &projects,
            &graph,
            &registry(),
        )
        .unwrap();
        let ignore = WorkspaceIgnore::build(&WatchWorkspaceConfig::default()).unwrap();
        let f = Fixture {
            plan,
            graph,
            workspace_root,
            ignore,
        };

        // nodejs test plugin inputs include test/**/*.ts and **/*.test.ts,
        // but not src/**/*.ts for :test alone. Actually both cover src/, but
        // test covers **/*.test.ts exclusively.
        let e = modify("/repo/libs/core/test/foo.test.ts");
        let (pending, _) = run_event(&f, &e, not_suppressed());
        assert!(pending.contains("//libs/core:test"));
        assert!(
            !pending.contains("//libs/core:build"),
            "a test-only path should not fire :build"
        );
    }

    #[test]
    fn suppression_that_expired_is_ignored() {
        let f = two_project_fixture();
        let past = Instant::now() - Duration::from_millis(1);
        let e = modify("/repo/libs/core/src/index.ts");
        let (pending, _) = run_event(&f, &e, past);
        // An expired suppress_until must NOT drop events.
        assert!(pending.contains("//libs/core:build"));
    }

    // ------------------------------------------------------------------
    // run_event_loop tests — exercise the full debounce + dispatch cycle
    // with synthetic events pushed through a channel.
    // ------------------------------------------------------------------

    use crossbeam_channel::{unbounded as crossbeam_unbounded, Sender};
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};

    /// Recorded invocations of the dispatch callback.
    #[derive(Default)]
    struct Dispatches {
        calls: Mutex<Vec<DispatchCall>>,
    }

    struct DispatchCall {
        primary: HashSet<String>,
        delay: Option<Duration>,
    }

    impl Dispatches {
        fn record(&self, primary: HashSet<String>, delay: Option<Duration>) {
            self.calls
                .lock()
                .unwrap()
                .push(DispatchCall { primary, delay });
        }

        fn count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }

        fn snapshot(&self) -> Vec<HashSet<String>> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|c| c.primary.clone())
                .collect()
        }
    }

    fn send_modify(tx: &Sender<notify::Result<Event>>, path: &str) {
        tx.send(Ok(modify(path))).unwrap();
    }

    /// Run `run_event_loop` in a background thread, collect dispatches.
    ///
    /// Returns `(tx, shutdown, dispatches, handle)` — tests push events via `tx`,
    /// flip `shutdown` to stop, then join the handle.
    fn spawn_loop_with_fixture(
        fixture: &'static Fixture,
        opts: WatchOpts,
        dispatch_delay: Option<Duration>,
    ) -> (
        Sender<notify::Result<Event>>,
        Arc<AtomicBool>,
        Arc<Dispatches>,
        std::thread::JoinHandle<()>,
    ) {
        let (tx, rx) = crossbeam_unbounded::<notify::Result<Event>>();
        let shutdown = Arc::new(AtomicBool::new(false));
        let dispatches = Arc::new(Dispatches::default());

        let shutdown_clone = Arc::clone(&shutdown);
        let dispatches_clone = Arc::clone(&dispatches);

        let handle = std::thread::spawn(move || {
            let dispatch = |primary: HashSet<String>, _triggers: Vec<PathBuf>| {
                dispatches_clone.record(primary, dispatch_delay);
                if let Some(d) = dispatch_delay {
                    std::thread::sleep(d);
                }
            };
            let _ = run_event_loop(
                &fixture.plan,
                &fixture.workspace_root,
                &fixture.graph,
                &fixture.ignore,
                &opts,
                &rx,
                &shutdown_clone,
                None,
                dispatch,
            );
        });

        (tx, shutdown, dispatches, handle)
    }

    fn leaked_fixture() -> &'static Fixture {
        Box::leak(Box::new(two_project_fixture()))
    }

    fn stop_loop(
        tx: Sender<notify::Result<Event>>,
        shutdown: Arc<AtomicBool>,
        handle: std::thread::JoinHandle<()>,
    ) {
        shutdown.store(true, Ordering::SeqCst);
        // Send a final event to wake the blocking recv so the shutdown flag is checked.
        drop(tx);
        let _ = handle.join();
    }

    fn wait_until<F: Fn() -> bool>(cond: F, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        cond()
    }

    #[test]
    fn debounce_coalesces_multiple_events_into_one_dispatch() {
        let f = leaked_fixture();
        let opts = WatchOpts {
            debounce: Duration::from_millis(200),
            cooldown: Duration::from_millis(10),
            ..Default::default()
        };
        let (tx, shutdown, dispatches, handle) = spawn_loop_with_fixture(f, opts, None);

        // Three events within the debounce window.
        send_modify(&tx, "/repo/libs/core/src/a.ts");
        std::thread::sleep(Duration::from_millis(30));
        send_modify(&tx, "/repo/libs/core/src/b.ts");
        std::thread::sleep(Duration::from_millis(30));
        send_modify(&tx, "/repo/services/api/src/main.ts");

        // Wait for dispatch to fire after debounce elapses.
        assert!(
            wait_until(|| dispatches.count() >= 1, Duration::from_secs(2)),
            "expected at least one dispatch"
        );

        // Give the loop a beat to make sure no *second* dispatch follows.
        std::thread::sleep(Duration::from_millis(300));
        let snapshot = dispatches.snapshot();
        assert_eq!(
            snapshot.len(),
            1,
            "expected exactly one dispatch, got {} with contents {snapshot:?}",
            snapshot.len()
        );

        let primary = &snapshot[0];
        assert!(primary.contains("//libs/core:build"));
        assert!(primary.contains("//services/api:build"));

        stop_loop(tx, shutdown, handle);
    }

    #[test]
    fn events_during_dispatch_fire_second_cycle_when_cooldown_is_zero() {
        // With cooldown=0, events that arrive during a running dispatch MUST
        // produce a second dispatch after the first returns.
        let f = leaked_fixture();
        let opts = WatchOpts {
            debounce: Duration::from_millis(80),
            cooldown: Duration::from_millis(0),
            ..Default::default()
        };
        // Dispatch sleeps 400ms so we can push events mid-run.
        let (tx, shutdown, dispatches, handle) =
            spawn_loop_with_fixture(f, opts, Some(Duration::from_millis(400)));

        send_modify(&tx, "/repo/libs/core/src/a.ts");
        assert!(
            wait_until(|| dispatches.count() >= 1, Duration::from_secs(2)),
            "dispatch #1 never started"
        );

        // Mid-build edit to a DIFFERENT target's inputs.
        std::thread::sleep(Duration::from_millis(50));
        send_modify(&tx, "/repo/services/api/src/main.ts");

        assert!(
            wait_until(|| dispatches.count() >= 2, Duration::from_secs(3)),
            "dispatch #2 never fired. got count={}",
            dispatches.count()
        );

        let snapshot = dispatches.snapshot();
        assert_eq!(snapshot.len(), 2, "expected 2 dispatches, got {snapshot:?}");

        // First dispatch: core change (+ api as a dependent in W).
        assert!(snapshot[0].contains("//libs/core:build"));
        assert!(snapshot[0].contains("//services/api:build"));
        // Second dispatch: api-only change.
        assert!(snapshot[1].contains("//services/api:build"));
        assert!(
            !snapshot[1].contains("//libs/core:build"),
            "second dispatch should not include core: {:?}",
            snapshot[1]
        );

        stop_loop(tx, shutdown, handle);
    }

    #[test]
    fn events_during_dispatch_are_suppressed_by_cooldown_window() {
        // With a nonzero cooldown, events that arrive during dispatch and are
        // processed while the cooldown window is still open MUST be dropped.
        // This is the anti-feedback-loop behavior for build-generated events.
        let f = leaked_fixture();
        let opts = WatchOpts {
            debounce: Duration::from_millis(50),
            cooldown: Duration::from_millis(400),
            ..Default::default()
        };
        // Dispatch blocks for 300ms. An event sent shortly after dispatch
        // starts will be processed while the 400ms cooldown is still open.
        let (tx, shutdown, dispatches, handle) =
            spawn_loop_with_fixture(f, opts, Some(Duration::from_millis(300)));

        send_modify(&tx, "/repo/libs/core/src/a.ts");
        assert!(
            wait_until(|| dispatches.count() >= 1, Duration::from_secs(2)),
            "dispatch #1 never started"
        );

        // Mid-build event — will sit in the channel until dispatch returns,
        // then get processed while cooldown is still active.
        std::thread::sleep(Duration::from_millis(50));
        send_modify(&tx, "/repo/services/api/src/main.ts");

        // Wait well past dispatch end but within cooldown. Second dispatch
        // must NOT have fired.
        std::thread::sleep(Duration::from_millis(450));
        assert_eq!(
            dispatches.count(),
            1,
            "expected cooldown to suppress mid-build event; got dispatches: {:?}",
            dispatches.snapshot()
        );

        stop_loop(tx, shutdown, handle);
    }

    #[test]
    fn events_after_cooldown_trigger_new_dispatch() {
        // After the cooldown window expires, subsequent events MUST fire.
        let f = leaked_fixture();
        let opts = WatchOpts {
            debounce: Duration::from_millis(50),
            cooldown: Duration::from_millis(100),
            ..Default::default()
        };
        let (tx, shutdown, dispatches, handle) = spawn_loop_with_fixture(f, opts, None);

        send_modify(&tx, "/repo/libs/core/src/a.ts");
        assert!(
            wait_until(|| dispatches.count() >= 1, Duration::from_secs(2)),
            "dispatch #1 never fired"
        );

        // Wait past cooldown + a safety margin before sending the next event.
        std::thread::sleep(Duration::from_millis(250));
        send_modify(&tx, "/repo/services/api/src/main.ts");

        assert!(
            wait_until(|| dispatches.count() >= 2, Duration::from_secs(2)),
            "dispatch #2 never fired after cooldown; count={}",
            dispatches.count()
        );

        stop_loop(tx, shutdown, handle);
    }

    #[test]
    fn shutdown_flag_stops_loop_promptly() {
        let f = leaked_fixture();
        let opts = WatchOpts {
            debounce: Duration::from_millis(50),
            cooldown: Duration::from_millis(5),
            ..Default::default()
        };
        let (tx, shutdown, _dispatches, handle) = spawn_loop_with_fixture(f, opts, None);

        shutdown.store(true, Ordering::SeqCst);
        // Wake the blocking select! so shutdown is checked.
        drop(tx);

        let start = Instant::now();
        handle.join().unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "loop did not stop promptly: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn empty_pending_does_not_trigger_dispatch() {
        let f = leaked_fixture();
        let opts = WatchOpts {
            debounce: Duration::from_millis(50),
            cooldown: Duration::from_millis(5),
            ..Default::default()
        };
        let (tx, shutdown, dispatches, handle) = spawn_loop_with_fixture(f, opts, None);

        // Only send events that don't match any target's cache_inputs.
        send_modify(&tx, "/repo/libs/core/README.md");
        send_modify(&tx, "/repo/services/api/CHANGELOG.md");

        // Wait past debounce window and verify no dispatch occurred.
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(dispatches.count(), 0);

        stop_loop(tx, shutdown, handle);
    }

    #[test]
    fn idle_tick_callback_fires_when_no_events() {
        let f = leaked_fixture();
        let ticks = Arc::new(AtomicUsize::new(0));
        let ticks_clone = Arc::clone(&ticks);

        let (tx, rx) = crossbeam_unbounded::<notify::Result<Event>>();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = Arc::clone(&shutdown);

        let handle = std::thread::spawn(move || {
            let idle = move || {
                ticks_clone.fetch_add(1, Ordering::SeqCst);
            };
            let dispatch = |_: HashSet<String>, _: Vec<PathBuf>| {};
            let opts = WatchOpts {
                debounce: Duration::from_millis(50),
                ..Default::default()
            };
            let _ = run_event_loop(
                &f.plan,
                &f.workspace_root,
                &f.graph,
                &f.ignore,
                &opts,
                &rx,
                &shutdown_clone,
                Some(&idle),
                dispatch,
            );
        });

        // poll_stream fires every 250ms — wait for at least 2 ticks.
        assert!(
            wait_until(|| ticks.load(Ordering::SeqCst) >= 2, Duration::from_secs(2),),
            "idle tick never fired"
        );

        shutdown.store(true, Ordering::SeqCst);
        drop(tx);
        handle.join().unwrap();
    }
}

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

fn install_signal_handler() -> &'static AtomicBool {
    SHUTDOWN.store(false, Ordering::SeqCst);

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

    &SHUTDOWN
}

#[cfg(unix)]
extern "C" fn shutdown_handler(_sig: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}
