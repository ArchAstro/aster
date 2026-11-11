//! Where an `aster affected` run spends its time before the first target
//! starts, as one stderr line.

use std::time::{Duration, Instant};

/// Named durations in the order the phases ran. A name marked twice adds up.
pub struct Phases {
    last: Instant,
    items: Vec<(&'static str, Duration)>,
}

impl Phases {
    /// Start timing; `earlier` is time already spent (project discovery).
    pub fn new(earlier: Duration) -> Phases {
        Phases {
            last: Instant::now(),
            items: vec![("discover", earlier)],
        }
    }

    /// Charge the time since the previous mark to `name`.
    pub fn mark(&mut self, name: &'static str) {
        let spent = self.last.elapsed();
        self.last = Instant::now();
        if let Some(entry) = self.items.iter_mut().find(|(n, _)| *n == name) {
            entry.1 += spent;
        } else {
            self.items.push((name, spent));
        }
    }

    /// `affected phases: discover 0.4s, changes 0.1s, ...  total 24.3s`.
    pub fn line(&self, replayed: bool) -> String {
        let total: Duration = self.items.iter().map(|(_, spent)| *spent).sum();
        let mut parts: Vec<String> = self
            .items
            .iter()
            .map(|(name, spent)| format!("{name} {:.1}s", spent.as_secs_f64()))
            .collect();
        parts.push(format!("total {:.1}s", total.as_secs_f64()));
        let how = if replayed { " (replayed plan)" } else { "" };
        format!("[aster] affected phases{how}: {}", parts.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phases_list_in_order_and_a_repeated_name_adds_up() {
        let mut phases = Phases::new(Duration::from_millis(400));
        phases.mark("changes");
        phases.mark("narrow");
        phases.mark("narrow");
        let line = phases.line(false);
        assert!(line.starts_with("[aster] affected phases: discover 0.4s, changes "));
        assert_eq!(line.matches("narrow").count(), 1, "{line}");
        assert!(line.contains(", total "), "{line}");
        assert!(phases.line(true).contains("(replayed plan)"));
    }
}
