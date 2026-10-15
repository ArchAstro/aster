//! The dashboard's pages. Each returns the page body; the frame comes from
//! [`super::html`]. Everything interpolated is escaped.

use std::fmt::Write;

use super::data::{
    Attempt, Cluster, Leaderboard, QuarantineChange, Quarantined, TestRow, BROKEN_LIMIT,
    CLUSTER_DAYS, CLUSTER_LIMIT, FLAKY_LIMIT, HISTORY_LIMIT, QUARANTINE_LIMIT, SCORED_LIMIT,
    TABLE_ROWS,
};
use super::html::{escape, short_sha, test_link, when, when_opt};
use crate::config::QuarantineRules;

/// Shown on every page when no repository has runs.
pub fn no_runs() -> String {
    "<p class=\"empty\">No runs have been uploaded yet.</p>".to_string()
}

fn test_cell(test: &TestRow, repo: &str) -> String {
    format!(
        "<td class=\"id\"><a href=\"{}\">{}</a><br><span class=\"muted\">{}</span></td>",
        escape(&test_link(&test.test_id, repo)),
        escape(&test.name),
        escape(&test.test_id),
    )
}

fn place_cell(test: &TestRow) -> String {
    let file = test.file.as_deref().map(|f| format!("<br>{}", escape(f)));
    format!(
        "<td class=\"id\"><code>{}</code>{}</td>",
        escape(&test.target),
        file.unwrap_or_default()
    )
}

fn state_tag(state: &str) -> String {
    format!(
        "<span class=\"tag {}\">{}</span>",
        escape(state),
        escape(state)
    )
}

fn capped(shown: usize, limit: i64) -> String {
    if shown as i64 >= limit {
        format!("<p class=\"muted\">Showing the first {limit}.</p>")
    } else {
        String::new()
    }
}

pub fn leaderboard(repo: &str, board: &Leaderboard) -> String {
    let mut out = String::new();
    out.push_str("<h2>Flaky</h2>");
    if board.flaky.is_empty() && board.scored.is_empty() {
        out.push_str(
            "<p class=\"empty\">No flaky tests. Nothing has flaked in this repository.</p>",
        );
    } else {
        out.push_str(
            "<div class=\"scroll\"><table><thead><tr><th>Test</th><th>State</th>\
             <th>Score</th><th>Last flake</th><th>Last seen</th><th>Target / file</th></tr></thead><tbody>",
        );
        for test in board.flaky.iter().chain(&board.scored) {
            let _ = write!(
                out,
                "<tr>{}<td>{}</td><td class=\"num\">{}</td><td>{}</td><td>{}</td>{}</tr>",
                test_cell(test, repo),
                state_tag(&test.state),
                test.flake_score,
                escape(&when_opt(test.last_flake_at)),
                escape(&when(test.last_seen)),
                place_cell(test),
            );
        }
        out.push_str("</tbody></table></div>");
        out.push_str(&capped(board.flaky.len(), FLAKY_LIMIT));
        out.push_str(&capped(board.scored.len(), SCORED_LIMIT));
    }

    out.push_str("<h2>Broken</h2>");
    if board.broken.is_empty() {
        out.push_str("<p class=\"empty\">No broken tests.</p>");
    } else {
        out.push_str(
            "<div class=\"scroll\"><table><thead><tr><th>Test</th><th>Broken since</th>\
             <th>Last seen</th><th>Target / file</th></tr></thead><tbody>",
        );
        for test in &board.broken {
            let since = test
                .broken_since_sha
                .as_deref()
                .map_or_else(|| "-".to_string(), |sha| escape(short_sha(sha)));
            let _ = write!(
                out,
                "<tr>{}<td><code>{since}</code></td><td>{}</td>{}</tr>",
                test_cell(test, repo),
                escape(&when(test.last_seen)),
                place_cell(test),
            );
        }
        out.push_str("</tbody></table></div>");
        out.push_str(&capped(board.broken.len(), BROKEN_LIMIT));
    }
    out
}

/// A failure message: inline when short, collapsed when long.
fn message(text: Option<&str>) -> String {
    let Some(text) = text.filter(|t| !t.is_empty()) else {
        return "<span class=\"muted\">-</span>".to_string();
    };
    let first_line = text.lines().next().unwrap_or("");
    let short = first_line.chars().count() <= 120 && !text.contains('\n');
    if short {
        return format!("<code>{}</code>", escape(text));
    }
    let preview: String = first_line.chars().take(120).collect();
    format!(
        "<details><summary>{}…</summary><pre>{}</pre></details>",
        escape(&preview),
        escape(text)
    )
}

fn cell_class(outcome: &str) -> &'static str {
    match outcome {
        "pass" => "pass",
        "fail" => "fail",
        "error" => "error",
        _ => "skip",
    }
}

/// `attempts` is newest first, as read.
pub fn test_detail(test: &TestRow, attempts: &[Attempt], retention_days: u32) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        "<dl><dt>Test</dt><dd>{name}</dd><dt>ID</dt><dd><code>{id}</code></dd>\
         <dt>Target</dt><dd><code>{target}</code></dd>",
        name = escape(&test.name),
        id = escape(&test.test_id),
        target = escape(&test.target),
    );
    if let Some(file) = &test.file {
        let _ = write!(out, "<dt>File</dt><dd>{}</dd>", escape(file));
    }
    let _ = write!(
        out,
        "<dt>State</dt><dd>{}</dd><dt>Flake score</dt><dd>{}</dd>\
         <dt>Last flake</dt><dd>{}</dd><dt>First seen</dt><dd>{}</dd>\
         <dt>Last seen</dt><dd>{}</dd>",
        state_tag(&test.state),
        test.flake_score,
        escape(&when_opt(test.last_flake_at)),
        escape(&when(test.first_seen)),
        escape(&when(test.last_seen)),
    );
    if test.state == "broken" || test.broken_since_sha.is_some() {
        let _ = write!(
            out,
            "<dt>Broken since</dt><dd><code>{}</code></dd>",
            escape(test.broken_since_sha.as_deref().unwrap_or("-"))
        );
    }
    out.push_str("</dl>");

    out.push_str("<h2>Recent attempts</h2>");
    if attempts.is_empty() {
        let _ = write!(
            out,
            "<p class=\"empty\">No results in the last {retention_days} days.</p>"
        );
        return out;
    }
    let _ = write!(
        out,
        "<p class=\"legend\">One cell per attempt, oldest on the left, newest on the right \
         (the last {} within {retention_days} days). \
         <i class=\"pass\"></i>pass\
         <i class=\"fail\"></i>fail\
         <i class=\"error\"></i>error\
         <i class=\"skip\"></i>skip</p><div class=\"strip\">",
        attempts.len()
    );
    for a in attempts.iter().rev() {
        let tip = format!(
            "{} {} {} attempt {} {}",
            short_sha(&a.sha),
            a.branch.as_deref().unwrap_or("no branch"),
            a.outcome,
            a.attempt,
            when(a.at),
        );
        let _ = write!(
            out,
            "<span class=\"cell {}\" role=\"img\" title=\"{tip}\" aria-label=\"{tip}\"></span>",
            cell_class(&a.outcome),
            tip = escape(&tip),
        );
    }
    out.push_str("</div>");
    out.push_str(&capped(attempts.len(), HISTORY_LIMIT));

    out.push_str("<h2>Results, newest first</h2>");
    out.push_str(
        "<div class=\"scroll\"><table><thead><tr><th>When</th><th>Outcome</th><th>Commit</th>\
         <th>Branch</th><th>Attempt</th><th>Duration</th><th>Message</th></tr></thead><tbody>",
    );
    for a in attempts.iter().take(TABLE_ROWS) {
        let duration = a
            .duration_ms
            .map_or_else(|| "-".to_string(), |ms| format!("{ms} ms"));
        let _ = write!(
            out,
            "<tr><td>{}</td><td>{}</td><td><code>{}</code></td><td>{}</td>\
             <td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"id\">{}</td></tr>",
            escape(&when(a.at)),
            state_outcome(&a.outcome),
            escape(short_sha(&a.sha)),
            escape(a.branch.as_deref().unwrap_or("-")),
            a.attempt,
            escape(&duration),
            message(a.message.as_deref()),
        );
    }
    out.push_str("</tbody></table></div>");
    if attempts.len() > TABLE_ROWS {
        let _ = write!(
            out,
            "<p class=\"muted\">Showing the newest {TABLE_ROWS}; the strip above covers all.</p>"
        );
    }
    out
}

fn state_outcome(outcome: &str) -> String {
    let class = match outcome {
        "fail" | "error" => "broken",
        "pass" => "healthy",
        _ => "",
    };
    format!("<span class=\"tag {class}\">{}</span>", escape(outcome))
}

pub fn clusters(repo: &str, clusters: &[Cluster]) -> String {
    let mut out = format!(
        "<p class=\"muted\">Failures from the last {CLUSTER_DAYS} days that share a failure hash.</p>"
    );
    if clusters.is_empty() {
        out.push_str("<p class=\"empty\">No failures in the last few days.</p>");
        return out;
    }
    out.push_str(
        "<div class=\"scroll\"><table><thead><tr><th>Failures</th><th>Tests</th><th>Hash</th>\
         <th>First seen</th><th>Last seen</th><th>Example</th></tr></thead><tbody>",
    );
    for c in clusters {
        let _ = write!(
            out,
            "<tr><td class=\"num\">{}</td><td class=\"num\">{}</td><td><code>{}</code></td>\
             <td>{}</td><td>{}</td><td class=\"id\"><a href=\"{}\">{}</a><br>{}</td></tr>",
            c.failures,
            c.tests,
            escape(short_sha(&c.failure_hash)),
            c.first_seen,
            c.last_seen,
            escape(&test_link(&c.example_test_id, repo)),
            escape(&c.example_test_id),
            message(c.example_message.as_deref()),
        );
    }
    out.push_str("</tbody></table></div>");
    out.push_str(&capped(clusters.len(), CLUSTER_LIMIT));
    out
}

/// How tests enter and leave quarantine, in one paragraph.
fn rules_note(rules: QuarantineRules) -> String {
    let entry = if rules.runs == 0 {
        "Automatic quarantine is off.".to_string()
    } else {
        format!(
            "A test enters after definite flakes in {} different run{} within 7 days, unless it \
             is broken.",
            rules.runs,
            if rules.runs == 1 { "" } else { "s" }
        )
    };
    format!(
        "<p class=\"muted\">Quarantined tests still run and upload, but their failures do not \
         fail the build. {entry} It leaves after {} consecutive passes, when it breaks, or after \
         {} days. Entries in a repository's <code>aster-quarantine.toml</code> also apply and are \
         not listed here.</p>",
        rules.release_passes, rules.days
    )
}

/// The quarantine list (`#quarantine`) and its recent changes.
pub fn quarantine(
    repo: Option<&str>,
    rules: QuarantineRules,
    entries: &[Quarantined],
    changes: &[QuarantineChange],
) -> String {
    let mut out = String::from("<section id=\"quarantine\">");
    out.push_str(&rules_note(rules));
    let name_cell = |test_id: &str, name: Option<&str>| match repo {
        Some(repo) => format!(
            "<td class=\"id\"><a href=\"{}\">{}</a><br><span class=\"muted\">{}</span></td>",
            escape(&test_link(test_id, repo)),
            escape(name.unwrap_or(test_id)),
            escape(test_id),
        ),
        None => format!("<td class=\"id\">{}</td>", escape(test_id)),
    };
    if entries.is_empty() {
        out.push_str("<p class=\"empty\">No tests are quarantined.</p>");
    } else {
        out.push_str(
            "<div class=\"scroll\"><table><thead><tr><th>Test</th><th>Since</th><th>Expires</th>\
             <th>Runs</th><th>Target / file</th></tr></thead><tbody>",
        );
        for entry in entries {
            let file = entry
                .file
                .as_deref()
                .map(|f| format!("<br>{}", escape(f)))
                .unwrap_or_default();
            let _ = write!(
                out,
                "<tr>{}<td>{}</td><td>{}</td><td class=\"num\">{}</td>\
                 <td class=\"id\"><code>{}</code>{}</td></tr>",
                name_cell(&entry.test_id, entry.name.as_deref()),
                escape(&when(entry.since)),
                escape(&when(entry.expires_at)),
                entry.runs,
                escape(entry.target.as_deref().unwrap_or("-")),
                file,
            );
        }
        out.push_str("</tbody></table></div>");
        out.push_str(&capped(entries.len(), QUARANTINE_LIMIT));
    }
    if !changes.is_empty() {
        out.push_str(
            "<h2>Recent changes</h2><div class=\"scroll\"><table><thead><tr><th>When</th>\
             <th>Change</th><th>Test</th></tr></thead><tbody>",
        );
        for change in changes {
            let class = match change.action.as_str() {
                "quarantined" => "flaky",
                "broken" => "broken",
                _ => "healthy",
            };
            let _ = write!(
                out,
                "<tr><td>{}</td><td><span class=\"tag {class}\">{}</span></td>{}</tr>",
                escape(&when(change.at)),
                escape(&change.action),
                name_cell(&change.test_id, change.name.as_deref()),
            );
        }
        out.push_str("</tbody></table></div>");
    }
    out.push_str("</section>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn test_row() -> TestRow {
        let at = Utc.with_ymd_and_hms(2026, 10, 13, 14, 3, 0).unwrap();
        TestRow {
            test_id: "<id>".into(),
            target: "//a:t".into(),
            file: Some("f\"x".into()),
            name: "n<b>".into(),
            state: "flaky".into(),
            flake_score: 7,
            last_flake_at: Some(at),
            broken_since_sha: None,
            first_seen: at,
            last_seen: at,
        }
    }

    #[test]
    fn leaderboard_escapes_test_fields() {
        let html = leaderboard(
            "o/<r>",
            &Leaderboard {
                flaky: vec![test_row()],
                scored: vec![],
                broken: vec![],
            },
        );
        assert!(!html.contains("<b>") && !html.contains("<id>"));
        assert!(html.contains("n&lt;b&gt;"));
        assert!(html.contains("/ui/tests/%3Cid%3E?repo=o%2F%3Cr%3E"));
        assert!(html.contains("2026-10-13 14:03 UTC"));
    }

    #[test]
    fn long_or_multiline_messages_collapse() {
        assert!(message(Some("short")).starts_with("<code>"));
        let long = "x".repeat(300);
        assert!(message(Some(&long)).starts_with("<details>"));
        assert!(message(Some("a\nb")).starts_with("<details>"));
        assert!(message(Some("<script>\nx")).contains("&lt;script&gt;"));
        assert!(message(None).contains("muted"));
    }

    const RULES: QuarantineRules = QuarantineRules {
        runs: 2,
        release_passes: 20,
        days: 30,
    };

    #[test]
    fn quarantine_page_has_an_empty_state_and_states_the_rules() {
        let html = quarantine(Some("o/r"), RULES, &[], &[]);
        assert!(html.contains("No tests are quarantined") && html.contains("id=\"quarantine\""));
        assert!(html.contains("2 different runs") && html.contains("20 consecutive passes"));
        let off = quarantine(None, QuarantineRules { runs: 0, ..RULES }, &[], &[]);
        assert!(off.contains("Automatic quarantine is off"));
    }

    #[test]
    fn quarantine_page_escapes_and_links_tests() {
        let at = Utc.with_ymd_and_hms(2026, 10, 13, 14, 3, 0).unwrap();
        let html = quarantine(
            Some("o/r"),
            RULES,
            &[Quarantined {
                test_id: "<id>".into(),
                name: Some("n<b>".into()),
                target: Some("//a:t".into()),
                file: None,
                since: at,
                expires_at: at,
                runs: 3,
            }],
            &[QuarantineChange {
                test_id: "gone".into(),
                name: None,
                action: "released".into(),
                at,
            }],
        );
        assert!(!html.contains("<b>") && !html.contains("<id>"));
        assert!(html.contains("/ui/tests/%3Cid%3E?repo=o%2Fr"));
        assert!(html.contains("Recent changes") && html.contains("released"));
    }
}
