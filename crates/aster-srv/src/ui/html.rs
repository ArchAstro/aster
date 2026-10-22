//! The dashboard's page frame and the helpers every page uses. Every value
//! that reaches a page goes through [`escape`].

use std::fmt::Write;

use chrono::{DateTime, Utc};

/// Escape text for HTML element content and quoted attribute values.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            // A NUL is not valid HTML; other controls are only noise.
            '\0' => out.push('\u{fffd}'),
            c => out.push(c),
        }
    }
    out
}

/// Percent-encode a path segment or query value.
pub fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            byte => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// `2026-10-13 14:03 UTC`.
pub fn when(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%d %H:%M UTC").to_string()
}

/// [`when`], or a dash.
pub fn when_opt(at: Option<DateTime<Utc>>) -> String {
    at.map_or_else(|| "-".to_string(), when)
}

/// A byte count with a binary unit: `0 B`, `1.5 KiB`, `12.0 MiB`.
pub fn bytes(count: i64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if count < 1024 {
        return format!("{count} B");
    }
    let mut value = count as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// The first `len` characters of a commit sha.
pub fn short_sha(sha: &str) -> &str {
    let end = sha
        .char_indices()
        .nth(10)
        .map_or(sha.len(), |(index, _)| index);
    &sha[..end]
}

/// A path on the dashboard for `repo`: `/ui/clusters?repo=...`.
pub fn link(path: &str, repo: Option<&str>) -> String {
    match repo {
        Some(repo) => format!("{path}?repo={}", encode(repo)),
        None => path.to_string(),
    }
}

/// The link to a test's page.
pub fn test_link(test_id: &str, repo: &str) -> String {
    link(&format!("/ui/tests/{}", encode(test_id)), Some(repo))
}

/// Where a page sits in the nav.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Flaky,
    Clusters,
    Quarantine,
    Cache,
    /// A test's page: no nav entry is current.
    Other,
}

/// What the frame shows around a page.
pub struct Frame<'a> {
    pub title: &'a str,
    pub viewer: &'a str,
    pub section: Section,
    /// The repository being viewed, if one is chosen.
    pub repo: Option<&'a str>,
    /// Every repository that has runs, for the switcher.
    pub repos: &'a [String],
}

/// A whole page around `body` (which the caller has already escaped).
pub fn page(frame: &Frame<'_>, body: &str) -> String {
    let mut nav = String::new();
    for (section, path, label) in [
        (Section::Flaky, "/ui", "Flaky tests"),
        (Section::Clusters, "/ui/clusters", "Failure clusters"),
        (Section::Quarantine, "/ui/quarantine", "Quarantine"),
        (Section::Cache, "/ui/cache", "Cache"),
    ] {
        let current = if frame.section == section {
            " aria-current=\"page\""
        } else {
            ""
        };
        let _ = write!(
            nav,
            "<a href=\"{}\"{current}>{label}</a>",
            escape(&link(path, frame.repo))
        );
    }

    let mut switcher = String::new();
    if frame.repos.len() > 1 {
        switcher.push_str("<nav class=\"repos\" aria-label=\"Repository\">");
        for repo in frame.repos {
            let current = if frame.repo.is_some_and(|r| r.eq_ignore_ascii_case(repo)) {
                " aria-current=\"true\""
            } else {
                ""
            };
            let _ = write!(
                switcher,
                "<a href=\"{}\"{current}>{}</a>",
                escape(&link(section_path(frame.section), Some(repo))),
                escape(repo)
            );
        }
        switcher.push_str("</nav>");
    } else if let Some(repo) = frame.repo {
        let _ = write!(switcher, "<p class=\"repo\">{}</p>", escape(repo));
    }

    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta name=\"color-scheme\" content=\"light dark\">\
         <title>{title} - Aster</title><style>{CSS}</style></head><body>\
         <header><strong>Aster</strong><nav>{nav}</nav>\
         <span class=\"viewer\">{viewer}</span></header>\
         <main>{switcher}<h1>{title}</h1>{body}</main></body></html>\n",
        title = escape(frame.title),
        viewer = escape(frame.viewer),
    )
}

/// Where the repo switcher leads on each page (a test's page leads home).
fn section_path(section: Section) -> &'static str {
    match section {
        Section::Flaky | Section::Other => "/ui",
        Section::Clusters => "/ui/clusters",
        Section::Quarantine => "/ui/quarantine",
        Section::Cache => "/ui/cache",
    }
}

/// A bare page for errors raised before there is a viewer or a repository.
pub fn message_page(title: &str, message: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta name=\"color-scheme\" content=\"light dark\">\
         <title>{title} - Aster</title><style>{CSS}</style></head><body>\
         <main><h1>{title}</h1><p>{message}</p></main></body></html>\n",
        title = escape(title),
        message = escape(message),
    )
}

const CSS: &str = "\
:root{--bg:#fff;--fg:#1b1f24;--muted:#5b6672;--line:#d8dee4;--panel:#f6f8fa;--link:#0b5cad;\
--pass:#2da44e;--fail:#cf222e;--error:#bf8700;--skip:#a0a8b1;--flaky:#bf8700;--broken:#cf222e}\
@media (prefers-color-scheme:dark){:root{--bg:#0e1116;--fg:#e6eaee;--muted:#9aa5b1;\
--line:#2b3138;--panel:#161b22;--link:#6cb6ff;--pass:#3fb950;--fail:#f85149;--error:#d29922;\
--skip:#6e7781;--flaky:#d29922;--broken:#f85149}}\
*{box-sizing:border-box}\
body{margin:0;background:var(--bg);color:var(--fg);\
font:15px/1.45 system-ui,-apple-system,Segoe UI,Roboto,sans-serif}\
a{color:var(--link)}\
header{display:flex;flex-wrap:wrap;gap:8px 20px;align-items:center;padding:10px 16px;\
border-bottom:1px solid var(--line);background:var(--panel)}\
header nav{display:flex;flex-wrap:wrap;gap:4px 16px}\
header nav a{color:var(--fg);text-decoration:none;padding:2px 0;border-bottom:2px solid transparent}\
header nav a[aria-current]{border-bottom-color:var(--link);font-weight:600}\
.viewer{margin-left:auto;color:var(--muted);font-size:13px;overflow-wrap:anywhere}\
main{padding:16px;max-width:1200px;margin:0 auto}\
h1{font-size:22px;margin:8px 0 16px}h2{font-size:17px;margin:28px 0 8px}\
.repos{display:flex;flex-wrap:wrap;gap:6px;margin:0 0 4px}\
.repos a{border:1px solid var(--line);border-radius:6px;padding:2px 10px;text-decoration:none;\
color:var(--fg);font-size:13px}\
.repos a[aria-current]{background:var(--link);border-color:var(--link);color:var(--bg)}\
.repo{margin:0;color:var(--muted);font-size:13px}\
.scroll{overflow-x:auto}\
table{border-collapse:collapse;width:100%}\
th,td{text-align:left;padding:6px 10px;border-bottom:1px solid var(--line);vertical-align:top}\
th{font-size:12px;text-transform:uppercase;letter-spacing:.04em;color:var(--muted);white-space:nowrap}\
td.num{text-align:right;font-variant-numeric:tabular-nums}\
code,pre{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:13px}\
pre{background:var(--panel);border:1px solid var(--line);border-radius:6px;padding:8px;\
overflow-x:auto;white-space:pre-wrap;overflow-wrap:anywhere;margin:6px 0 0}\
td.id{overflow-wrap:anywhere;min-width:12em}\
.muted{color:var(--muted)}\
.empty{border:1px dashed var(--line);border-radius:8px;padding:16px;color:var(--muted)}\
.tag{display:inline-block;border-radius:10px;padding:0 8px;font-size:12px;font-weight:600;\
border:1px solid var(--line)}\
.tag.flaky{color:var(--flaky);border-color:var(--flaky)}\
.tag.broken{color:var(--broken);border-color:var(--broken)}\
.tag.healthy{color:var(--pass);border-color:var(--pass)}\
dl{display:grid;grid-template-columns:max-content 1fr;gap:4px 16px;margin:0}\
dt{color:var(--muted)}dd{margin:0;overflow-wrap:anywhere}\
.strip{display:flex;flex-wrap:wrap;gap:2px;margin:8px 0}\
.cell{width:12px;height:22px;border-radius:2px;background:var(--skip)}\
.cell.pass{background:var(--pass)}.cell.fail{background:var(--fail)}\
.cell.error{background:var(--error)}\
form{margin:8px 0}input[type=search]{font:inherit;padding:2px 6px;border:1px solid var(--line);\
border-radius:6px;background:var(--bg);color:var(--fg)}\
button{font:inherit;padding:2px 10px}\
.legend{font-size:13px;color:var(--muted)}\
.legend i{display:inline-block;width:10px;height:10px;border-radius:2px;margin:0 4px 0 12px}\
.legend i:first-child{margin-left:0}\
.legend i.pass{background:var(--pass)}.legend i.fail{background:var(--fail)}\
.legend i.error{background:var(--error)}.legend i.skip{background:var(--skip)}\
@media (max-width:600px){main{padding:12px}th,td{padding:6px}dl{grid-template-columns:1fr}\
dt{margin-top:6px}}";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_markup_quotes_and_nuls() {
        assert_eq!(
            escape(r#"<script>alert("x" & 'y')</script>"#),
            "&lt;script&gt;alert(&quot;x&quot; &amp; &#39;y&#39;)&lt;/script&gt;"
        );
        assert_eq!(escape("a\0b"), "a\u{fffd}b");
        assert_eq!(escape("plain ünïcode"), "plain ünïcode");
        assert_eq!(escape(""), "");
    }

    #[test]
    fn encodes_ids_for_urls() {
        assert_eq!(encode("owner/name"), "owner%2Fname");
        assert_eq!(encode("a b&c=d"), "a%20b%26c%3Dd");
        assert_eq!(encode("é"), "%C3%A9");
        assert_eq!(encode("safe-_.~09az"), "safe-_.~09az");
        assert_eq!(
            test_link("a/b c", "ArchAstro/x"),
            "/ui/tests/a%2Fb%20c?repo=ArchAstro%2Fx"
        );
    }

    #[test]
    fn decoding_inverts_encoding() {
        for text in ["a/b?c=d&e", "ünï/ç ode", "100%", "+plus+"] {
            assert_eq!(
                crate::test_results::query::percent_decode(&encode(text)).as_deref(),
                Some(text)
            );
        }
    }

    #[test]
    fn formats_byte_counts() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1536), "1.5 KiB");
        assert_eq!(bytes(12 * 1024 * 1024), "12.0 MiB");
        assert_eq!(bytes(i64::MAX), "8192.0 PiB");
    }

    #[test]
    fn shortens_shas_on_character_boundaries() {
        assert_eq!(short_sha("0123456789abcdef"), "0123456789");
        assert_eq!(short_sha("abc"), "abc");
        assert_eq!(short_sha("ééééééééééééé"), "éééééééééé");
    }

    #[test]
    fn pages_escape_the_frame() {
        let repos = vec!["a/<b>".to_string(), "c/d".to_string()];
        let html = page(
            &Frame {
                title: "<T>",
                viewer: "x@y.z\"><i>",
                section: Section::Flaky,
                repo: Some("a/<b>"),
                repos: &repos,
            },
            "<p>body</p>",
        );
        assert!(!html.contains("<T>") && !html.contains("<i>") && !html.contains("<b>"));
        assert!(html.contains("&lt;T&gt;"));
        assert!(html.contains("<p>body</p>"));
        assert!(html.contains("href=\"/ui?repo=a%2F%3Cb%3E\""));
        assert!(html.contains("aria-current=\"true\""));
    }
}
