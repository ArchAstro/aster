//! End-to-end remote cache: real aster-srv, real Postgres, fake GCS.
//!
//! Checkout A runs a target and stores through aster-srv. The signed blob
//! URLs the server returns are rewritten onto the server itself, which holds
//! the bytes in its in-memory bucket. Checkout B, at another path and with
//! an empty local cache, restores those bytes and does not run the command.
//!
//! Set `ASTER_SRV_TEST_DATABASE_URL` to a Postgres URL whose user may create
//! databases. Without it the test skips, the same way the aster-srv suite does.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use aster_srv::auth::Verifier;
use aster_srv::cache::{BlobStore, FixedSigner};
use aster_srv::raw_store::RawStore;
use aster_srv::{App, Config};

const PUBLIC: &[u8] = include_bytes!("fixtures/test-oidc-key.pub.pem");
const REPO: &str = "acme/app";
const TOKEN: &str = "e2e-token-0123456789abcdef";

struct Running {
    url: String,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    app: Arc<App>,
    admin_url: String,
    database: String,
    accept: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
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
}

async fn start() -> Option<Running> {
    let Ok(admin_url) = std::env::var("ASTER_SRV_TEST_DATABASE_URL") else {
        eprintln!("skipping: ASTER_SRV_TEST_DATABASE_URL is not set");
        return None;
    };
    let database = format!(
        "aster_e2e_{}_{}",
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

    let config = Config::from_env(|name| match name {
        "ASTER_SRV_DATABASE_URL" => Some(database_url.clone()),
        "ASTER_SRV_REPOSITORIES" => Some("acme/*".into()),
        "ASTER_SRV_CACHE_BUCKET" => Some("aster-cache-e2e".into()),
        "ASTER_SRV_CACHE_SIGNER" => Some("cache-e2e@example.iam.gserviceaccount.com".into()),
        "ASTER_SRV_STATIC_TOKENS" => Some(format!("{REPO}={TOKEN}")),
        _ => None,
    })
    .expect("test config");
    let verifier = Verifier::new(&config).with_key(
        "test",
        jsonwebtoken::DecodingKey::from_rsa_pem(PUBLIC).unwrap(),
    );
    let iap = aster_srv::iap::IapVerifier::new(&config);
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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (shutdown, stopped) = tokio::sync::oneshot::channel::<()>();
    let serving = Arc::clone(&app);
    let accept = async move {
        let _ = aster_srv::serve(serving, listener, async {
            let _ = stopped.await;
        })
        .await;
    };
    Some(Running {
        url,
        shutdown: Some(shutdown),
        app,
        admin_url,
        database,
        accept: Box::pin(accept),
    })
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

fn run_blocking(dir: &Path, server: &str) -> std::process::Output {
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(dir)
        .env("ASTER_SRV_URL", server)
        .env("ASTER_SRV_TOKEN", TOKEN)
        .env("ASTER_SRV_GCS_BASE", server)
        .env("GITHUB_REPOSITORY", REPO)
        .env_remove("ASTER_REMOTE_CACHE")
        .env_remove("ASTER_CACHE_MODE")
        .env_remove("ASTER_CACHE_DIR")
        .args(["run", "//app:build", "--no-deps"])
        .output()
        .expect("spawn aster");
    assert!(
        output.status.success(),
        "aster failed in {}: stdout={} stderr={}",
        dir.display(),
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

#[tokio::test]
async fn remote_restore_across_checkouts_through_aster_srv() {
    let Some(mut server) = start().await else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let a = root.path().join("workspace-a");
    let b = root.path().join("other-machine").join("workspace-b");
    checkout(&a);
    checkout(&b);

    let first = tokio::select! {
        _ = &mut server.accept => unreachable!("accept ended"),
        output = tokio::task::spawn_blocking({
            let url = server.url.clone();
            let a = a.clone();
            move || run_blocking(&a, &url)
        }) => output.expect("aster thread"),
    };
    assert_eq!(
        runs(&a),
        1,
        "checkout A must run the target: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let stored = tree(&a.join("app/out"));
    assert_eq!(stored, vec![("file.txt".into(), b"hi\n".to_vec())]);
    assert!(
        !server.app.cache_store.objects().await.is_empty(),
        "A must upload a blob into the fake GCS"
    );

    let restored = tokio::select! {
        _ = &mut server.accept => unreachable!("accept ended"),
        output = tokio::task::spawn_blocking({
            let url = server.url.clone();
            let b = b.clone();
            move || run_blocking(&b, &url)
        }) => output.expect("aster thread"),
    };
    let stderr = String::from_utf8_lossy(&restored.stderr);
    assert_eq!(runs(&b), 0, "checkout B must not run the target: {stderr}");
    assert_eq!(
        tree(&b.join("app/out")),
        stored,
        "checkout B must restore a byte-identical output tree: {stderr}"
    );
    assert!(
        !stderr.contains(TOKEN),
        "the token must not be logged: {stderr}"
    );

    server.stop().await;
}
