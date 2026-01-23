//! Color scheme for status indicators
//!
//! Provides consistent styled output for pass/fail/skip/running states.

use console::{style, StyledObject};

/// Green "PASS" indicator for successful targets
pub fn status_pass() -> StyledObject<&'static str> {
    style("PASS").green().bold()
}

/// Red "FAIL" indicator for failed targets
pub fn status_fail() -> StyledObject<&'static str> {
    style("FAIL").red().bold()
}

/// Yellow "SKIP" indicator for skipped targets
pub fn status_skip() -> StyledObject<&'static str> {
    style("SKIP").yellow().bold()
}

/// Cyan "RUNNING" indicator for in-progress targets
pub fn status_running() -> StyledObject<&'static str> {
    style("RUNNING").cyan().bold()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_indicators_exist() {
        // Just verify these don't panic
        let _ = status_pass();
        let _ = status_fail();
        let _ = status_skip();
        let _ = status_running();
    }
}
