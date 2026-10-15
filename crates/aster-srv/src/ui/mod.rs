//! The dashboard: server-rendered HTML under `/ui`, for people behind
//! Google IAP.
//!
//! - `/ui`: the flaky leaderboard and broken tests
//! - `/ui/tests/:test_id`: one test's state and history
//! - `/ui/clusters`: failures grouped by failure hash
//! - `/ui/quarantine`: the quarantine list and its recent changes
//!
//! `?repo=owner/name` picks the repository; without it the one with the
//! newest run is shown. Any repository on the allowlist can be viewed. The
//! pages only read; there is no state-changing route.

mod data;
mod html;
mod pages;

use chrono::Utc;
use hyper::Method;
use serde_json::json;

use crate::iap::ViewerError;
use crate::test_results::query::{parse_query, percent_decode};
use crate::{App, Reply};

use html::{Frame, Section};

/// Sent with every page. Nothing runs script or loads anything but inline
/// styles, and the pages may not be framed.
pub const SECURITY_HEADERS: [(&str, &str); 4] = [
    (
        "content-security-policy",
        "default-src 'none'; style-src 'unsafe-inline'; img-src data:",
    ),
    ("x-content-type-options", "nosniff"),
    ("referrer-policy", "same-origin"),
    ("x-frame-options", "DENY"),
];

/// A dashboard page.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    Flaky,
    Test(String),
    Clusters,
    Quarantine,
}

/// Whether `path` belongs to the dashboard.
pub fn is_ui_path(path: &str) -> bool {
    path == "/ui" || path.starts_with("/ui/")
}

/// The page for a request path, with the test id percent-decoded (it may
/// hold `/`, so it arrives as `%2F`).
pub fn parse_route(path: &str) -> Option<Route> {
    let rest = path.strip_prefix("/ui")?;
    match rest {
        "" | "/" => return Some(Route::Flaky),
        "/clusters" | "/clusters/" => return Some(Route::Clusters),
        "/quarantine" | "/quarantine/" => return Some(Route::Quarantine),
        _ => {}
    }
    match rest.strip_prefix("/tests/")?.trim_end_matches('/') {
        id if !id.is_empty() && !id.contains('/') => Some(Route::Test(percent_decode(id)?)),
        _ => None,
    }
}

pub async fn handle(
    app: &App,
    method: &Method,
    path: &str,
    query: Option<&str>,
    assertion: Option<String>,
) -> Reply {
    if !app.iap.enabled() {
        return Reply::error(404, "not found");
    }
    if method != Method::GET {
        return failure(405, "Method not allowed", "The dashboard only reads.");
    }
    let Some(route) = parse_route(path) else {
        return failure(404, "Not found", "There is no such page.");
    };
    let viewer = match app.iap.verify(assertion.as_deref()).await {
        Ok(viewer) => viewer,
        Err(error @ ViewerError::Forbidden(_)) => {
            return failure(403, "Not allowed", &error.to_string())
        }
        Err(error) => return failure(401, "Sign in", &error.to_string()),
    };
    let Some(pairs) = parse_query(query) else {
        return failure(400, "Bad request", "The query is not valid.");
    };
    let requested = pairs
        .into_iter()
        .find(|(key, _)| key == "repo")
        .map(|(_, value)| value);
    if let Some(repo) = &requested {
        if !app.config.repositories.allows(repo) {
            return failure(403, "Not allowed", "That repository is not available here.");
        }
    }

    match render(app, &viewer.email, route, requested).await {
        Ok(reply) => reply,
        Err(error) => {
            crate::log::error(
                "cannot render the dashboard",
                json!({"error": format!("{error:#}")}),
            );
            failure(
                503,
                "Unavailable",
                "The results could not be read; retry later.",
            )
        }
    }
}

fn failure(status: u16, title: &str, message: &str) -> Reply {
    Reply::html(status, html::message_page(title, message))
}

async fn render(
    app: &App,
    viewer: &str,
    route: Route,
    requested: Option<String>,
) -> anyhow::Result<Reply> {
    let client = app.pool.get().await?;
    let repos: Vec<String> = data::repos(&client)
        .await?
        .into_iter()
        .filter(|repo| app.config.repositories.allows(repo))
        .collect();
    // Use the spelling the data is stored under; a repository with no runs
    // yet keeps the spelling it was asked for.
    let repo: Option<String> = match requested {
        Some(requested) => Some(
            repos
                .iter()
                .find(|repo| repo.eq_ignore_ascii_case(&requested))
                .cloned()
                .unwrap_or(requested),
        ),
        None => repos.first().cloned(),
    };
    let make = |title: &str, section: Section, body: &str| -> Reply {
        Reply::html(
            200,
            html::page(
                &Frame {
                    title,
                    viewer,
                    section,
                    repo: repo.as_deref(),
                    repos: &repos,
                },
                body,
            ),
        )
    };

    let Some(repo_name) = repo.as_deref() else {
        let (title, section) = match route {
            Route::Flaky => ("Flaky tests", Section::Flaky),
            Route::Clusters => ("Failure clusters", Section::Clusters),
            Route::Quarantine => ("Quarantine", Section::Quarantine),
            Route::Test(_) => ("Test", Section::Other),
        };
        let body = if route == Route::Quarantine {
            pages::quarantine(None, app.config.quarantine, &[], &[])
        } else {
            pages::no_runs()
        };
        return Ok(make(title, section, &body));
    };

    Ok(match route {
        Route::Flaky => {
            let board = data::leaderboard(&client, repo_name).await?;
            make(
                "Flaky tests",
                Section::Flaky,
                &pages::leaderboard(repo_name, &board),
            )
        }
        Route::Clusters => {
            let since = Utc::now().date_naive() - chrono::Days::new(data::CLUSTER_DAYS);
            let clusters = data::clusters(&client, repo_name, since).await?;
            make(
                "Failure clusters",
                Section::Clusters,
                &pages::clusters(repo_name, &clusters),
            )
        }
        Route::Quarantine => {
            let (entries, changes) = data::quarantine(&client, repo_name).await?;
            make(
                "Quarantine",
                Section::Quarantine,
                &pages::quarantine(Some(repo_name), app.config.quarantine, &entries, &changes),
            )
        }
        Route::Test(test_id) => {
            let Some(test) = data::test(&client, repo_name, &test_id).await? else {
                let body = "<p class=\"empty\">No such test in this repository.</p>";
                let mut reply = make("Test not found", Section::Other, body);
                reply.status = 404;
                return Ok(reply);
            };
            let retention = app.config.retention_days;
            let since = Utc::now().date_naive() - chrono::Days::new(u64::from(retention));
            let attempts = data::history(&client, repo_name, &test_id, since).await?;
            make(
                &test.name,
                Section::Other,
                &pages::test_detail(&test, &attempts, retention),
            )
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pages() {
        assert_eq!(parse_route("/ui"), Some(Route::Flaky));
        assert_eq!(parse_route("/ui/"), Some(Route::Flaky));
        assert_eq!(parse_route("/ui/clusters"), Some(Route::Clusters));
        assert_eq!(parse_route("/ui/quarantine/"), Some(Route::Quarantine));
        assert_eq!(
            parse_route("/ui/tests/a%2Fb%20c"),
            Some(Route::Test("a/b c".into()))
        );
        assert_eq!(parse_route("/ui/tests/x/"), Some(Route::Test("x".into())));
        for bad in [
            "/ui/tests",
            "/ui/tests/",
            "/ui/tests/a/b",
            "/ui/tests/%zz",
            "/ui/other",
            "/uix",
            "/",
        ] {
            assert_eq!(parse_route(bad), None, "{bad}");
        }
    }

    #[test]
    fn recognizes_dashboard_paths() {
        assert!(is_ui_path("/ui") && is_ui_path("/ui/") && is_ui_path("/ui/x"));
        assert!(!is_ui_path("/uix") && !is_ui_path("/v1/ui"));
    }
}
