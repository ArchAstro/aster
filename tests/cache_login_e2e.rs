//! End-to-end laptop sign-in: real aster-srv, real Postgres, real `aster`
//! binary on both ends, a scripted browser in the middle.
//!
//! CI writes a build artifact with a GitHub OIDC token. A laptop with no
//! credentials builds locally until `aster cache login` signs it in: the
//! scripted browser plays a person behind IAP (a signed
//! `x-goog-iap-jwt-assertion`), confirms the sign-in, and follows the
//! server's redirect to the CLI's loopback listener. The laptop then
//! restores what CI wrote without running the command.
//!
//! Set `ASTER_SRV_TEST_DATABASE_URL` to a Postgres URL whose user may create
//! databases. Without it the test skips, the same way the aster-srv suite does.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use aster_srv::auth::Verifier;
use aster_srv::cache::{BlobStore, FixedSigner};
use aster_srv::iap::IapVerifier;
use aster_srv::raw_store::RawStore;
use aster_srv::{App, Config};
use jsonwebtoken::{encode, Algorithm, DecodingKey, EncodingKey, Header};
use serde_json::{json, Value};

const OIDC_PRIVATE: &[u8] = include_bytes!("fixtures/test-oidc-key.pem");
const OIDC_PUBLIC: &[u8] = include_bytes!("fixtures/test-oidc-key.pub.pem");
const REPO: &str = "acme/app";
const PERSON: &str = "ada@example.com";
const CI_WORKFLOW: &str = "acme/app/.github/workflows/ci.yml@refs/heads/main";
const IAP_AUDIENCE: &str = "/projects/123456/global/backendServices/4242";
const CLI_KEY: &str = "e2e1:Rm7vKq2LxT9bYc4nWz8PdJ3hSeA6UgF1oIiNtXuMvB0";

struct Running {
    url: String,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    app: Arc<App>,
    admin_url: String,
    database: String,
    /// The test's stand-in for Google's IAP signing key.
    iap_signing_key: Vec<u8>,
}

impl Running {
    async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.app.pool.close();
        let (admin, connection) = tokio_postgres::connect(&self.admin_url, tokio_postgres::NoTls)
            .await
            .expect("admin connection");
        tokio::spawn(connection);
        admin
            .batch_execute(&format!(
                "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                self.database
            ))
            .await
            .expect("drop test database");
    }

    /// What IAP would add to a request from `email`.
    fn iap_assertion(&self, email: &str) -> String {
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some("iap".into());
        encode(
            &header,
            &json!({
                "iss": "https://cloud.google.com/iap",
                "aud": IAP_AUDIENCE,
                "iat": chrono::Utc::now().timestamp(),
                "exp": chrono::Utc::now().timestamp() + 300,
                "email": email,
                "sub": "accounts.google.com:e2e-1001",
            }),
            &EncodingKey::from_ec_der(&self.iap_signing_key),
        )
        .expect("sign an IAP assertion")
    }
}

async fn start() -> Option<Running> {
    let Ok(admin_url) = std::env::var("ASTER_SRV_TEST_DATABASE_URL") else {
        eprintln!("skipping: ASTER_SRV_TEST_DATABASE_URL is not set");
        return None;
    };
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let database = format!(
        "aster_login_e2e_{}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_micros()
    );
    let (admin, connection) = tokio_postgres::connect(&admin_url, tokio_postgres::NoTls)
        .await
        .expect("connect to the test database");
    tokio::spawn(connection);
    admin
        .batch_execute(&format!("CREATE DATABASE {database}"))
        .await
        .expect("create test database");
    let (base, _) = admin_url.rsplit_once('/').expect("database url has a name");
    let database_url = format!("{base}/{database}?sslmode=disable");

    // The server names itself in the tokens it issues, so bind first.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}", listener.local_addr().unwrap());
    let config = Config::from_env(|name| match name {
        "ASTER_SRV_DATABASE_URL" => Some(database_url.clone()),
        "ASTER_SRV_REPOSITORIES" => Some("acme/*".into()),
        "ASTER_SRV_CACHE_BUCKET" => Some("aster-cache-e2e".into()),
        "ASTER_SRV_CACHE_SIGNER" => Some("cache-e2e@example.iam.gserviceaccount.com".into()),
        "ASTER_SRV_IAP_AUDIENCE" => Some(IAP_AUDIENCE.into()),
        "ASTER_SRV_DEV_BLOB_ROUTE" => Some("1".into()),
        "ASTER_SRV_UI_EMAIL_DOMAINS" => Some("example.com".into()),
        "ASTER_SRV_PUBLIC_URL" => Some(url.clone()),
        // The e2e server listens on loopback http.
        "ASTER_SRV_ALLOW_HTTP_PUBLIC_URL" => Some("1".into()),
        "ASTER_SRV_CLI_TOKEN_KEYS" => Some(CLI_KEY.into()),
        "ASTER_SRV_CACHE_WRITER_WORKFLOWS" => Some("acme/app/.github/workflows/ci.yml@".into()),
        _ => None,
    })
    .expect("test config");
    let verifier = Verifier::new(&config).with_key(
        "test",
        DecodingKey::from_rsa_pem(OIDC_PUBLIC).expect("OIDC public key"),
    );
    let iap_key =
        rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).expect("generate a P-256 key");
    let iap = IapVerifier::new(&config)
        .with_key("iap", DecodingKey::from_ec_der(iap_key.public_key_raw()));
    let signer = Arc::new(FixedSigner {
        email: "cache-e2e@example.iam.gserviceaccount.com".into(),
        signature: b"e2e-signature".to_vec(),
        signed: Default::default(),
    });
    let store = BlobStore::memory("aster-cache-e2e", signer);
    let app = App::start_with_cache(
        config,
        verifier,
        iap,
        Some(RawStore::Memory(Default::default())),
        Some(store),
    )
    .await
    .expect("aster-srv starts");
    let (shutdown, stopped) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(aster_srv::serve(Arc::clone(&app), listener, async {
        let _ = stopped.await;
    }));
    Some(Running {
        url,
        shutdown: Some(shutdown),
        app,
        admin_url,
        database,
        iap_signing_key: iap_key.serialize_der(),
    })
}

/// A stand-in for GitHub's OIDC token endpoint: any GET gets a token that
/// names `acme/app` on `main`, from the `ci.yml` workflow.
fn github_oidc_endpoint(stop: Arc<AtomicBool>) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("test".into());
    let jwt = encode(
        &header,
        &json!({
            "iss": "https://token.actions.githubusercontent.com",
            "aud": "aster-srv",
            "exp": chrono::Utc::now().timestamp() + 600,
            "repository": REPO,
            "ref": "refs/heads/main",
            "sha": "0123abc",
            "run_id": "7001",
            "event_name": "push",
            "workflow_ref": CI_WORKFLOW,
            "job_workflow_ref": CI_WORKFLOW,
        }),
        &EncodingKey::from_rsa_pem(OIDC_PRIVATE).expect("OIDC private key"),
    )
    .expect("sign an OIDC token");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the OIDC stand-in");
    let url = format!(
        "http://{}/token?api-version=2.0",
        listener.local_addr().unwrap()
    );
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let Ok(mut stream) = stream else { break };
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let body = json!({"value": jwt}).to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    url
}

fn checkout(dir: &Path) {
    fs::create_dir_all(dir.join("app")).unwrap();
    fs::write(dir.join("app/package.json"), r#"{"name":"app"}"#).unwrap();
    fs::write(
        dir.join("app/aster.toml"),
        r#"
[targets.build]
command = "bash -c 'mkdir -p out && echo hi > out/file.txt && echo ran >> runs.txt'"
cache = { enabled = true, outputs = ["out/**"] }
"#,
    )
    .unwrap();
    // A real git repo, so cache repo detection stops here instead of walking
    // up into the aster checkout and naming a different remote.
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "e2e")
            .env("GIT_AUTHOR_EMAIL", "e2e@example.com")
            .env("GIT_COMMITTER_NAME", "e2e")
            .env("GIT_COMMITTER_EMAIL", "e2e@example.com")
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    };
    git(&["init", "-q"]);
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "fixture"]);
}

/// `aster` with a clean environment apart from what each actor needs.
fn aster(dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_aster"));
    command
        .current_dir(dir)
        .env("GITHUB_REPOSITORY", REPO)
        .env_remove("ASTER_REMOTE_CACHE")
        .env_remove("ASTER_CACHE_MODE")
        .env_remove("ASTER_CACHE_DIR")
        .env_remove("ASTER_SRV_TOKEN")
        .env_remove("ASTER_SRV_PROXY_TOKEN")
        .env_remove("ACTIONS_ID_TOKEN_REQUEST_URL")
        .env_remove("ACTIONS_ID_TOKEN_REQUEST_TOKEN");
    command
}

fn build(mut command: Command) -> std::process::Output {
    let output = command
        .args(["run", "//app:build", "--no-deps"])
        .output()
        .expect("spawn aster");
    assert!(
        output.status.success(),
        "aster build failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn runs(dir: &Path) -> usize {
    fs::read_to_string(dir.join("app/runs.txt"))
        .unwrap_or_default()
        .lines()
        .count()
}

fn tree(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(dir).sort_by_file_name() {
        let entry = entry.unwrap();
        if entry.file_type().is_file() {
            let relative = entry.path().strip_prefix(dir).unwrap();
            files.push((
                relative.to_string_lossy().replace('\\', "/"),
                fs::read(entry.path()).unwrap(),
            ));
        }
    }
    files
}

/// What the browser got back from one request.
struct Reply {
    status: u16,
    location: Option<String>,
    body: String,
}

fn browser() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        // The redirect to the CLI's loopback port is followed by hand.
        .max_redirects(0)
        .max_redirects_will_error(false)
        .build()
        .into()
}

fn reply(mut response: ureq::http::Response<ureq::Body>) -> Reply {
    Reply {
        status: response.status().as_u16(),
        location: response
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        body: response.body_mut().read_to_string().unwrap_or_default(),
    }
}

/// The hidden fields of the confirmation form, as a browser would submit them.
fn form_body(html: &str) -> String {
    html.split("<input type=\"hidden\" name=\"")
        .skip(1)
        .map(|part| {
            let (name, rest) = part.split_once("\" value=\"").unwrap();
            let (value, _) = rest.split_once('"').unwrap();
            format!("{name}={value}")
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Run `aster cache login --no-browser` and return its stdout lines once it
/// exits, handing the sign-in URL it prints to `browse` while it waits.
fn login_with(
    dir: &Path,
    server: &str,
    config_home: &Path,
    browse: impl FnOnce(&str) + Send,
) -> (std::process::ExitStatus, Vec<String>) {
    let mut child = aster(dir)
        .args(["cache", "login", "--loopback", "--no-browser"])
        .env("ASTER_SRV_URL", server)
        .env("XDG_CONFIG_HOME", config_home)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn aster cache login");
    let stdout = child.stdout.take().unwrap();
    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        let mut lines = Vec::new();
        for line in BufReader::new(stdout).lines() {
            let line = line.unwrap();
            if line.starts_with("http") {
                let _ = url_tx.send(line.clone());
            }
            lines.push(line);
        }
        lines
    });
    let url = url_rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("aster cache login prints the sign-in URL");
    browse(&url);
    let status = child.wait().expect("aster cache login exits");
    (status, reader.join().unwrap())
}

/// `aster cache login` against the real server, with the scripted browser
/// playing a person behind IAP: GET the page, confirm, and follow the
/// server's redirect to the CLI's loopback listener.
async fn sign_in(
    server: &Running,
    dir: &Path,
    config_home: &Path,
) -> (std::process::ExitStatus, Vec<String>) {
    let url = server.url.clone();
    let assertion = server.iap_assertion(PERSON);
    tokio::task::spawn_blocking({
        let (dir, url, config) = (dir.to_path_buf(), url, config_home.to_path_buf());
        move || {
            login_with(&dir, &url, &config, |sign_in_url| {
                let agent = browser();
                // The confirmation page names who is signing in.
                let page = reply(
                    agent
                        .get(sign_in_url)
                        .header("x-goog-iap-jwt-assertion", &assertion)
                        .call()
                        .expect("GET /cli-auth"),
                );
                assert_eq!(page.status, 200, "{}", page.body);
                assert!(page.body.contains(PERSON), "{}", page.body);
                // The person clicks Authorize.
                let confirmed = reply(
                    agent
                        .post(format!("{url}/cli-auth"))
                        .header("content-type", "application/x-www-form-urlencoded")
                        .header("x-goog-iap-jwt-assertion", &assertion)
                        .header("origin", &url)
                        .send(form_body(&page.body))
                        .expect("POST /cli-auth"),
                );
                assert_eq!(confirmed.status, 302, "{}", confirmed.body);
                let callback = confirmed.location.expect("a redirect");
                assert!(
                    callback.starts_with("http://127.0.0.1:")
                        && callback.contains("/callback?code="),
                    "the redirect goes to the loopback listener only: {callback}"
                );
                // The browser lands on the CLI's listener, with no IAP header.
                let landed = reply(agent.get(&callback).call().expect("GET /callback"));
                assert_eq!(landed.status, 200, "{}", landed.body);
                assert!(landed.body.contains("close this tab"), "{}", landed.body);
            })
        }
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_laptop_signs_in_through_aster_srv_and_restores_what_ci_wrote() {
    let Some(server) = start().await else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let ci_checkout = root.path().join("ci").join("workspace");
    let early_laptop = root.path().join("laptop-before-login");
    let laptop = root.path().join("laptop").join("workspace");
    let config_home = root.path().join("laptop-config");
    for dir in [&ci_checkout, &early_laptop, &laptop] {
        checkout(dir);
    }
    let url = server.url.clone();

    // Phase 1: CI builds on main and writes the artifact. Its credential is a
    // GitHub OIDC token for the ci.yml job, fetched from the (stand-in)
    // Actions endpoint exactly as the real client does.
    let stop_oidc = Arc::new(AtomicBool::new(false));
    let oidc_url = github_oidc_endpoint(stop_oidc.clone());
    let first = tokio::task::spawn_blocking({
        let (dir, url) = (ci_checkout.clone(), url.clone());
        move || {
            let mut command = aster(&dir);
            command
                .env("ASTER_SRV_URL", &url)
                .env("ASTER_SRV_GCS_BASE", &url)
                .env("ACTIONS_ID_TOKEN_REQUEST_URL", oidc_url)
                .env("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "request-token");
            build(command)
        }
    })
    .await
    .unwrap();
    stop_oidc.store(true, Ordering::Relaxed);
    assert_eq!(
        runs(&ci_checkout),
        1,
        "CI must run the target: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let written = tree(&ci_checkout.join("app/out"));
    assert_eq!(written, vec![("file.txt".into(), b"hi\n".to_vec())]);
    assert!(
        !server.app.cache_store.objects().await.is_empty(),
        "CI must upload a blob into the fake GCS"
    );

    // Phase 2: a laptop with no sign-in. It says so once and builds locally.
    let before = tokio::task::spawn_blocking({
        let (dir, url, config) = (early_laptop.clone(), url.clone(), config_home.clone());
        move || {
            let mut command = aster(&dir);
            command
                .env("ASTER_SRV_URL", &url)
                .env("ASTER_SRV_GCS_BASE", &url)
                .env("XDG_CONFIG_HOME", &config);
            build(command)
        }
    })
    .await
    .unwrap();
    let before_stderr = String::from_utf8_lossy(&before.stderr);
    assert!(
        before_stderr.contains("not signed in; run aster cache login"),
        "{before_stderr}"
    );
    assert_eq!(runs(&early_laptop), 1, "no sign-in means no restore");

    // Phase 3: `aster cache login` against the real server. The scripted
    // browser is a person behind IAP: GET the page, confirm, and follow the
    // server's redirect to the CLI's loopback listener.
    let (status, lines) = sign_in(&server, &laptop, &config_home).await;
    let transcript = lines.join("\n");
    assert!(status.success(), "login failed: {transcript}");
    assert!(
        transcript.contains(&format!("Signed in as {PERSON}")),
        "{transcript}"
    );
    let token_file = config_home.join("aster/cache-tokens").join(format!(
        "{}.json",
        url.trim_start_matches("http://").replace(':', "_")
    ));
    let stored: Value = serde_json::from_str(&fs::read_to_string(&token_file).unwrap()).unwrap();
    let token = stored["token"].as_str().unwrap().to_string();
    assert_eq!(stored["server"], url.as_str());
    assert!(
        !transcript.contains(&token),
        "the token must not be printed: {transcript}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&token_file), 0o600, "the token file is private");
        assert_eq!(mode(token_file.parent().unwrap()), 0o700);
    }

    // Phase 4: the signed-in laptop builds in a clean checkout. It restores
    // what CI wrote, byte for byte, and does not run the command.
    let restored = tokio::task::spawn_blocking({
        let (dir, url, config) = (laptop.clone(), url.clone(), config_home.clone());
        move || {
            let mut command = aster(&dir);
            command
                .env("ASTER_SRV_URL", &url)
                .env("ASTER_SRV_GCS_BASE", &url)
                .env("XDG_CONFIG_HOME", &config);
            build(command)
        }
    })
    .await
    .unwrap();
    let stderr = String::from_utf8_lossy(&restored.stderr);
    assert_eq!(
        runs(&laptop),
        0,
        "the laptop must not run the target: {stderr}"
    );
    assert_eq!(
        tree(&laptop.join("app/out")),
        written,
        "the laptop must restore a byte-identical output tree: {stderr}"
    );
    assert!(
        !stderr.contains(&token),
        "the token must not be logged: {stderr}"
    );
    assert!(!stderr.contains("not signed in"), "{stderr}");

    // Phase 5: the person's token reads and nothing more. The server, not
    // just the client, refuses a write.
    let write = tokio::task::spawn_blocking({
        let (url, token) = (url.clone(), token.clone());
        move || {
            reply(
                browser()
                    .post(format!("{url}/v1/cache/upload"))
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .send(json!({"repo": REPO, "digests": ["0".repeat(64)]}).to_string())
                    .expect("POST /v1/cache/upload"),
            )
        }
    })
    .await
    .unwrap();
    assert_eq!(write.status, 403, "{}", write.body);

    server.stop().await;
}

/// What `aster srv <args>` printed and how it exited.
async fn srv(dir: &Path, url: &str, config_home: &Path, args: &[&str]) -> (bool, Value, String) {
    let output = tokio::task::spawn_blocking({
        let (dir, url, config) = (
            dir.to_path_buf(),
            url.to_string(),
            config_home.to_path_buf(),
        );
        let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        move || {
            aster(&dir)
                .arg("srv")
                .args(args)
                .env("ASTER_SRV_URL", url)
                .env("XDG_CONFIG_HOME", config)
                .output()
                .expect("spawn aster srv")
        }
    })
    .await
    .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    (
        output.status.success(),
        serde_json::from_str(&stdout).unwrap_or(Value::Null),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// One CI run of `//app:test`, as `--test-results` writes it: `flaky_test`
/// fails and then passes on retry, `broken_test` fails on every attempt.
fn ci_results(run: usize) -> String {
    let record = |value: Value| format!("{value}\n");
    let test = |id: &str, outcome: &str, attempt: u32, hash: Option<&str>| {
        record(json!({
            "type": "test", "target": "//app:test", "test_id": id, "file": "app/t.exs",
            "name": id, "outcome": outcome, "attempt": attempt, "duration_ms": 5,
            "failure_hash": hash, "message": hash.map(|_| format!("boom in {id}")),
        }))
    };
    [
        record(json!({
            "type": "run", "run_id": format!("e2e-run-{run}"), "repo": REPO,
            "sha": format!("{run:040}"), "branch": "main", "aster_version": "0.0.0",
            "started_at": chrono::Utc::now().to_rfc3339(),
        })),
        record(json!({
            "type": "target", "address": "//app:test", "input_hash": format!("h{run}"),
            "status": "failed", "cached": false, "duration_ms": 10, "confidence": "complete",
        })),
        test("flaky_test", "fail", 1, Some("flake-hash")),
        test("flaky_test", "pass", 2, None),
        test("broken_test", "fail", 1, Some("broken-hash")),
    ]
    .concat()
}

/// A person who signed in with `aster cache login` can read every dataset the
/// dashboard shows through `aster srv`, from data CI uploaded, and the server
/// refuses their writes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_signed_in_laptop_reads_every_dashboard_dataset_with_aster_srv() {
    let Some(server) = start().await else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let ci_checkout = root.path().join("ci").join("workspace");
    let laptop = root.path().join("laptop").join("workspace");
    let config_home = root.path().join("laptop-config");
    for dir in [&ci_checkout, &laptop] {
        checkout(dir);
    }
    let url = server.url.clone();

    // Phase 1: CI uploads three runs of a suite with a flaky test and a broken
    // one, with the job's OIDC token, through `aster results upload`.
    let stop_oidc = Arc::new(AtomicBool::new(false));
    let oidc_url = github_oidc_endpoint(stop_oidc.clone());
    for run in 1..=3 {
        let file = root.path().join(format!("results-{run}.ndjson"));
        fs::write(&file, ci_results(run)).unwrap();
        let uploaded = tokio::task::spawn_blocking({
            let (dir, url, oidc_url) = (ci_checkout.clone(), url.clone(), oidc_url.clone());
            move || {
                aster(&dir)
                    .args(["results", "upload", "--strict"])
                    .arg(&file)
                    .env("ASTER_SRV_URL", &url)
                    .env("ACTIONS_ID_TOKEN_REQUEST_URL", oidc_url)
                    .env("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "request-token")
                    .output()
                    .expect("spawn aster results upload")
            }
        })
        .await
        .unwrap();
        assert!(
            uploaded.status.success(),
            "upload {run}: {}",
            String::from_utf8_lossy(&uploaded.stderr)
        );
    }
    stop_oidc.store(true, Ordering::Relaxed);
    // The server's classifier decides flaky, broken and quarantined; run its
    // pass now instead of waiting for the schedule.
    aster_srv::test_results::classifier::run_once(
        &server.app,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap()
    .expect("the classifier lock is free");

    // Phase 2: before signing in, the laptop is told how, and prints no JSON.
    let (ok, body, stderr) = srv(&laptop, &url, &config_home, &["flaky"]).await;
    assert!(!ok && body.is_null(), "{body}");
    assert!(
        stderr.contains("not signed in; run aster cache login"),
        "{stderr}"
    );

    // Phase 3: sign in, as in the cache test.
    let (status, lines) = sign_in(&server, &laptop, &config_home).await;
    assert!(status.success(), "login failed: {}", lines.join("\n"));

    // Phase 4: one read per dataset. The repository is the checkout's
    // (GITHUB_REPOSITORY), which the person never typed.
    let today = chrono::Utc::now().date_naive().to_string();
    let durations = format!("/v1/test-results/durations?target=%2F%2Fapp%3Atest&day={today}");
    let ids = |body: &Value, key: &str| -> Vec<String> {
        body[key]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .map(|entry| entry["test_id"].as_str().unwrap_or_default().to_string())
            .collect()
    };
    let read = |args: &'static [&'static str]| {
        let (laptop, url, config_home) = (laptop.clone(), url.clone(), config_home.clone());
        async move {
            let (ok, body, stderr) = srv(&laptop, &url, &config_home, args).await;
            assert!(ok, "aster srv {args:?}: {stderr}");
            assert_eq!(body["repo"], REPO, "aster srv {args:?}: {body}");
            body
        }
    };
    assert!(read(&["cache"]).await["entries"].is_null());
    assert_eq!(ids(&read(&["flaky"]).await, "tests"), ["flaky_test"]);
    assert_eq!(ids(&read(&["broken"]).await, "tests"), ["broken_test"]);
    let quarantine = read(&["quarantine"]).await;
    assert_eq!(ids(&quarantine, "tests"), ["flaky_test"], "{quarantine}");
    let clusters = read(&["clusters"]).await;
    let hashes: Vec<&str> = clusters["clusters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|cluster| cluster["failure_hash"].as_str().unwrap())
        .collect();
    assert_eq!(hashes.len(), 2, "{clusters}");
    assert!(hashes.contains(&"flake-hash") && hashes.contains(&"broken-hash"));
    let history = read(&["test", "flaky_test"]).await;
    assert_eq!(history["test"]["state"], "flaky", "{history}");
    assert_eq!(history["history"].as_array().unwrap().len(), 6, "{history}");
    let by_path = {
        let (ok, body, stderr) = srv(
            &laptop,
            &url,
            &config_home,
            &["get", "/v1/test-results/runs/e2e-run-1/failures"],
        )
        .await;
        assert!(ok, "{stderr}");
        body
    };
    assert_eq!(by_path["run"]["repo"], REPO, "{by_path}");
    let (ok, durations_body, stderr) = srv(&laptop, &url, &config_home, &["get", &durations]).await;
    assert!(ok, "{stderr}");
    assert!(durations_body["files"].is_array(), "{durations_body}");

    // Phase 5: a repository the server does not serve, and writes, are 403.
    let (ok, body, stderr) = srv(
        &laptop,
        &url,
        &config_home,
        &["flaky", "--repo", "evil/other"],
    )
    .await;
    assert!(!ok && body.is_null(), "{body}");
    assert!(stderr.contains("403"), "{stderr}");
    let stored: Value = serde_json::from_str(
        &fs::read_to_string(config_home.join("aster/cache-tokens").join(format!(
            "{}.json",
            url.trim_start_matches("http://").replace(':', "_")
        )))
        .unwrap(),
    )
    .unwrap();
    let token = stored["token"].as_str().unwrap().to_string();
    for (path, body) in [
        ("/v1/test-results/runs", "{}".to_string()),
        (
            "/v1/cache/upload",
            json!({"repo": REPO, "digests": ["0".repeat(64)]}).to_string(),
        ),
        (
            "/v1/cache/commit",
            json!({"repo": REPO, "key": "k", "manifest": ""}).to_string(),
        ),
    ] {
        let write = tokio::task::spawn_blocking({
            let (url, token) = (url.clone(), token.clone());
            move || {
                reply(
                    browser()
                        .post(format!("{url}{path}"))
                        .header("authorization", format!("Bearer {token}"))
                        .header("content-type", "application/json")
                        .send(body)
                        .expect("POST"),
                )
            }
        })
        .await
        .unwrap();
        assert_eq!(write.status, 403, "POST {path}: {}", write.body);
    }

    server.stop().await;
}
/// Run `aster cache login` as a person in a remote shell would: stdin is a
/// pipe the person types into, and `ssh` makes the environment look like an
/// SSH session (so no flag is needed). `browse` gets the printed sign-in URL
/// and returns the line to paste, or `None` to type nothing.
fn copy_login_with(
    dir: &Path,
    server: &str,
    config_home: &Path,
    flags: &[&str],
    ssh: bool,
    browse: impl FnOnce(&str) -> Option<String> + Send,
) -> (std::process::ExitStatus, Vec<String>, String) {
    let mut command = aster(dir);
    command
        .args(["cache", "login"])
        .args(flags)
        .env("ASTER_SRV_URL", server)
        .env("XDG_CONFIG_HOME", config_home)
        // A real SSH session has no display; make the test host look alike.
        .env_remove("SSH_CONNECTION")
        .env_remove("SSH_CLIENT")
        .env_remove("SSH_TTY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if ssh {
        command.env("SSH_CONNECTION", "203.0.113.9 50000 10.0.0.2 22");
    }
    let mut child = command.spawn().expect("spawn aster cache login");
    let stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        let mut lines = Vec::new();
        for line in BufReader::new(stdout).lines() {
            let line = line.unwrap();
            if line.starts_with("http") {
                let _ = url_tx.send(line.clone());
            }
            lines.push(line);
        }
        lines
    });
    let errors = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let url = url_rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("aster cache login prints the sign-in URL");
    let mut stdin = child.stdin.take().unwrap();
    if let Some(paste) = browse(&url) {
        writeln!(stdin, "{paste}").unwrap();
    }
    drop(stdin);
    let status = child.wait().expect("aster cache login exits");
    (status, reader.join().unwrap(), errors.join().unwrap())
}

fn query_value(url: &str, name: &str) -> String {
    url.split(['?', '&'])
        .find_map(|part| part.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("{name} in {url}"))
        .to_string()
}

/// The code a copy-mode page shows.
fn shown_code(html: &str) -> String {
    html.split("<pre><code>")
        .nth(1)
        .expect("the page shows a code")
        .split("</code>")
        .next()
        .unwrap()
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_remote_shell_signs_in_by_pasting_a_code_and_restores_what_ci_wrote() {
    let Some(server) = start().await else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let ci_checkout = root.path().join("ci").join("workspace");
    let devbox = root.path().join("devbox").join("workspace");
    let config_home = root.path().join("devbox-config");
    for dir in [&ci_checkout, &devbox] {
        checkout(dir);
    }
    let url = server.url.clone();

    // Phase 1: CI builds on main and writes the artifact, as in the laptop
    // test: a GitHub OIDC token from the (stand-in) Actions endpoint.
    let stop_oidc = Arc::new(AtomicBool::new(false));
    let oidc_url = github_oidc_endpoint(stop_oidc.clone());
    tokio::task::spawn_blocking({
        let (dir, url) = (ci_checkout.clone(), url.clone());
        move || {
            let mut command = aster(&dir);
            command
                .env("ASTER_SRV_URL", &url)
                .env("ASTER_SRV_GCS_BASE", &url)
                .env("ACTIONS_ID_TOKEN_REQUEST_URL", oidc_url)
                .env("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "request-token");
            build(command)
        }
    })
    .await
    .unwrap();
    stop_oidc.store(true, Ordering::Relaxed);
    let written = tree(&ci_checkout.join("app/out"));
    assert_eq!(written, vec![("file.txt".into(), b"hi\n".to_vec())]);

    let assertion = server.iap_assertion(PERSON);
    let token_file = config_home.join("aster/cache-tokens").join(format!(
        "{}.json",
        url.trim_start_matches("http://").replace(':', "_")
    ));

    // Phase 2: a sign-in whose code is stolen before the person pastes it.
    // The person's browser confirms, but another party redeems the code with
    // a verifier of its own. The CLI's paste is then refused and stores nothing.
    let (status, lines, _) = tokio::task::spawn_blocking({
        let (dir, url, config, assertion) = (
            devbox.clone(),
            url.clone(),
            config_home.clone(),
            assertion.clone(),
        );
        move || {
            copy_login_with(&dir, &url, &config, &[], true, |sign_in_url| {
                let agent = browser();
                let page = reply(
                    agent
                        .get(sign_in_url)
                        .header("x-goog-iap-jwt-assertion", &assertion)
                        .call()
                        .unwrap(),
                );
                assert_eq!(page.status, 200, "{}", page.body);
                let code = confirm_copy(&agent, &url, &assertion, &page);
                // Someone who saw the code but not the CLI's verifier.
                let thief = reply(
                    agent
                        .post(format!("{url}/v1/auth/token"))
                        .header("content-type", "application/json")
                        .send(
                            json!({
                                "code": code,
                                "verifier": "v".repeat(43),
                                "user_code": query_value(sign_in_url, "user_code"),
                            })
                            .to_string(),
                        )
                        .unwrap(),
                );
                assert_eq!(thief.status, 400, "a wrong verifier is refused");
                Some(code)
            })
        }
    })
    .await
    .unwrap();
    assert!(!status.success(), "a spent code cannot sign in: {lines:?}");
    assert!(!token_file.exists(), "a refused sign-in stores nothing");

    // Phase 3: the real sign-in. No flag: SSH_CONNECTION alone picks the
    // pasted-code flow. The scripted browser plays a person behind IAP.
    let (status, lines, stderr) = tokio::task::spawn_blocking({
        let (dir, url, config, assertion) = (
            devbox.clone(),
            url.clone(),
            config_home.clone(),
            assertion.clone(),
        );
        move || {
            copy_login_with(&dir, &url, &config, &[], true, |sign_in_url| {
                assert!(
                    sign_in_url.contains("/cli-auth?mode=copy&") && !sign_in_url.contains("port="),
                    "copy mode names no port: {sign_in_url}"
                );
                let agent = browser();
                let page = reply(
                    agent
                        .get(sign_in_url)
                        .header("x-goog-iap-jwt-assertion", &assertion)
                        .call()
                        .unwrap(),
                );
                assert_eq!(page.status, 200, "{}", page.body);
                let user_code = query_value(sign_in_url, "user_code");
                assert!(
                    page.body
                        .contains(&format!("{}-{}", &user_code[..4], &user_code[4..])),
                    "the page shows the code the terminal printed: {}",
                    page.body
                );

                // A loopback-shaped replay of this form cannot make a code:
                // adding a port, or dropping the mode, breaks the MAC.
                let form = form_body(&page.body);
                for tampered in [format!("{form}&port=40000"), form.replace("mode=copy", "")] {
                    let refused = reply(
                        agent
                            .post(format!("{url}/cli-auth"))
                            .header("content-type", "application/x-www-form-urlencoded")
                            .header("x-goog-iap-jwt-assertion", &assertion)
                            .header("origin", &url)
                            .send(tampered)
                            .unwrap(),
                    );
                    assert_eq!(refused.status, 400, "{}", refused.body);
                    assert!(refused.location.is_none());
                }
                // And a loopback page's form cannot be turned into a copy code.
                let loopback_page = reply(
                    agent
                        .get(format!(
                            "{}&port=40000",
                            sign_in_url.replace("mode=copy&", "")
                        ))
                        .header("x-goog-iap-jwt-assertion", &assertion)
                        .call()
                        .unwrap(),
                );
                assert_eq!(loopback_page.status, 200, "{}", loopback_page.body);
                let as_copy = reply(
                    agent
                        .post(format!("{url}/cli-auth"))
                        .header("content-type", "application/x-www-form-urlencoded")
                        .header("x-goog-iap-jwt-assertion", &assertion)
                        .header("origin", &url)
                        .send(format!("{}&mode=copy", form_body(&loopback_page.body)))
                        .unwrap(),
                );
                assert_eq!(as_copy.status, 400, "{}", as_copy.body);

                Some(confirm_copy(&agent, &url, &assertion, &page))
            })
        }
    })
    .await
    .unwrap();
    let transcript = lines.join("\n");
    assert!(status.success(), "login failed: {transcript}\n{stderr}");
    assert!(
        transcript.contains("looks like a remote shell"),
        "{transcript}"
    );
    assert!(
        transcript.contains(&format!("Signed in as {PERSON}")),
        "{transcript}"
    );
    let stored: Value = serde_json::from_str(&fs::read_to_string(&token_file).unwrap()).unwrap();
    let token = stored["token"].as_str().unwrap().to_string();
    assert!(
        !transcript.contains(&token) && !stderr.contains(&token),
        "the token must not be printed"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&token_file), 0o600);
        assert_eq!(mode(token_file.parent().unwrap()), 0o700);
    }

    // Phase 4: the signed-in devbox restores what CI wrote, byte for byte,
    // without running the command.
    let restored = tokio::task::spawn_blocking({
        let (dir, url, config) = (devbox.clone(), url.clone(), config_home.clone());
        move || {
            let mut command = aster(&dir);
            command
                .env("ASTER_SRV_URL", &url)
                .env("ASTER_SRV_GCS_BASE", &url)
                .env("XDG_CONFIG_HOME", &config);
            build(command)
        }
    })
    .await
    .unwrap();
    let restore_stderr = String::from_utf8_lossy(&restored.stderr);
    assert_eq!(
        runs(&devbox),
        0,
        "must not run the target: {restore_stderr}"
    );
    assert_eq!(tree(&devbox.join("app/out")), written, "{restore_stderr}");
    assert!(!restore_stderr.contains(&token), "{restore_stderr}");

    // Phase 5: `aster cache logout` deletes the file and the server refuses
    // the old token from then on.
    let logout = tokio::task::spawn_blocking({
        let (dir, url, config) = (devbox.clone(), url.clone(), config_home.clone());
        move || {
            aster(&dir)
                .args(["cache", "logout"])
                .env("ASTER_SRV_URL", &url)
                .env("XDG_CONFIG_HOME", &config)
                .output()
                .unwrap()
        }
    })
    .await
    .unwrap();
    assert!(
        logout.status.success(),
        "{}",
        String::from_utf8_lossy(&logout.stderr)
    );
    assert!(!token_file.exists(), "logout deletes the token file");
    let after = tokio::task::spawn_blocking({
        let (url, token) = (url.clone(), token.clone());
        move || {
            reply(
                browser()
                    .post(format!("{url}/v1/cache/lookup"))
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .send(json!({"repo": REPO, "keys": ["k"]}).to_string())
                    .unwrap(),
            )
        }
    })
    .await
    .unwrap();
    assert_eq!(
        after.status, 401,
        "a revoked token is refused: {}",
        after.body
    );

    server.stop().await;
}

/// Click Authorize on `page` and read the code off the answer. A copy-mode
/// answer is a page, never a redirect, and never carries a token.
fn confirm_copy(agent: &ureq::Agent, url: &str, assertion: &str, page: &Reply) -> String {
    let done = reply(
        agent
            .post(format!("{url}/cli-auth"))
            .header("content-type", "application/x-www-form-urlencoded")
            .header("x-goog-iap-jwt-assertion", assertion)
            .header("origin", url)
            .send(form_body(&page.body))
            .expect("POST /cli-auth"),
    );
    assert_eq!(done.status, 200, "copy mode never redirects: {}", done.body);
    assert!(done.location.is_none());
    assert!(!done.body.contains("eyJ"), "no token on the page");
    shown_code(&done.body)
}
