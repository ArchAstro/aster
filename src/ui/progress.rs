//! Progress display for multi-project execution
//!
//! Supports three display modes:
//! - Concise (default): Status bar with counts, spinners for running targets
//! - Verbose: Shows each target with PASS/FAIL/SKIP and duration
//! - CI: Simple line-by-line output with timestamps (auto-enabled when stderr is not a TTY)
//!
//! Writes to stderr so JSON output on stdout remains clean.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::Local;
use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

/// Format current time as [HH:MM:SS] for CI output
fn format_timestamp() -> String {
    Local::now().format("[%H:%M:%S]").to_string()
}

/// Manages progress display for parallel target execution
pub struct ProgressDisplay {
    /// Multi-progress manager for coordinating spinners
    multi: MultiProgress,
    /// Status bar showing overall progress (concise mode only)
    status_bar: Option<ProgressBar>,
    /// Active progress bars keyed by target address
    bars: HashMap<String, ProgressBar>,
    /// Whether progress display is enabled
    enabled: bool,
    /// Verbose mode shows each item with PASS/FAIL/SKIP
    verbose: bool,
    /// CI mode: simple line-by-line output with timestamps (no spinners)
    ci_mode: bool,
    /// Counters for status display
    total: Arc<AtomicUsize>,
    passed: Arc<AtomicUsize>,
    failed: Arc<AtomicUsize>,
    skipped: Arc<AtomicUsize>,
    cached: Arc<AtomicUsize>,
    running: Arc<AtomicUsize>,
}

impl ProgressDisplay {
    /// Create a new progress display
    ///
    /// If not enabled, creates with hidden draw target (no output).
    /// When enabled, draws to stderr to not interfere with JSON on stdout.
    /// Automatically enables CI mode when stderr is not a TTY.
    pub fn new(enabled: bool) -> Self {
        Self::with_verbose(enabled, false)
    }

    /// Create a new progress display with verbose mode option
    ///
    /// - Concise mode (verbose=false): Status bar with counts, clean output
    /// - Verbose mode (verbose=true): Shows each target with PASS/FAIL/SKIP and duration
    /// - CI mode (auto-detected): Simple line-by-line output with timestamps
    pub fn with_verbose(enabled: bool, verbose: bool) -> Self {
        // Auto-detect CI mode: when stderr is not a terminal, use simple line output
        let ci_mode = enabled && !std::io::stderr().is_terminal();

        // In CI mode, don't use MultiProgress (no spinners needed)
        let multi = if enabled && !ci_mode {
            MultiProgress::with_draw_target(ProgressDrawTarget::stderr())
        } else {
            MultiProgress::with_draw_target(ProgressDrawTarget::hidden())
        };

        Self {
            multi,
            status_bar: None,
            bars: HashMap::new(),
            enabled,
            verbose,
            ci_mode,
            total: Arc::new(AtomicUsize::new(0)),
            passed: Arc::new(AtomicUsize::new(0)),
            failed: Arc::new(AtomicUsize::new(0)),
            skipped: Arc::new(AtomicUsize::new(0)),
            cached: Arc::new(AtomicUsize::new(0)),
            running: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Set the total number of targets to run
    pub fn set_total(&mut self, total: usize) {
        self.total.store(total, Ordering::SeqCst);

        // Only create status bar in concise mode (not verbose, not CI)
        if self.enabled && !self.verbose && !self.ci_mode {
            let status = self.multi.add(ProgressBar::new_spinner());
            let style = ProgressStyle::default_spinner()
                .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏")
                .template("{spinner:.cyan} {msg}")
                .expect("Invalid progress template");
            status.set_style(style);
            status.enable_steady_tick(Duration::from_millis(100));
            self.update_status_message(&status);
            self.status_bar = Some(status);
        }
    }

    /// Update the status bar message with current counts
    fn update_status_message(&self, bar: &ProgressBar) {
        let total = self.total.load(Ordering::SeqCst);
        let passed = self.passed.load(Ordering::SeqCst);
        let failed = self.failed.load(Ordering::SeqCst);
        let cached = self.cached.load(Ordering::SeqCst);
        let running = self.running.load(Ordering::SeqCst);
        let completed = passed + failed + cached + self.skipped.load(Ordering::SeqCst);
        let remaining = total.saturating_sub(completed).saturating_sub(running);

        let mut parts = Vec::new();

        if running > 0 {
            parts.push(format!("{} running", style(running).cyan()));
        }

        if passed > 0 {
            parts.push(format!(
                "{} {}",
                style(passed).green(),
                style("passed").green()
            ));
        }

        if cached > 0 {
            parts.push(format!(
                "{} {}",
                style(cached).cyan(),
                style("cached").cyan()
            ));
        }

        if failed > 0 {
            parts.push(format!("{} {}", style(failed).red(), style("failed").red()));
        }

        if remaining > 0 {
            parts.push(format!("{} remaining", style(remaining).dim()));
        }

        let msg = if parts.is_empty() {
            "Starting...".to_string()
        } else {
            parts.join(" │ ")
        };

        bar.set_message(msg);
    }

    /// Refresh the status bar
    fn refresh_status(&self) {
        if let Some(ref bar) = self.status_bar {
            self.update_status_message(bar);
        }
    }

    /// Add a running spinner for a target
    ///
    /// Creates a spinner with the target address as prefix and enables steady tick.
    /// In CI mode, prints a START message instead.
    pub fn add_running(&mut self, address: &str) {
        if !self.enabled {
            return;
        }

        self.running.fetch_add(1, Ordering::SeqCst);

        // CI mode: print START message and return (no spinner)
        if self.ci_mode {
            eprintln!(
                "{} {} {}",
                style(format_timestamp()).dim(),
                style("START").cyan(),
                address
            );
            return;
        }

        self.refresh_status();

        let pb = self.multi.add(ProgressBar::new_spinner());

        let template = if self.verbose {
            "{prefix:.bold.cyan} [{spinner:.yellow}]"
        } else {
            "  {spinner:.yellow} {prefix:.dim}"
        };

        let style = ProgressStyle::default_spinner()
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏")
            .template(template)
            .expect("Invalid progress template");

        pb.set_style(style);
        pb.set_prefix(address.to_string());
        pb.enable_steady_tick(Duration::from_millis(100));

        self.bars.insert(address.to_string(), pb);
    }

    /// Mark a target as skipped (no spinner shown, just count)
    pub fn mark_skipped(&mut self, address: &str) {
        self.skipped.fetch_add(1, Ordering::SeqCst);
        self.refresh_status();

        // CI mode: print SKIP with timestamp
        if self.enabled && self.ci_mode {
            eprintln!(
                "{} {} {}",
                style(format_timestamp()).dim(),
                style("SKIP").yellow(),
                address
            );
            return;
        }

        // In verbose mode, show the skip
        if self.enabled && self.verbose {
            let msg = format!("{} {}", style("SKIP").yellow(), address);
            let _ = self.multi.println(msg);
        }
    }

    /// Mark a target as cached (no execution needed)
    pub fn mark_cached(&mut self, _address: &str) {
        self.cached.fetch_add(1, Ordering::SeqCst);
        self.refresh_status();
    }

    /// Mark a target as complete
    ///
    /// Updates counts and removes spinner. Behavior differs by mode:
    /// - Concise: Only failures shown briefly, passes/skips cleared silently
    /// - Verbose: Shows PASS/FAIL with duration for each target
    /// - CI: Shows PASS/FAIL with timestamp and duration
    pub fn mark_complete(
        &mut self,
        address: &str,
        success: bool,
        skipped: bool,
        duration_ms: u128,
    ) {
        // Skipped items are handled by mark_skipped(), don't process again
        if skipped {
            return;
        }

        // Decrement running count (only for non-skipped items that had spinners)
        self.running.fetch_sub(1, Ordering::SeqCst);

        if success {
            self.passed.fetch_add(1, Ordering::SeqCst);
        } else {
            self.failed.fetch_add(1, Ordering::SeqCst);
        }

        // CI mode: print PASS/FAIL with timestamp and duration
        if self.ci_mode {
            let duration_str = format_duration(duration_ms);
            if success {
                eprintln!(
                    "{} {} {} ({})",
                    style(format_timestamp()).dim(),
                    style("PASS").green(),
                    address,
                    style(duration_str).dim()
                );
            } else {
                eprintln!(
                    "{} {} {} ({})",
                    style(format_timestamp()).dim(),
                    style("FAIL").red(),
                    address,
                    style(duration_str).dim()
                );
            }
            return;
        }

        // Remove the spinner and show appropriate output
        if let Some(pb) = self.bars.remove(address) {
            if self.verbose {
                // Verbose mode: show each result with duration
                let duration_str = format_duration(duration_ms);
                if success {
                    pb.finish_with_message(format!(
                        "{} {} ({})",
                        style("PASS").green(),
                        address,
                        style(duration_str).dim()
                    ));
                } else {
                    pb.finish_with_message(format!(
                        "{} {} ({})",
                        style("FAIL").red(),
                        address,
                        style(duration_str).dim()
                    ));
                }
            } else {
                // Concise mode: clear passes, show failures briefly
                if !success {
                    pb.finish_with_message(format!("{} {}", style("✗").red(), address));
                } else {
                    pb.finish_and_clear();
                }
            }
        }

        self.refresh_status();
    }

    /// Print a message above the progress bars
    ///
    /// Uses multi.println() to avoid interfering with spinner output.
    /// In CI mode, uses eprintln! directly since there are no spinners.
    pub fn println(&self, msg: &str) {
        if self.enabled {
            if self.ci_mode {
                eprintln!("{msg}");
            } else {
                let _ = self.multi.println(msg);
            }
        }
    }

    /// Check if progress display is enabled
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Finish the progress display
    pub fn finish(&self) {
        // CI mode: no spinners or status bar to clean up
        if self.ci_mode {
            return;
        }

        // Clear any remaining spinners
        for pb in self.bars.values() {
            pb.finish_and_clear();
        }

        if let Some(ref bar) = self.status_bar {
            // Concise mode: update status bar with final summary
            let passed = self.passed.load(Ordering::SeqCst);
            let failed = self.failed.load(Ordering::SeqCst);
            let cached = self.cached.load(Ordering::SeqCst);

            let status_icon = if failed > 0 {
                style("✗").red()
            } else {
                style("✓").green()
            };

            let mut parts = Vec::new();
            parts.push(format!("{} passed", style(passed).green()));

            if cached > 0 {
                parts.push(format!("{} cached", style(cached).cyan()));
            }

            if failed > 0 {
                parts.push(format!("{} failed", style(failed).red()));
            }

            bar.finish_with_message(format!("{} {}", status_icon, parts.join(", ")));
        }
    }

    /// Check if verbose mode is enabled
    pub fn is_verbose(&self) -> bool {
        self.verbose
    }

    /// Check if CI mode is enabled
    pub fn is_ci_mode(&self) -> bool {
        self.ci_mode
    }
}

/// Format duration in human-readable form
pub fn format_duration(duration_ms: u128) -> String {
    if duration_ms < 1000 {
        format!("{duration_ms}ms")
    } else if duration_ms < 60_000 {
        format!("{:.1}s", duration_ms as f64 / 1000.0)
    } else {
        let minutes = duration_ms / 60_000;
        let seconds = (duration_ms % 60_000) / 1000;
        format!("{minutes}m{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_progress_display_disabled() {
        let progress = ProgressDisplay::new(false);
        assert!(!progress.is_enabled());
    }

    #[test]
    fn test_progress_display_enabled() {
        let progress = ProgressDisplay::new(true);
        assert!(progress.is_enabled());
    }

    #[test]
    fn test_format_duration_milliseconds() {
        assert_eq!(format_duration(500), "500ms");
        assert_eq!(format_duration(0), "0ms");
        assert_eq!(format_duration(999), "999ms");
    }

    #[test]
    fn test_format_duration_seconds() {
        assert_eq!(format_duration(1000), "1.0s");
        assert_eq!(format_duration(1500), "1.5s");
        assert_eq!(format_duration(59999), "60.0s");
    }

    #[test]
    fn test_format_duration_minutes() {
        assert_eq!(format_duration(60000), "1m0s");
        assert_eq!(format_duration(90000), "1m30s");
        assert_eq!(format_duration(125000), "2m5s");
    }

    #[test]
    fn test_counters() {
        let mut progress = ProgressDisplay::new(false);
        progress.set_total(10);

        assert_eq!(progress.total.load(Ordering::SeqCst), 10);
        assert_eq!(progress.passed.load(Ordering::SeqCst), 0);
        assert_eq!(progress.failed.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn test_format_timestamp() {
        let ts = format_timestamp();
        // Should match [HH:MM:SS] format
        assert!(ts.starts_with('['));
        assert!(ts.ends_with(']'));
        assert_eq!(ts.len(), 10); // "[HH:MM:SS]"
    }

    #[test]
    fn test_ci_mode_disabled_when_not_enabled() {
        // When progress is disabled, CI mode should also be false
        let progress = ProgressDisplay::new(false);
        assert!(!progress.is_ci_mode());
    }

    #[test]
    fn test_is_ci_mode_accessor() {
        // When enabled, CI mode depends on terminal detection
        // In tests, stderr is typically not a terminal, so CI mode would be true
        let progress = ProgressDisplay::new(true);
        // Just verify the accessor works - actual value depends on test environment
        let _ = progress.is_ci_mode();
    }
}
