//! Progress display for multi-project execution
//!
//! Shows a status bar with counts and spinners for running targets.
//! Writes to stderr so JSON output on stdout remains clean.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

/// Manages progress display for parallel target execution
pub struct ProgressDisplay {
    /// Multi-progress manager for coordinating spinners
    multi: MultiProgress,
    /// Status bar showing overall progress
    status_bar: Option<ProgressBar>,
    /// Active progress bars keyed by target address
    bars: HashMap<String, ProgressBar>,
    /// Whether progress display is enabled
    enabled: bool,
    /// Counters for status display
    total: Arc<AtomicUsize>,
    passed: Arc<AtomicUsize>,
    failed: Arc<AtomicUsize>,
    skipped: Arc<AtomicUsize>,
    running: Arc<AtomicUsize>,
}

impl ProgressDisplay {
    /// Create a new progress display
    ///
    /// If not enabled, creates with hidden draw target (no output).
    /// When enabled, draws to stderr to not interfere with JSON on stdout.
    pub fn new(enabled: bool) -> Self {
        let multi = if enabled {
            MultiProgress::with_draw_target(ProgressDrawTarget::stderr())
        } else {
            MultiProgress::with_draw_target(ProgressDrawTarget::hidden())
        };

        Self {
            multi,
            status_bar: None,
            bars: HashMap::new(),
            enabled,
            total: Arc::new(AtomicUsize::new(0)),
            passed: Arc::new(AtomicUsize::new(0)),
            failed: Arc::new(AtomicUsize::new(0)),
            skipped: Arc::new(AtomicUsize::new(0)),
            running: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Set the total number of targets to run
    pub fn set_total(&mut self, total: usize) {
        self.total.store(total, Ordering::SeqCst);

        if self.enabled {
            // Create status bar at the top
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
        let running = self.running.load(Ordering::SeqCst);
        let completed = passed + failed + self.skipped.load(Ordering::SeqCst);
        let remaining = total.saturating_sub(completed).saturating_sub(running);

        let mut parts = Vec::new();

        if running > 0 {
            parts.push(format!("{} running", style(running).cyan()));
        }

        if passed > 0 {
            parts.push(format!("{} {}", style(passed).green(), style("passed").green()));
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
    pub fn add_running(&mut self, address: &str) {
        if !self.enabled {
            return;
        }

        self.running.fetch_add(1, Ordering::SeqCst);
        self.refresh_status();

        let pb = self.multi.add(ProgressBar::new_spinner());

        let style = ProgressStyle::default_spinner()
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏")
            .template("  {spinner:.yellow} {prefix:.dim}")
            .expect("Invalid progress template");

        pb.set_style(style);
        pb.set_prefix(address.to_string());
        pb.enable_steady_tick(Duration::from_millis(100));

        self.bars.insert(address.to_string(), pb);
    }

    /// Mark a target as skipped (no spinner shown, just count)
    pub fn mark_skipped(&mut self, _address: &str) {
        self.skipped.fetch_add(1, Ordering::SeqCst);
        self.refresh_status();
    }

    /// Mark a target as complete
    ///
    /// Removes spinner and updates counts. Only failures are shown briefly.
    pub fn mark_complete(&mut self, address: &str, success: bool, skipped: bool, _duration_ms: u128) {
        self.running.fetch_sub(1, Ordering::SeqCst);

        if skipped {
            self.skipped.fetch_add(1, Ordering::SeqCst);
        } else if success {
            self.passed.fetch_add(1, Ordering::SeqCst);
        } else {
            self.failed.fetch_add(1, Ordering::SeqCst);
        }

        // Remove the spinner - completed items don't stay on screen
        if let Some(pb) = self.bars.remove(address) {
            if !success && !skipped {
                // Show failures briefly before clearing
                pb.finish_with_message(format!("{} {}", style("✗").red(), address));
            } else {
                // Just clear - don't show passes or skips
                pb.finish_and_clear();
            }
        }

        self.refresh_status();
    }

    /// Print a message above the progress bars
    ///
    /// Uses multi.println() to avoid interfering with spinner output.
    pub fn println(&self, msg: &str) {
        if self.enabled {
            let _ = self.multi.println(msg);
        }
    }

    /// Check if progress display is enabled
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Finish the progress display
    pub fn finish(&self) {
        if let Some(ref bar) = self.status_bar {
            let passed = self.passed.load(Ordering::SeqCst);
            let failed = self.failed.load(Ordering::SeqCst);

            let status_icon = if failed > 0 {
                style("✗").red()
            } else {
                style("✓").green()
            };

            let mut parts = Vec::new();
            parts.push(format!("{} passed", style(passed).green()));

            if failed > 0 {
                parts.push(format!("{} failed", style(failed).red()));
            }

            bar.finish_with_message(format!("{} {}", status_icon, parts.join(", ")));
        }

        // Clear any remaining spinners
        for (_, pb) in &self.bars {
            pb.finish_and_clear();
        }
    }
}

/// Format duration in human-readable form
pub fn format_duration(duration_ms: u128) -> String {
    if duration_ms < 1000 {
        format!("{}ms", duration_ms)
    } else if duration_ms < 60_000 {
        format!("{:.1}s", duration_ms as f64 / 1000.0)
    } else {
        let minutes = duration_ms / 60_000;
        let seconds = (duration_ms % 60_000) / 1000;
        format!("{}m{}s", minutes, seconds)
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
}
