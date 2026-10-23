//! The route table: every path the server answers, who may call it, and
//! which handler runs. It is the single source of truth. The router in
//! the HTTP router dispatches on [`Route::handler`], and [`gate`] decides
//! from [`Route::auth`] before any handler runs or any body is read, so a
//! route cannot exist without an auth class, and a `/v1/` path that is not
//! in the table gets no answer without a valid token.
//!
//! | Class | Who |
//! |---|---|
//! | `Public` | anyone: health checks and the sign-in code exchange |
//! | `Iap` | a person behind Google IAP (the dashboard and `/cli-auth`); the handler verifies the IAP assertion |
//! | `SignedUrl` | the test-only stand-in for a signed blob URL; only the in-memory store answers it |
//! | `Person` | a signed-in person's token only (sign out); a CI token is refused. With person sign-in switched off (`ASTER_SRV_PERSON_TOKENS=off`) the route answers 404 before any token is read |
//! | `Ci` | a CI token (GitHub OIDC or a development static token); a person's token is refused |
//! | `CiOrPerson` | a CI token for its own repository, or a signed-in person for any served repository: the read-only routes |
//! | `CiWriter` | a CI token only, held at the gate to the route's writer rule before any body is read: the cache's (ref, workflow, push event) for cache writes, the results rule for test-results upload |

use crate::audit;
use crate::auth::{AuthError, Identity, Principal};
use crate::App;
use crate::Reply;

/// Who may call a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    Public,
    Iap,
    SignedUrl,
    Person,
    Ci,
    CiOrPerson,
    CiWriter,
}

/// What answers a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handler {
    Healthz,
    Readyz,
    CliAuthConfirm,
    CliAuthAuthorize,
    TokenExchange,
    TokenRevoke,
    ResultsUpload,
    Durations,
    ResultsRead,
    CacheRead,
    CacheLookup,
    CacheWrite,
    SignedBlob,
    Ui,
}

/// One entry of the table. In `pattern`, `:name` matches one non-empty
/// segment and a trailing `*` matches the rest of the path.
#[derive(Debug, Clone, Copy)]
pub struct Route {
    pub method: &'static str,
    pub pattern: &'static str,
    pub auth: Auth,
    pub handler: Handler,
}

const fn route(method: &'static str, pattern: &'static str, auth: Auth, handler: Handler) -> Route {
    Route {
        method,
        pattern,
        auth,
        handler,
    }
}

/// Every route. Add a new route here and nowhere else.
pub const ROUTES: &[Route] = &[
    route("GET", "/healthz", Auth::Public, Handler::Healthz),
    route("GET", "/readyz", Auth::Public, Handler::Readyz),
    route("GET", "/cli-auth", Auth::Iap, Handler::CliAuthConfirm),
    route("POST", "/cli-auth", Auth::Iap, Handler::CliAuthAuthorize),
    route(
        "POST",
        "/v1/auth/token",
        Auth::Public,
        Handler::TokenExchange,
    ),
    // Sign out everywhere: the bearer is the credential, there is no IAP
    // session on this route.
    route(
        "POST",
        "/v1/auth/revoke",
        Auth::Person,
        Handler::TokenRevoke,
    ),
    route(
        "POST",
        "/v1/test-results/runs",
        Auth::CiWriter,
        Handler::ResultsUpload,
    ),
    route(
        "GET",
        "/v1/test-results/durations",
        Auth::CiOrPerson,
        Handler::Durations,
    ),
    route(
        "GET",
        "/v1/test-results/tests",
        Auth::CiOrPerson,
        Handler::ResultsRead,
    ),
    route(
        "GET",
        "/v1/test-results/tests/:test_id",
        Auth::CiOrPerson,
        Handler::ResultsRead,
    ),
    route(
        "GET",
        "/v1/test-results/failures",
        Auth::CiOrPerson,
        Handler::ResultsRead,
    ),
    route(
        "GET",
        "/v1/test-results/quarantine",
        Auth::CiOrPerson,
        Handler::ResultsRead,
    ),
    route(
        "GET",
        "/v1/test-results/clusters",
        Auth::CiOrPerson,
        Handler::ResultsRead,
    ),
    route(
        "GET",
        "/v1/test-results/runs/:run_id/failures",
        Auth::CiOrPerson,
        Handler::ResultsRead,
    ),
    route(
        "GET",
        "/v1/cache/summary",
        Auth::CiOrPerson,
        Handler::CacheRead,
    ),
    route(
        "GET",
        "/v1/cache/entries",
        Auth::CiOrPerson,
        Handler::CacheRead,
    ),
    // A lookup is a read made with POST so it can carry many keys.
    route(
        "POST",
        "/v1/cache/lookup",
        Auth::CiOrPerson,
        Handler::CacheLookup,
    ),
    route(
        "POST",
        "/v1/cache/upload",
        Auth::CiWriter,
        Handler::CacheWrite,
    ),
    route(
        "POST",
        "/v1/cache/commit",
        Auth::CiWriter,
        Handler::CacheWrite,
    ),
    // A rewritten signed blob URL: `PUT|GET /<bucket>/cas/<sha256>`.
    route(
        "PUT",
        "/:bucket/cas/:sha",
        Auth::SignedUrl,
        Handler::SignedBlob,
    ),
    route(
        "GET",
        "/:bucket/cas/:sha",
        Auth::SignedUrl,
        Handler::SignedBlob,
    ),
    route("*", "/ui", Auth::Iap, Handler::Ui),
    route("*", "/ui/*", Auth::Iap, Handler::Ui),
];

fn matches(pattern: &str, path: &str) -> bool {
    let mut want = pattern.split('/');
    let mut got = path.split('/');
    loop {
        match (want.next(), got.next()) {
            (Some("*"), Some(_)) => return true,
            (Some(w), Some(g)) if w.starts_with(':') => {
                if g.is_empty() {
                    return false;
                }
            }
            (Some(w), Some(g)) if w == g => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

/// The route for a request, if the table has one.
pub fn lookup(method: &str, path: &str) -> Option<&'static Route> {
    ROUTES.iter().find(|route| {
        (route.method == "*" || route.method == method)
            && matches(route.pattern, path)
            // `/v1/` is the API; a signed-URL stand-in never lives there.
            && !(route.auth == Auth::SignedUrl && path.starts_with("/v1/"))
    })
}

/// Whether some route has this path under another method.
pub fn path_known(path: &str) -> bool {
    ROUTES.iter().any(|route| {
        matches(route.pattern, path) && !(route.auth == Auth::SignedUrl && path.starts_with("/v1/"))
    })
}

/// The templated route of a request, a bounded set safe as a metric and audit
/// label. It comes from the table, so a new route is labelled where it is
/// declared; the few paths answered before the table is consulted have fixed
/// labels, and anything else is `/v1/other` or `other`.
pub fn label(method: &str, path: &str) -> &'static str {
    match lookup(method, path).map(|route| route.handler) {
        Some(Handler::Healthz) => "/healthz",
        Some(Handler::Readyz) => "/readyz",
        Some(Handler::CliAuthConfirm | Handler::CliAuthAuthorize) => "/cli-auth",
        Some(Handler::TokenExchange) => "/v1/auth/token",
        Some(Handler::TokenRevoke) => "/v1/auth/revoke",
        Some(Handler::ResultsUpload) => "/v1/test-results/runs",
        Some(Handler::Durations) => "/v1/test-results/durations",
        Some(Handler::ResultsRead) => "/v1/test-results/query",
        Some(Handler::CacheRead | Handler::CacheLookup | Handler::CacheWrite) => "/v1/cache",
        Some(Handler::Ui) => "/ui",
        Some(Handler::SignedBlob) => "other",
        None => match path {
            "/metrics" => "/metrics",
            "/v1/healthz" => "/v1/healthz",
            p if p.starts_with("/v1/") => "/v1/other",
            _ => "other",
        },
    }
}

/// The auth gate, run before every handler of a route that needs a token.
///
/// Deny by default: a missing, malformed, expired, mis-signed or
/// wrong-audience token is `401` whatever the route, and a `/v1/` path the
/// table does not list is `401` without a valid token and only then
/// `404`. A valid token is then held to the route's class: a person's
/// token on `Ci` and `CiWriter` routes is `403`. Handlers do their own
/// repository and writer checks on top; this is the floor.
///
/// Every refusal is audited and counted here, whichever handler the request
/// was for, and a valid token is charged to its subject's rate limit
/// (`429`). `authorization` is the `Authorization` header value. It is never
/// put in an error.
pub async fn gate(
    app: &App,
    route: Option<&Route>,
    authorization: Option<&str>,
) -> Result<Option<Principal>, Reply> {
    let class = route.map_or(Auth::CiOrPerson, |route| route.auth);
    match class {
        Auth::Public | Auth::Iap | Auth::SignedUrl => Ok(None),
        Auth::Person | Auth::Ci | Auth::CiWriter | Auth::CiOrPerson => {
            // The kill switch: a server with person sign-in off has no
            // sign-out to offer, whoever asks.
            if class == Auth::Person && app.person_tokens.is_none() {
                return Err(Reply::error(404, "not found"));
            }
            let principal = match app.principal(authorization).await {
                Ok(principal) => principal,
                Err(error) => {
                    let subject = match &error {
                        AuthError::Forbidden(repository) => Some(repository.as_str()),
                        _ => None,
                    };
                    audit::auth_failure(subject, error.reason_code(), &error.to_string());
                    return Err(error.into_reply());
                }
            };
            let subject = match &principal {
                Principal::Ci(identity) => format!("ci:{}", identity.repository),
                Principal::Person(person) => format!("person:{}", person.subject),
            };
            app.limits.api_subject.check(&subject).map_err(|seconds| {
                audit::rate_limited("subject", Some(&subject));
                Reply::too_many(seconds)
            })?;
            if matches!(class, Auth::Ci | Auth::CiWriter)
                && matches!(principal, Principal::Person(_))
            {
                audit::auth_failure(
                    Some(&person_subject(&principal)),
                    "person_token_not_allowed",
                    "a person's token is read-only",
                );
                return Err(Reply::error(403, "a person's token is read-only"));
            }
            // A CI token that is not allowed to write is refused here, before
            // any body is read: a pull-request job's token must not make the
            // server read and parse up to 64 MiB only to refuse it.
            if class == Auth::CiWriter {
                if let (Some(route), Principal::Ci(identity)) = (route, &principal) {
                    if let Some(message) = writer_refusal(app, route.handler, identity) {
                        audit::auth_failure(
                            Some(&identity.repository),
                            "writer_not_allowed",
                            message,
                        );
                        return Err(Reply::error(403, message));
                    }
                }
            }
            if class == Auth::Person && matches!(principal, Principal::Ci(_)) {
                audit::auth_failure(
                    Some(&person_subject(&principal)),
                    "ci_token_not_allowed",
                    "only a signed-in person can sign out",
                );
                return Err(Reply::error(403, "only a signed-in person can sign out"));
            }
            Ok(Some(principal))
        }
    }
}

/// Why a CI token may not call a `CiWriter` route, or `None` when it may.
/// The cache writes use the cache's own writer rule (ref, event, workflow).
/// Test-results upload deliberately accepts pull-request runs (a retry that
/// passes there is the flake signal), so it applies only the results rule.
/// The repository allowlist and the owner-id pin were applied when the token
/// was verified, before this runs.
fn writer_refusal(app: &App, handler: Handler, identity: &Identity) -> Option<&'static str> {
    match handler {
        Handler::CacheWrite if !crate::cache::cache_writer_allowed(&app.config, identity) => {
            Some("this caller may not write the cache")
        }
        Handler::ResultsUpload if !crate::cache::results_writer_allowed(&app.config, identity) => {
            Some("this workflow may not upload test results")
        }
        _ => None,
    }
}

fn person_subject(principal: &Principal) -> String {
    match principal {
        Principal::Person(person) => person.subject.clone(),
        Principal::Ci(identity) => identity.repository.clone(),
    }
}

/// Whether the router must run [`gate`] for this path: every `/v1/` path
/// except the ones the table marks as not needing a token.
pub fn needs_gate(route: Option<&Route>, path: &str) -> bool {
    match route {
        Some(route) => !matches!(route.auth, Auth::Public | Auth::Iap | Auth::SignedUrl),
        None => path.starts_with("/v1/"),
    }
}

/// The identity a CI principal carries, or the person's, for the repository
/// they named.
pub fn identity_for(principal: Principal, repo: Option<&str>) -> Result<Identity, Reply> {
    match principal {
        Principal::Ci(identity) => Ok(identity),
        Principal::Person(_) => {
            let repo =
                repo.ok_or_else(|| Reply::error(400, "name the repository with ?repo=owner/name"))?;
            Ok(Identity {
                repository: repo.to_string(),
                git_ref: None,
                run_id: None,
                sha: None,
                workflow: None,
                job_workflow: None,
                event_name: None,
                static_token: false,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_match_whole_segments() {
        assert!(matches(
            "/v1/test-results/tests/:id",
            "/v1/test-results/tests/a"
        ));
        assert!(!matches(
            "/v1/test-results/tests/:id",
            "/v1/test-results/tests/"
        ));
        assert!(!matches(
            "/v1/test-results/tests/:id",
            "/v1/test-results/tests/a/b"
        ));
        assert!(matches("/ui/*", "/ui/tests/a%2Fb"));
        assert!(!matches("/ui/*", "/uix"));
        assert!(matches("/:bucket/cas/:sha", "/b/cas/abc"));
        assert_eq!(lookup("GET", "/ui").unwrap().handler, Handler::Ui);
        assert!(lookup("DELETE", "/v1/cache/lookup").is_none());
        assert!(path_known("/v1/cache/lookup"));
    }

    #[test]
    fn every_v1_route_but_the_code_exchange_needs_a_token() {
        for route in ROUTES.iter().filter(|r| r.pattern.starts_with("/v1/")) {
            let public = route.auth == Auth::Public;
            assert_eq!(
                public,
                route.pattern == "/v1/auth/token",
                "{}",
                route.pattern
            );
        }
    }
}
