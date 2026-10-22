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
const IAP_AUDIENCE: &str = "/projects/123456/global/backendServices/";
const CLI_KEY: &str = "e2e1:cli-e2e-signing-secret-0123456789abcdef";

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
                "aud": format!("{IAP_AUDIENCE}4242"),
                "exp": chrono::Utc::now().timestamp() + 300,
                "email": email,
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
    let database = format!(
        "aster_login_e2e_{}_{}",
        std::process::id(),
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
        "ASTER_SRV_UI_EMAIL_DOMAINS" => Some("example.com".into()),
        "ASTER_SRV_PUBLIC_URL" => Some(url.clone()),
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
        .args(["cache", "login", "--no-browser"])
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
    let assertion = server.iap_assertion(PERSON);
    let (status, lines) = tokio::task::spawn_blocking({
        let (dir, url, config) = (laptop.clone(), url.clone(), config_home.clone());
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
    .unwrap();
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
