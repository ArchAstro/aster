//! Progress display for multi-project execution
//!
//! Shows spinners per running project using indicatif's MultiProgress.
//! Writes to stderr so JSON output on stdout remains clean.

use std::collections::HashMap;
use std::time::Duration;

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

use super::colors;

/// Manages progress display for parallel target execution
pub struct ProgressDisplay {
    /// Multi-progress manager for coordinating spinners
    multi: MultiProgress,
    /// Active progress bars keyed by target address
    bars: HashMap<String, ProgressBar>,
    /// Whether progress display is enabled
    enabled: bool,
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
            bars: HashMap::new(),
            enabled,
        }
    }

    /// Add a running spinner for a target
    ///
    /// Creates a spinner with the target address as prefix and enables steady tick.
    pub fn add_running(&mut self, address: &str) -> ProgressBar {
        let pb = self.multi.add(ProgressBar::new_spinner());

        let style = ProgressStyle::default_spinner()
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏")
            .template("{prefix:.bold.cyan} [{spinner:.yellow}] {msg}")
            .expect("Invalid progress template");

        pb.set_style(style);
        pb.set_prefix(address.to_string());
        pb.enable_steady_tick(Duration::from_millis(100));

        self.bars.insert(address.to_string(), pb.clone());
        pb
    }

    /// Update the message for a running target
    ///
    /// Typically shows the last 2-3 lines of output.
    pub fn update_message(&self, address: &str, msg: &str) {
        if let Some(pb) = self.bars.get(address) {
            pb.set_message(msg.to_string());
        }
    }

    /// Mark a target as complete
    ///
    /// Removes from active bars and shows final status with duration.
    pub fn mark_complete(&mut self, address: &str, success: bool, skipped: bool, duration_ms: u128) {
        if let Some(pb) = self.bars.remove(address) {
            let status = if skipped {
                colors::status_skip()
            } else if success {
                colors::status_pass()
            } else {
                colors::status_fail()
            };

            let duration_str = format_duration(duration_ms);
            let final_msg = format!("{} {} ({})", status, address, duration_str);

            pb.finish_with_message(final_msg);
        }
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
}

/// Format duration in human-readable form
fn format_duration(duration_ms: u128) -> String {
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
    fn test_add_running_and_complete() {
        let mut progress = ProgressDisplay::new(false); // Disabled for test

        let _pb = progress.add_running("//test:build");
        assert!(progress.bars.contains_key("//test:build"));

        progress.mark_complete("//test:build", true, false, 1000);
        assert!(!progress.bars.contains_key("//test:build"));
    }
}
