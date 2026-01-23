# Phase 4: Output & Testing - Research

**Researched:** 2026-01-23
**Domain:** Terminal UI, progress display, JSON output, CLI flags, integration testing
**Confidence:** HIGH

## Summary

Phase 4 transforms aster from a functional CLI into a polished user experience with multi-line progress display, machine-readable JSON output, and validation against a real monorepo. The core challenges are: (1) implementing Nx/Turborepo-style multi-line live progress using indicatif's MultiProgress, (2) adding global `--json`, `--verbose`, `--quiet` flags across all commands with proper output routing, (3) creating the `aster logs` command for post-run log retrieval, and (4) building integration tests against the ~/archastro/firstlanding-wt9 monorepo.

The CONTEXT.md decisions lock in specific behaviors: multi-line live status with last 2-3 lines per running project, final JSON blob only (not streaming), log storage for retrieval via `aster logs`, and show last 10-15 lines of failure output inline. Research confirms indicatif + console is the standard stack for this, and assert_cmd + predicates for integration testing.

**Primary recommendation:** Use `indicatif` 0.17+ with `MultiProgress` for concurrent progress display, `console` for terminal detection and colors, add `--json`/`--verbose`/`--quiet` as global clap arguments, store run logs in `.aster/logs/` (project-local), and use `assert_cmd` + `predicates` for integration tests.

## Standard Stack

The established libraries/tools for this domain:

### Core
| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| indicatif | 0.17.x | Progress bars, spinners, MultiProgress | From console-rs, 95x faster than 0.16, industry standard |
| console | 0.16.x | Terminal abstraction, colors, styling | Same author as indicatif, integrates perfectly |
| assert_cmd | 2.0.x | CLI integration testing | Standard for Rust CLI testing, predicates integration |
| predicates | 3.1.x | Assertion matchers for tests | Powers assert_cmd assertions, flexible matching |

### Supporting
| Library | Version | Purpose | When to Use |
|---------|---------|---------|-------------|
| serde_json | 1.0.x | JSON serialization | Already in Cargo.toml, for --json output |
| directories | 5.x | Platform-agnostic dirs | If log storage moves to XDG dirs |
| tempfile | 3.x | Temp directories for tests | Already in dev-dependencies |

### Alternatives Considered
| Instead of | Could Use | Tradeoff |
|------------|-----------|----------|
| indicatif | linya | linya is simpler but less features, fewer style options |
| indicatif | ratatui | ratatui is full TUI, overkill for progress display |
| console | termcolor | termcolor doesn't include indicatif integration |
| console | owo-colors | owo-colors is lighter but no Term abstraction |

**Installation:**
```toml
# Add to Cargo.toml [dependencies]
indicatif = "0.17"
console = "0.16"

# Add to [dev-dependencies]
assert_cmd = "2.0"
predicates = "3.1"
```

## Architecture Patterns

### Recommended Project Structure
```
src/
├── cli/
│   ├── mod.rs           # CLI module (existing)
│   ├── commands.rs      # Command definitions (extend with global flags)
│   ├── run.rs           # Target execution logic (existing)
│   └── output.rs        # NEW: Output formatting (human vs JSON)
├── executor/
│   ├── mod.rs           # (existing)
│   ├── runner.rs        # (existing, enhance with progress)
│   └── logs.rs          # NEW: Log storage and retrieval
├── ui/
│   ├── mod.rs           # NEW: UI module
│   ├── progress.rs      # NEW: MultiProgress management
│   └── colors.rs        # NEW: Color scheme and styling
tests/
├── integration.rs       # (existing, expand)
├── integration/
│   ├── mod.rs           # NEW: Integration test utilities
│   ├── monorepo.rs      # NEW: Real monorepo tests
│   └── json_output.rs   # NEW: JSON output tests
```

### Pattern 1: Global CLI Flags

**What:** Add `--json`, `--verbose`, `--quiet` as global flags available to all commands.

**When to use:** For all commands to support machine-readable and verbosity control.

**Example:**
```rust
// Source: clap derive docs + existing commands.rs
use clap::{Parser, Subcommand, ArgGroup};

#[derive(Parser)]
#[command(name = "aster")]
#[command(version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Enable verbose output (streams all command output)
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Show only final pass/fail (minimal output)
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    /// Output results as JSON (machine-readable)
    #[arg(long, global = true)]
    pub json: bool,
}

// Output mode derived from flags
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OutputMode {
    Normal,    // Default: progress display + grouped output
    Verbose,   // Stream all output as it happens
    Quiet,     // Only final summary
    Json,      // Machine-readable JSON to stdout
}

impl Cli {
    pub fn output_mode(&self) -> OutputMode {
        if self.json {
            OutputMode::Json
        } else if self.verbose {
            OutputMode::Verbose
        } else if self.quiet {
            OutputMode::Quiet
        } else {
            OutputMode::Normal
        }
    }
}
```

### Pattern 2: Multi-Line Progress Display

**What:** Nx/Turborepo-style progress showing multiple running projects with live status.

**When to use:** For target execution in Normal output mode.

**Example:**
```rust
// Source: indicatif MultiProgress docs + CONTEXT.md decisions
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use console::{style, Term};
use std::time::Duration;

pub struct ProgressDisplay {
    multi: MultiProgress,
    term: Term,
    bars: HashMap<String, ProgressBar>,
}

impl ProgressDisplay {
    pub fn new() -> Self {
        let multi = MultiProgress::new();
        // Enable cursor movement to reduce flicker
        // Note: Don't use set_move_cursor if bar count changes dynamically

        Self {
            multi,
            term: Term::stderr(),
            bars: HashMap::new(),
        }
    }

    /// Create a progress bar for a running target
    pub fn add_running(&mut self, address: &str) -> ProgressBar {
        // Style: "//services/api:test [running] last output line..."
        let style = ProgressStyle::with_template(
            "{prefix:.bold.cyan} [{spinner:.yellow}] {msg}"
        )
        .unwrap()
        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏");

        let pb = self.multi.add(ProgressBar::new_spinner());
        pb.set_style(style);
        pb.set_prefix(address.to_string());
        pb.enable_steady_tick(Duration::from_millis(100));

        self.bars.insert(address.to_string(), pb.clone());
        pb
    }

    /// Update progress bar with last few output lines
    pub fn update_output(&self, address: &str, output: &str) {
        if let Some(pb) = self.bars.get(address) {
            // Take last 2-3 lines for preview
            let lines: Vec<&str> = output.lines().rev().take(3).collect();
            let preview = lines.into_iter().rev().collect::<Vec<_>>().join(" | ");
            pb.set_message(preview);
        }
    }

    /// Mark target as complete with success/failure
    pub fn mark_complete(&mut self, address: &str, success: bool, duration_ms: u128) {
        if let Some(pb) = self.bars.remove(address) {
            let status = if success {
                style("PASS").green().bold()
            } else {
                style("FAIL").red().bold()
            };

            pb.set_style(ProgressStyle::with_template(
                "{prefix:.bold} [{msg}] ({elapsed})"
            ).unwrap());
            pb.set_message(format!("{}", status));
            pb.finish();
        }
    }

    /// Print summary line at bottom
    pub fn update_summary(&self, complete: usize, running: usize, pending: usize, failed: usize) {
        // Use println through MultiProgress to print above bars
        let summary = format!(
            "{}/{} complete {} {} running {} {} pending{}",
            complete,
            complete + running + pending,
            style("•").dim(),
            running,
            style("•").dim(),
            pending,
            if failed > 0 { format!(" {} {} failed", style("•").dim(), style(failed).red()) } else { String::new() }
        );
        // Note: This would need custom implementation - indicatif doesn't have
        // a built-in "pinned footer" feature. Alternative: use Term::move_cursor_up
    }
}
```

### Pattern 3: JSON Output Structure

**What:** Machine-readable JSON output for all commands.

**When to use:** When `--json` flag is provided.

**Example:**
```rust
// Source: CONTEXT.md decisions + serde_json patterns
use serde::{Serialize, Deserialize};
use serde_json::json;

// For execution commands (test, build, etc.)
#[derive(Serialize)]
pub struct ExecutionOutput {
    pub results: HashMap<String, HashMap<String, TargetResult>>,
    pub summary: ExecutionSummary,
}

#[derive(Serialize)]
pub struct TargetResult {
    pub status: String,  // "passed", "failed", "skipped"
    pub exit_code: Option<i32>,
    pub duration_ms: u128,
    // Note: actual output not included - use `aster logs` for that
}

#[derive(Serialize)]
pub struct ExecutionSummary {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub duration_ms: u128,
}

// For `aster list --json`
#[derive(Serialize)]
pub struct ProjectInfo {
    pub address: String,
    pub path: String,
    pub plugin: String,
    pub targets: Vec<String>,
}

// For `aster graph --json`
#[derive(Serialize)]
pub struct GraphOutput {
    pub nodes: Vec<String>,
    pub edges: HashMap<String, Vec<String>>,  // adjacency list
}

// For `aster why --json`
#[derive(Serialize)]
pub struct WhyOutput {
    pub from: String,
    pub to: String,
    pub path: Option<Vec<String>>,
}

// Output helper
pub fn output_json<T: Serialize>(value: &T) -> anyhow::Result<()> {
    // Pretty print for human readability if terminal, compact otherwise
    let json = if std::io::stdout().is_terminal() {
        serde_json::to_string_pretty(value)?
    } else {
        serde_json::to_string(value)?
    };
    println!("{}", json);
    Ok(())
}
```

### Pattern 4: Log Storage and Retrieval

**What:** Store execution logs for later retrieval via `aster logs` command.

**When to use:** After every target execution, logs are stored for the `logs` command.

**Example:**
```rust
// Source: CONTEXT.md decisions
use std::fs;
use std::path::{Path, PathBuf};

pub struct LogStore {
    log_dir: PathBuf,  // .aster/logs/
}

#[derive(Serialize, Deserialize)]
pub struct RunLog {
    pub timestamp: String,
    pub target: String,
    pub results: Vec<TargetLog>,
}

#[derive(Serialize, Deserialize)]
pub struct TargetLog {
    pub address: String,
    pub status: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u128,
    pub output: String,  // Full output stored here
}

impl LogStore {
    pub fn new(workspace_root: &Path) -> Self {
        let log_dir = workspace_root.join(".aster").join("logs");
        Self { log_dir }
    }

    /// Store logs from a run
    pub fn store(&self, run: &RunLog) -> anyhow::Result<()> {
        fs::create_dir_all(&self.log_dir)?;

        // Store as "latest.json" (overwritten each run)
        let latest = self.log_dir.join("latest.json");
        let json = serde_json::to_string_pretty(run)?;
        fs::write(latest, json)?;

        Ok(())
    }

    /// Get the latest run log
    pub fn load_latest(&self) -> anyhow::Result<Option<RunLog>> {
        let latest = self.log_dir.join("latest.json");
        if !latest.exists() {
            return Ok(None);
        }

        let content = fs::read_to_string(latest)?;
        let run: RunLog = serde_json::from_str(&content)?;
        Ok(Some(run))
    }

    /// Get log for specific project:target from latest run
    pub fn get_target_log(&self, address: &str) -> anyhow::Result<Option<TargetLog>> {
        let run = self.load_latest()?;
        Ok(run.and_then(|r| {
            r.results.into_iter().find(|t| t.address == address)
        }))
    }
}
```

### Pattern 5: aster logs Command

**What:** New command to retrieve full logs from collapsed runs.

**When to use:** After a run, when user wants full output for a project.

**Example:**
```rust
// Source: CONTEXT.md decisions
// Add to Commands enum
#[derive(Subcommand)]
pub enum Commands {
    // ... existing commands ...

    /// View logs from the last run
    Logs {
        /// Specific project:target to view (e.g., //services/api:test)
        target: Option<String>,
    },
}

// Handler
pub fn handle_logs(target: Option<String>, workspace_root: &Path) -> anyhow::Result<()> {
    let store = LogStore::new(workspace_root);

    match target {
        None => {
            // List all targets from last run with status
            let run = store.load_latest()?
                .ok_or_else(|| anyhow::anyhow!("No previous run found"))?;

            println!("Last run: {} ({})", run.target, run.timestamp);
            println!();

            for result in &run.results {
                let status_icon = match result.status.as_str() {
                    "passed" => style("PASS").green(),
                    "failed" => style("FAIL").red(),
                    "skipped" => style("SKIP").yellow(),
                    _ => style(&result.status).dim(),
                };
                println!("  {} {}", status_icon, result.address);
            }

            println!();
            println!("Use `aster logs <project:target>` to view full output");
        }
        Some(addr) => {
            // Dump full logs for specific target
            match store.get_target_log(&addr)? {
                Some(log) => {
                    println!("--- {} ---", log.address);
                    println!("{}", log.output);
                    println!();
                    println!("[{}] {} ({}ms)",
                        log.status.to_uppercase(),
                        log.address,
                        log.duration_ms
                    );
                }
                None => {
                    // Empty output - project wasn't in last run
                    // (Not an error per CONTEXT.md)
                }
            }
        }
    }

    Ok(())
}
```

### Pattern 6: Failure Presentation

**What:** Show last 10-15 lines of failure output inline with hint.

**When to use:** When a target fails during execution.

**Example:**
```rust
// Source: CONTEXT.md decisions
use console::style;

pub fn display_failure(address: &str, output: &str, exit_code: Option<i32>) {
    println!();
    println!("{} {}", style("FAILED").red().bold(), address);

    if let Some(code) = exit_code {
        println!("Exit code: {}", code);
    }

    println!();

    // Show last 10-15 lines
    let lines: Vec<&str> = output.lines().collect();
    let start = lines.len().saturating_sub(15);

    if start > 0 {
        println!("{}", style(format!("... ({} lines omitted)", start)).dim());
    }

    for line in &lines[start..] {
        println!("  {}", line);
    }

    println!();
    println!("{}", style(format!("Run `aster logs {}` for full output", address)).dim());
}

pub fn display_summary(results: &[ExecutionResult]) {
    let failed: Vec<_> = results.iter().filter(|r| !r.success && !r.skipped).collect();

    if !failed.is_empty() {
        println!();
        println!("{}", style("=== Failed Projects ===").red().bold());
        for result in &failed {
            let code = result.exit_code.map(|c| format!(" (exit {})", c)).unwrap_or_default();
            println!("  {} {}{}", style("X").red(), result.address, code);
        }
    }
}
```

### Anti-Patterns to Avoid

- **Mixing stdout and stderr for different output modes:** JSON goes to stdout; progress/errors go to stderr. Don't mix them or piped JSON will include progress noise.

- **Updating progress too frequently:** indicatif has a 50ms default rate limit. Don't bypass it or you'll consume CPU redrawing.

- **Blocking on progress updates:** Progress updates should be async/non-blocking. Don't wait for render before continuing execution.

- **Hardcoding terminal colors:** Use `console::colors_enabled()` to detect if colors are supported. Respect `NO_COLOR` environment variable.

- **Writing to stdout in JSON mode:** When `--json` is set, only the final JSON blob goes to stdout. All other output (progress, warnings) must go to stderr.

## Don't Hand-Roll

Problems that look simple but have existing solutions:

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| Multi-line terminal progress | ANSI cursor manipulation | `indicatif::MultiProgress` | Handles terminal differences, resize, etc. |
| Terminal color detection | Check `TERM` env var | `console::colors_enabled()` | Handles NO_COLOR, CLICOLOR, pipe detection |
| CLI assertions in tests | Manual process spawning | `assert_cmd::Command` | Clean API, predicate integration |
| Output matching in tests | Manual string comparison | `predicates::str::contains` | Better error messages, regex support |
| Platform-agnostic cache dirs | Hardcode `~/.aster` | `directories::ProjectDirs` | Handles Windows, macOS, Linux correctly |
| JSON serialization | Manual string building | `serde_json::to_string` | Type-safe, handles escaping |

**Key insight:** Terminal UI is deceptively complex. indicatif handles edge cases like terminal resize, non-TTY output, and rate limiting that manual ANSI codes miss.

## Common Pitfalls

### Pitfall 1: Progress Bars Interfere with JSON Output

**What goes wrong:** Progress bars write to stdout, corrupting JSON output.

**Why it happens:** Default progress bar target is stdout.

**How to avoid:** When `--json` is set: (1) write progress to stderr only via `MultiProgress::with_draw_target(ProgressDrawTarget::stderr())`, (2) write JSON to stdout only at the end.

**Warning signs:** JSON parsers fail when piping `aster test --json`.

### Pitfall 2: Tests Depend on Color Codes

**What goes wrong:** Tests pass locally but fail in CI.

**Why it happens:** CI doesn't have a TTY, so colors are disabled.

**How to avoid:** Test the semantic content, not the exact byte output. Use `predicates::str::contains()` for content, not exact match. Or force colors off in tests.

**Warning signs:** Tests fail only in CI with "expected X got Y" where Y has ANSI codes.

### Pitfall 3: Verbose Mode Interleaves Output

**What goes wrong:** In `--verbose` mode, output from parallel projects interleaves illegibly.

**Why it happens:** Streaming output from multiple threads without synchronization.

**How to avoid:** Even in verbose mode, buffer per-project and flush on newlines or a short timeout. Or prefix each line with project address.

**Warning signs:** Verbose output is unreadable when running multiple projects.

### Pitfall 4: Log Storage Grows Unbounded

**What goes wrong:** `.aster/logs/` directory grows indefinitely.

**Why it happens:** Storing every run's logs without cleanup.

**How to avoid:** Per CONTEXT.md decision, only store "latest.json". Previous runs are overwritten. If retention is needed later, implement rotation.

**Warning signs:** N/A - we only store latest per design.

### Pitfall 5: Integration Tests Assume Clean State

**What goes wrong:** Tests pass individually but fail when run together.

**Why it happens:** Tests modify the monorepo state (git status, file changes) and don't clean up.

**How to avoid:** Each test should either: (1) work in a temp copy of the monorepo, or (2) be read-only. For the real monorepo tests, prefer read-only operations.

**Warning signs:** Tests fail with "dirty working tree" or unexpected project state.

### Pitfall 6: Exit Code Doesn't Reflect Failures

**What goes wrong:** CI doesn't detect failures because aster exits 0.

**Why it happens:** Success path doesn't check for any failed results.

**How to avoid:** Per CONTEXT.md: exit code 1 if any target fails. Check after all execution completes.

**Warning signs:** CI shows green but targets actually failed.

## Code Examples

Verified patterns from official sources:

### indicatif MultiProgress with Spinner Template
```rust
// Source: indicatif docs + examples/multi.rs
use indicatif::{MultiProgress, ProgressBar, ProgressStyle, ProgressDrawTarget};
use std::time::Duration;

let multi = MultiProgress::with_draw_target(ProgressDrawTarget::stderr());

let style = ProgressStyle::with_template(
    "{prefix:.bold.dim} {spinner} {wide_msg}"
)
.unwrap()
.tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ ");

let pb = multi.add(ProgressBar::new_spinner());
pb.set_style(style);
pb.set_prefix("//services/api:test");
pb.enable_steady_tick(Duration::from_millis(100));
pb.set_message("Running tests...");

// Later...
pb.finish_with_message("PASS (1.2s)");
```

### console Terminal Detection
```rust
// Source: console docs
use console::{Term, style, colors_enabled};
use std::io::IsTerminal;

// Check if colors should be used
if colors_enabled() {
    println!("{}", style("Success").green().bold());
} else {
    println!("Success");
}

// Check if interactive terminal
if std::io::stdout().is_terminal() {
    // Show progress bars
} else {
    // Simple line-by-line output for pipes/CI
}
```

### assert_cmd Integration Test
```rust
// Source: assert_cmd docs
use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn test_list_json_output() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("aster")?;
    cmd.arg("list")
        .arg("--json")
        .current_dir("/path/to/monorepo");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("//services/api"))
        .stdout(predicate::str::is_match(r#""address":\s*"//[^"]+""#)?);

    Ok(())
}

#[test]
fn test_help_output() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("aster")?;
    cmd.arg("--help");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Build orchestration for polyglot monorepos"));

    Ok(())
}
```

### JSON Output with Conditional Formatting
```rust
// Source: Rust CLI book machine-communication
use serde::Serialize;
use std::io::IsTerminal;

pub fn output_json<T: Serialize>(value: &T) -> anyhow::Result<()> {
    let stdout = std::io::stdout();
    let writer = stdout.lock();

    if stdout.is_terminal() {
        // Pretty for humans looking at terminal
        serde_json::to_writer_pretty(writer, value)?;
    } else {
        // Compact for piping to other tools
        serde_json::to_writer(writer, value)?;
    }
    println!(); // Ensure newline at end

    Ok(())
}
```

## State of the Art

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|--------------|--------|
| Sequential output per project | Multi-line live progress | Nx 14+ / Turborepo 1.0+ | Better UX for parallel execution |
| Exit code only | Structured JSON output | Always best practice | CI/CD integration, scripting |
| Manual ANSI codes | indicatif/console abstractions | indicatif 0.17 (2023) | Cross-platform, handles edge cases |
| Print to stdout always | stderr for progress, stdout for data | Rust CLI book convention | Proper pipe handling |

**Deprecated/outdated:**
- indicatif 0.16: Replaced by 0.17+ with 95x performance improvement
- `termion` for progress: `indicatif` is the standard now
- `colored` crate alone: `console` provides better integration with indicatif

## Open Questions

Things that couldn't be fully resolved:

1. **Exact progress bar template for best visual**
   - What we know: indicatif supports rich templates with spinners, elapsed time, messages
   - What's unclear: Exact visual format that looks like Nx/Turborepo
   - Recommendation: Start with spinner + project:target + status. Iterate based on feedback.

2. **--parallel flag integration**
   - What we know: Phase 3 uses DAG-level parallelism by default
   - What's unclear: Should Phase 4 add `--parallel=N` to limit concurrency?
   - Recommendation: Not in scope per requirements. Add if requested.

3. **Log rotation/retention policy**
   - What we know: CONTEXT.md says only store "latest"
   - What's unclear: Users might want history for debugging
   - Recommendation: Start with latest-only. Add `--keep-logs=N` later if requested.

4. **Progress display when terminal too narrow**
   - What we know: indicatif handles some truncation
   - What's unclear: Behavior with very long project paths
   - Recommendation: Use `{prefix:.30}` to truncate prefix in template if needed.

## Sources

### Primary (HIGH confidence)
- [indicatif MultiProgress](https://docs.rs/indicatif/latest/indicatif/struct.MultiProgress.html) - API for concurrent progress bars
- [indicatif ProgressBar](https://docs.rs/indicatif/latest/indicatif/struct.ProgressBar.html) - Spinner, message, prefix API
- [indicatif ProgressStyle](https://docs.rs/indicatif/latest/indicatif/style/struct.ProgressStyle.html) - Template format documentation
- [console crate](https://docs.rs/console/latest/console/) - Terminal detection, colors, styling
- [assert_cmd](https://docs.rs/assert_cmd/latest/assert_cmd/) - CLI testing integration
- [predicates](https://docs.rs/predicates/latest/predicates/) - Assertion matchers
- [Rust CLI book - Machine Communication](https://rust-cli.github.io/book/in-depth/machine-communication.html) - JSON output best practices

### Secondary (MEDIUM confidence)
- [clap-verbosity-flag](https://github.com/clap-rs/clap-verbosity-flag) - Pattern for verbose/quiet flags
- [directories crate](https://crates.io/crates/directories) - Platform-agnostic directory paths
- [Nx Terminal UI docs](https://nx.dev/docs/guides/tasks--caching/terminal-ui) - UI inspiration (limited detail)

### Tertiary (LOW confidence)
- [Turborepo TUI overview](https://deepwiki.com/vercel/turborepo/5.1-terminal-ui) - Uses ratatui, different approach

## Metadata

**Confidence breakdown:**
- Terminal UI (indicatif): HIGH - well-documented, widely used, verified API
- JSON output: HIGH - serde_json is standard, patterns documented
- Global flags: HIGH - clap global arg pattern is documented
- Integration testing: HIGH - assert_cmd/predicates are standard
- Log storage: MEDIUM - design is custom, location is our choice
- Progress display exact format: MEDIUM - inspired by Nx/Turborepo but our design

**Research date:** 2026-01-23
**Valid until:** 60 days (indicatif, console, assert_cmd are stable crates)
