#![allow(clippy::disallowed_macros)] // test diagnostics may use std print macros
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
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use aster_srv::auth::Verifier;
use aster_srv::cache::{BlobStore, FanoutConfig, FixedSigner};
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
    start_with(None).await
}

/// [`start`], over a chosen blob store instead of the default in-memory one.
async fn start_with(store: Option<BlobStore>) -> Option<Running> {
    let Ok(admin_url) = std::env::var("ASTER_SRV_TEST_DATABASE_URL") else {
        eprintln!("skipping: ASTER_SRV_TEST_DATABASE_URL is not set");
        return None;
    };
    // The tests in this file start in parallel, so a clock reading alone can
    // name the same database twice.
    static NEXT_DATABASE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let database = format!(
        "aster_e2e_{}_{}_{}",
        std::process::id(),
        NEXT_DATABASE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
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
        "ASTER_SRV_DEV_BLOB_ROUTE" => Some("1".into()),
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
    let store = store.unwrap_or_else(|| BlobStore::memory("aster-cache-e2e", signer));
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
# runs.txt only counts executions. Hashing it would change the key after the
# command writes it, so the result would never be stored or restored.
cache = { enabled = true, outputs = ["out/**"], exclude = ["runs.txt"] }
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
    run_target(dir, server, "//app:build", &["--no-deps"])
}

/// `aster run <target> <extra>` in `dir` against `server`, which must pass.
fn run_target(dir: &Path, server: &str, target: &str, extra: &[&str]) -> std::process::Output {
    let output = Command::new(env!("CARGO_BIN_EXE_aster"))
        .current_dir(dir)
        .env("ASTER_SRV_URL", server)
        .env("ASTER_SRV_TOKEN", TOKEN)
        .env("ASTER_SRV_GCS_BASE", server)
        .env("GITHUB_REPOSITORY", REPO)
        .env_remove("ASTER_REMOTE_CACHE")
        .env_remove("ASTER_CACHE_MODE")
        .env_remove("ASTER_CACHE_DIR")
        .args(["run", target])
        .args(extra)
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

/// The `remote-io: ...` line a run prints on stderr, if it did remote I/O.
fn io_line(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .find(|line| line.starts_with("remote-io: "))
        .unwrap_or_default()
        .to_string()
}

/// The `remote: ...` line a run prints on stderr. Exactly one per run.
fn summary_line(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lines: Vec<&str> = stderr
        .lines()
        .filter(|line| line.starts_with("remote: "))
        .collect();
    assert_eq!(lines.len(), 1, "expected one summary line in: {stderr}");
    lines[0].to_string()
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
    // The run ends with one line saying what the remote did: A asked, found
    // nothing, ran the target, and committed its manifest.
    assert_eq!(
        summary_line(&first),
        "remote: 0 hits / 1 misses / 1 uploads",
        "{}",
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
    // B restored from the remote and had nothing to upload.
    assert_eq!(
        summary_line(&restored),
        "remote: 1 hits / 0 misses / 0 uploads",
        "{stderr}"
    );
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

/// One `aster cache save|restore` in `dir`, with `$GITHUB_OUTPUT` captured.
fn dir_cache(dir: &Path, server: &str, args: &[&str], mode: Option<&str>) -> (String, String) {
    fs::create_dir_all(dir).unwrap();
    let outputs = dir.join("github-output");
    let mut command = Command::new(env!("CARGO_BIN_EXE_aster"));
    command
        .current_dir(dir)
        .env("HOME", dir)
        .env("ASTER_SRV_URL", server)
        .env("ASTER_SRV_TOKEN", TOKEN)
        .env("ASTER_SRV_GCS_BASE", server)
        .env("GITHUB_REPOSITORY", REPO)
        .env("GITHUB_OUTPUT", &outputs)
        .env_remove("ASTER_REMOTE_CACHE")
        .env_remove("ASTER_CACHE_MODE")
        .args(["cache"])
        .args(args);
    if let Some(mode) = mode {
        command.env("ASTER_CACHE_MODE", mode);
    }
    let output = command.output().expect("spawn aster");
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(output.status.success(), "aster cache failed: {stderr}");
    let written = fs::read_to_string(&outputs).unwrap_or_default();
    let _ = fs::remove_file(&outputs);
    (written, stderr)
}

/// Bytes zstd cannot shrink, so an entry is forced across several chunks.
fn incompressible(length: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    let mut bytes = Vec::with_capacity(length + 8);
    while bytes.len() < length {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        bytes.extend_from_slice(&state.to_le_bytes());
    }
    bytes.truncate(length);
    bytes
}

/// A real Cargo-cache-sized entry, 5 GiB after compression. Run by hand with
/// `--ignored --release`: it needs about 15 GiB of memory and disk.
///
/// Real: the release aster binary, aster-srv's routes, Postgres. Faked: GCS,
/// by the server's in-memory bucket, which hashes one chunk at a time and is
/// therefore slower at commit than the real bucket's concurrent reads.
#[tokio::test]
#[ignore = "needs about 15 GiB; run by hand"]
async fn a_five_gigabyte_directory_cache_round_trips() {
    let Some(mut server) = start().await else {
        return;
    };
    let url = server.url.clone();
    let root = tempfile::tempdir().unwrap();
    let writer = root.path().join("writer");

    // Five files of bytes zstd cannot shrink, each linked a second time the
    // way Cargo links an artifact: the entry must hold one copy of each.
    fs::create_dir_all(writer.join("target/deps")).unwrap();
    let mut expected = Vec::new();
    for index in 0..5_u8 {
        let mut body = incompressible(1024 * 1024 * 1024);
        body[0] = index;
        let name = format!("target/deps/artifact-{index}");
        fs::write(writer.join(&name), &body).unwrap();
        fs::hard_link(
            writer.join(&name),
            writer.join(format!("target/artifact-{index}")),
        )
        .unwrap();
        expected.push((name, sha256(&body)));
    }

    let started = std::time::Instant::now();
    let saving = {
        let (url, writer) = (url.clone(), writer.clone());
        tokio::task::spawn_blocking(move || {
            dir_cache(
                &writer,
                &url,
                &["save", "--key", "cargo-1", "--path", "target"],
                None,
            )
        })
    };
    let (_, stderr) = tokio::select! {
        _ = &mut server.accept => unreachable!("accept ended"),
        done = saving => done.unwrap(),
    };
    let saved_in = started.elapsed();
    assert!(stderr.contains("stored cargo-1"), "{stderr}");
    let chunks = server.app.cache_store.objects().await.len();
    assert!(
        (160..=172).contains(&chunks),
        "5 GiB is about 161 chunks, one copy of each file; got {chunks}"
    );

    let started = std::time::Instant::now();
    let machine = root.path().join("other-machine");
    let restoring = {
        let (url, machine) = (url.clone(), machine.clone());
        tokio::task::spawn_blocking(move || {
            dir_cache(
                &machine,
                &url,
                &["restore", "--key", "cargo-1", "--path", "target"],
                None,
            )
        })
    };
    let (outputs, stderr) = tokio::select! {
        _ = &mut server.accept => unreachable!("accept ended"),
        done = restoring => done.unwrap(),
    };
    let restored_in = started.elapsed();
    assert!(outputs.contains("cache-hit=true\n"), "{outputs}{stderr}");
    for (name, digest) in &expected {
        assert_eq!(&sha256(&fs::read(machine.join(name)).unwrap()), digest);
    }
    eprintln!("5 GiB: {chunks} chunks, saved in {saved_in:?}, restored in {restored_in:?}");
    server.stop().await;
}

fn sha256(bytes: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).to_vec()
}

/// The `actions/cache` contract, end to end: a main-branch job stores a
/// dependency tree, and jobs on other machines restore it by exact key or as
/// the newest entry under a prefix.
///
/// Real: the aster binary, aster-srv's routes, migrations and Postgres.
/// Faked: GCS, by the server's in-memory bucket behind the signed-URL path.
#[tokio::test]
async fn a_directory_cache_restores_by_exact_key_and_by_newest_prefix() {
    let Some(mut server) = start().await else {
        return;
    };
    let url = server.url.clone();
    let root = tempfile::tempdir().unwrap();
    let writer = root.path().join("writer");

    // A writer stores two generations of a tree. The first is 70 MiB of
    // incompressible data, so it has to travel as several 32 MiB chunks.
    let large = incompressible(70 * 1024 * 1024);
    fs::create_dir_all(writer.join("deps/pkg/incremental")).unwrap();
    fs::write(writer.join("deps/pkg/large.bin"), &large).unwrap();
    fs::write(writer.join("deps/pkg/incremental/skip"), b"not stored").unwrap();
    fs::create_dir_all(writer.join(".tool")).unwrap();
    fs::write(writer.join(".tool/bin"), b"tool in home").unwrap();
    fs::create_dir_all(writer.join("tools")).unwrap();
    fs::write(writer.join("tools/esbuild-0.25"), b"matched").unwrap();
    fs::write(writer.join("tools/unrelated"), b"not stored").unwrap();
    let paths = [
        "--path",
        "deps",
        "--path",
        "~/.tool",
        "--path",
        "tools/esbuild-*",
    ];
    let save = |key: &'static str| {
        let (url, writer) = (url.clone(), writer.clone());
        let mut args = vec!["save", "--key", key, "--exclude", "**/incremental"];
        args.extend(paths);
        tokio::task::spawn_blocking(move || dir_cache(&writer, &url, &args, None))
    };
    let first = tokio::select! {
        _ = &mut server.accept => unreachable!("accept ended"),
        done = save("deps-linux-1") => done.unwrap(),
    };
    assert!(first.1.contains("stored deps-linux-1"), "{}", first.1);
    let chunks = server.app.cache_store.objects().await.len();
    assert!(chunks >= 3, "70 MiB must be several chunks, got {chunks}");

    // Postgres orders generations by created_at, so they must not tie.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    fs::write(writer.join("deps/pkg/version"), b"generation two").unwrap();
    let second = tokio::select! {
        _ = &mut server.accept => unreachable!("accept ended"),
        done = save("deps-linux-2") => done.unwrap(),
    };
    assert!(second.1.contains("stored deps-linux-2"), "{}", second.1);

    // Saving a key that exists is a quiet no-op, as actions/cache is.
    let again = tokio::select! {
        _ = &mut server.accept => unreachable!("accept ended"),
        done = save("deps-linux-1") => done.unwrap(),
    };
    assert!(again.1.contains("already stored"), "{}", again.1);

    let restore = |dir: std::path::PathBuf, extra: Vec<&'static str>| {
        let url = url.clone();
        let mut args = vec!["restore"];
        args.extend(extra);
        args.extend(paths);
        tokio::task::spawn_blocking(move || dir_cache(&dir, &url, &args, None))
    };

    // Exact key: this generation, reported as a hit.
    let exact = root.path().join("exact-machine");
    let (outputs, stderr) = tokio::select! {
        _ = &mut server.accept => unreachable!("accept ended"),
        done = restore(exact.clone(), vec!["--key", "deps-linux-1"]) => done.unwrap(),
    };
    assert!(outputs.contains("cache-hit=true\n"), "{outputs}{stderr}");
    assert!(
        outputs.contains("cache-matched-key=deps-linux-1\n"),
        "{outputs}"
    );
    assert_eq!(fs::read(exact.join("deps/pkg/large.bin")).unwrap(), large);
    assert_eq!(fs::read(exact.join(".tool/bin")).unwrap(), b"tool in home");
    assert_eq!(
        fs::read(exact.join("tools/esbuild-0.25")).unwrap(),
        b"matched"
    );
    assert!(!exact.join("tools/unrelated").exists(), "glob ignored");
    assert!(
        !exact.join("deps/pkg/incremental").exists(),
        "the exclude was ignored"
    );
    assert!(!exact.join("deps/pkg/version").exists(), "wrong generation");

    // A changed lockfile: no exact entry, so the newest under the prefix.
    let fallback = root.path().join("fallback-machine");
    let args = vec![
        // A blank restore-keys line is ignored, as in actions/cache.
        "--restore-key",
        "",
        "--key",
        "deps-linux-9",
        "--restore-key",
        "nope-",
        "--restore-key",
        "deps-linux-",
    ];
    let (outputs, stderr) = tokio::select! {
        _ = &mut server.accept => unreachable!("accept ended"),
        done = restore(fallback.clone(), args) => done.unwrap(),
    };
    assert!(outputs.contains("cache-hit=false\n"), "{outputs}{stderr}");
    assert!(
        outputs.contains("cache-matched-key=deps-linux-2\n"),
        "{outputs}{stderr}"
    );
    assert_eq!(
        fs::read(fallback.join("deps/pkg/version")).unwrap(),
        b"generation two"
    );

    // Nothing matches: not an error, and nothing is written.
    let cold = root.path().join("cold-machine");
    let (outputs, _) = tokio::select! {
        _ = &mut server.accept => unreachable!("accept ended"),
        done = restore(cold.clone(), vec!["--key", "other", "--restore-key", "zzz"]) => done.unwrap(),
    };
    assert!(outputs.contains("cache-hit=false\n"), "{outputs}");
    assert!(outputs.contains("cache-matched-key=\n"), "{outputs}");
    assert!(!cold.join("deps").exists());

    // The path list is part of an entry's identity.
    let other_paths = root.path().join("other-paths");
    let (outputs, _) = tokio::select! {
        _ = &mut server.accept => unreachable!("accept ended"),
        done = tokio::task::spawn_blocking({
            let (url, dir) = (url.clone(), other_paths.clone());
            move || dir_cache(&dir, &url, &["restore", "--key", "deps-linux-1", "--path", "deps"], None)
        }) => done.unwrap(),
    };
    assert!(outputs.contains("cache-matched-key=\n"), "{outputs}");

    // A pull-request job runs with ASTER_CACHE_MODE=read: it stores nothing.
    let before = server.app.cache_store.objects().await.len();
    let (_, stderr) = tokio::select! {
        _ = &mut server.accept => unreachable!("accept ended"),
        done = tokio::task::spawn_blocking({
            let (url, dir) = (url.clone(), writer.clone());
            move || dir_cache(&dir, &url, &["save", "--key", "pr-key", "--path", "deps"], Some("read"))
        }) => done.unwrap(),
    };
    assert!(stderr.contains("does not write"), "{stderr}");
    assert_eq!(server.app.cache_store.objects().await.len(), before);
    assert!(!stderr.contains(TOKEN), "the token must not be logged");

    server.stop().await;
}

/// A gateway that cannot be reached never fails the build, and the summary is
/// where a person sees that the remote did nothing. No server is needed: the
/// URL names a port nothing listens on.
#[test]
fn unreachable_remote_is_counted_in_the_summary_and_the_build_still_passes() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("workspace");
    checkout(&dir);
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };

    let output = run_blocking(&dir, &closed);

    assert_eq!(runs(&dir), 1, "the target runs locally");
    // The batch lookup, the target's own lookup and the store each failed
    // once. Only the last two count toward the three it takes to disable the
    // remote (a batch never disables), so there is no `disabled:` suffix.
    assert_eq!(
        summary_line(&output),
        "remote: 0 hits / 0 misses / 0 uploads / 3 failures",
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

const FILES: usize = 700;

/// A checkout whose `build` writes the shape of a front-end bundle: hundreds
/// of small files in nested directories, a few MB of binary, an executable, a
/// symlink and an empty directory.
fn many_file_checkout(dir: &Path) {
    fs::create_dir_all(dir.join("app")).unwrap();
    fs::write(dir.join("app/package.json"), r#"{"name":"app"}"#).unwrap();
    fs::write(
        dir.join("app/build.sh"),
        format!(
            "set -e\n\
             mkdir -p out/assets out/empty\n\
             for i in $(seq 1 {FILES}); do echo \"export const n = $i;\" > out/assets/f$i.js; done\n\
             head -c 3000000 /dev/urandom > out/bundle.bin\n\
             printf '#!/bin/sh\\n' > out/run.sh\n\
             chmod 755 out/run.sh\n\
             ln -s assets/f1.js out/current\n\
             echo ran >> runs.txt\n"
        ),
    )
    .unwrap();
    fs::write(
        dir.join("app/aster.toml"),
        r#"
[targets.build]
command = "bash build.sh"
cache = { enabled = true, outputs = ["out/**"], exclude = ["runs.txt"] }
"#,
    )
    .unwrap();
    git_init(dir);
}

fn git_init(dir: &Path) {
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

/// Every path under `root` with what restore must reproduce: file bytes,
/// symlink target or directory marker, and the permission bits.
fn listing(root: &Path) -> Vec<(String, String, u32)> {
    use std::os::unix::fs::PermissionsExt;
    let mut rows = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
    {
        let entry = entry.unwrap();
        let relative = entry
            .path()
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let metadata = fs::symlink_metadata(entry.path()).unwrap();
        let what = if metadata.file_type().is_symlink() {
            format!("link:{}", fs::read_link(entry.path()).unwrap().display())
        } else if metadata.is_dir() {
            "dir".to_string()
        } else {
            let digest = sha256(&fs::read(entry.path()).unwrap());
            format!(
                "file:{}",
                digest
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            )
        };
        rows.push((relative, what, metadata.permissions().mode() & 0o777));
    }
    rows
}

/// A bundle of 700 files is one blob on the server, one lookup entry and one
/// download, and restores byte for byte on another machine.
///
/// Before archives, this target was 701 blobs: the server's lookup resolved
/// each one (a GCS HEAD and a signature apiece) inside a 3 second budget, and
/// a client restored them one file at a time.
///
/// Real: the aster binary (two checkouts, two empty local caches), aster-srv's
/// routes and Postgres. Faked: GCS, by the server's in-memory bucket behind
/// the signed-URL path. Integration matrix: lookup, restore and upload each
/// cross the real HTTP boundary here; the budget-exceeded and corrupt-chunk
/// cells are the two tests that follow, and old-client behaviour is the
/// manifest decode test in `src/cache/artifact.rs`.
#[tokio::test]
async fn a_seven_hundred_file_target_is_one_archive_through_aster_srv() {
    let Some(mut server) = start().await else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let a = root.path().join("workspace-a");
    let b = root.path().join("other-machine").join("workspace-b");
    many_file_checkout(&a);
    many_file_checkout(&b);

    // Phase 1, upload: checkout A runs the build, which writes the files.
    // The archive goes to the bucket and the manifest is committed.
    let first = tokio::select! {
        _ = &mut server.accept => unreachable!("accept ended"),
        output = tokio::task::spawn_blocking({
            let (url, a) = (server.url.clone(), a.clone());
            move || run_blocking(&a, &url)
        }) => output.expect("aster thread"),
    };
    let stderr_a = String::from_utf8_lossy(&first.stderr).to_string();
    assert_eq!(runs(&a), 1, "A must run the target: {stderr_a}");
    assert_eq!(
        summary_line(&first),
        "remote: 0 hits / 1 misses / 1 uploads",
        "{stderr_a}"
    );
    let objects = server.app.cache_store.objects().await;
    assert_eq!(
        objects.len(),
        1,
        "{FILES} files and a binary are one chunk in the bucket, not {FILES} blobs: {objects:?}"
    );
    assert!(
        io_line(&first).contains("upload 1 blobs"),
        "{}",
        io_line(&first)
    );
    let built = listing(&a.join("app/out"));
    assert!(
        built.len() > FILES,
        "the fixture built its tree: {}",
        built.len()
    );

    // Phase 2, lookup and download: checkout B has an empty local cache and
    // asks the server. The reply names one blob; B downloads it once.
    let restored = tokio::select! {
        _ = &mut server.accept => unreachable!("accept ended"),
        output = tokio::task::spawn_blocking({
            let (url, b) = (server.url.clone(), b.clone());
            move || run_target(&b, &url, "//app:build", &["--no-deps", "--json"])
        }) => output.expect("aster thread"),
    };
    let stderr_b = String::from_utf8_lossy(&restored.stderr).to_string();
    assert_eq!(runs(&b), 0, "B must not run the target: {stderr_b}");
    assert_eq!(
        summary_line(&restored),
        "remote: 1 hits / 0 misses / 0 uploads",
        "{stderr_b}"
    );
    let io = io_line(&restored);
    assert!(io.contains("lookup 1 requests / 1 keys / 1 blobs"), "{io}");
    assert!(io.contains("download 1 blobs"), "{io}");
    assert!(io.contains("unpack 1 targets"), "{io}");
    // The same counters are in the machine-readable run summary.
    let json: serde_json::Value = serde_json::from_slice(&restored.stdout).expect("run json");
    let remote = &json["summary"]["remote_cache"];
    assert_eq!(remote["hits"], 1, "{json}");
    assert_eq!(remote["io"]["lookup_blobs"], 1, "{json}");
    assert_eq!(remote["io"]["download_blobs"], 1, "{json}");
    assert_eq!(remote["io"]["lookup_budget_misses"], 0, "{json}");

    // Phase 3, observable result: the tree on B is A's tree, file for file,
    // with modes, the symlink and the empty directory.
    assert_eq!(
        listing(&b.join("app/out")),
        built,
        "B must restore an identical tree: {stderr_b}"
    );

    // Phase 4: B's next run is a local hit and downloads nothing.
    let local = tokio::select! {
        _ = &mut server.accept => unreachable!("accept ended"),
        output = tokio::task::spawn_blocking({
            let (url, b) = (server.url.clone(), b.clone());
            move || run_blocking(&b, &url)
        }) => output.expect("aster thread"),
    };
    assert_eq!(runs(&b), 0);
    assert!(
        !io_line(&local).contains("download"),
        "a local hit downloaded again: {}",
        io_line(&local)
    );

    server.stop().await;
}

/// A chunk whose bytes on the server are not the bytes its digest names is a
/// miss. The target runs and its own output stands: no half-restored tree.
#[tokio::test]
async fn a_corrupt_chunk_on_the_server_is_a_miss_and_the_target_runs() {
    let Some(mut server) = start().await else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let a = root.path().join("workspace-a");
    let c = root.path().join("other-machine").join("workspace-c");
    many_file_checkout(&a);
    many_file_checkout(&c);

    let first = tokio::select! {
        _ = &mut server.accept => unreachable!("accept ended"),
        output = tokio::task::spawn_blocking({
            let (url, a) = (server.url.clone(), a.clone());
            move || run_blocking(&a, &url)
        }) => output.expect("aster thread"),
    };
    assert_eq!(runs(&a), 1, "{}", String::from_utf8_lossy(&first.stderr));

    // The bucket now holds other bytes under the chunk's name.
    let objects = server.app.cache_store.objects().await;
    assert_eq!(objects.len(), 1);
    server
        .app
        .cache_store
        .put_unrecorded(&objects[0], bytes::Bytes::from_static(b"bit rot"))
        .await
        .unwrap();

    let second = tokio::select! {
        _ = &mut server.accept => unreachable!("accept ended"),
        output = tokio::task::spawn_blocking({
            let (url, c) = (server.url.clone(), c.clone());
            move || run_blocking(&c, &url)
        }) => output.expect("aster thread"),
    };
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert_eq!(runs(&c), 1, "a corrupt chunk must not be a hit: {stderr}");
    assert!(stderr.contains("digest mismatch"), "{stderr}");
    assert_eq!(
        listing(&c.join("app/out")).len(),
        listing(&a.join("app/out")).len(),
        "the target's own build is the tree: {stderr}"
    );

    server.stop().await;
}

/// 40 targets are looked up in one batch. The server resolves blobs 16 at a
/// time inside a budget shorter than that takes, so it answers some keys as
/// unresolved. Before, those keys were read as misses and the targets reran.
/// Now the client asks again and every target is a hit.
///
/// Real: the aster binary, aster-srv, Postgres and its lookup budget logic.
/// Faked: GCS, with 150 ms added to every HEAD so the budget is reachable.
#[tokio::test]
async fn a_batch_cut_short_by_the_server_budget_is_asked_again_and_every_target_hits() {
    const TARGETS: usize = 40;
    let signer = std::sync::Arc::new(FixedSigner {
        email: "cache-e2e@example.iam.gserviceaccount.com".into(),
        signature: b"e2e-signature".to_vec(),
        signed: Default::default(),
    });
    let store = BlobStore::memory_with(
        "aster-cache-e2e",
        signer,
        FanoutConfig {
            lookup_budget: std::time::Duration::from_millis(300),
            ..FanoutConfig::default()
        },
        std::time::Duration::from_millis(150),
    );
    let Some(mut server) = start_with(Some(store)).await else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let write_checkout = |dir: &Path| {
        fs::create_dir_all(dir.join("app")).unwrap();
        fs::write(dir.join("app/package.json"), r#"{"name":"app"}"#).unwrap();
        let mut config = String::new();
        let mut names = Vec::new();
        for index in 0..TARGETS {
            config.push_str(&format!(
                "[targets.t{index}]\ncommand = \"bash -c 'mkdir -p out/t{index} && echo {index} > out/t{index}/f'\"\n\
                 cache = {{ enabled = true, outputs = [\"out/t{index}/**\"], exclude = [\"out/**\"] }}\n\n"
            ));
            names.push(format!("\"//app:t{index}\""));
        }
        config.push_str(&format!(
            "[targets.all]\ncommand = \"true\"\ndepends_on = [{}]\n",
            names.join(", ")
        ));
        fs::write(dir.join("app/aster.toml"), config).unwrap();
        git_init(dir);
    };
    let a = root.path().join("workspace-a");
    let b = root.path().join("other-machine").join("workspace-b");
    write_checkout(&a);
    write_checkout(&b);

    // A runs and stores all 40 targets; B restores them.
    let first = tokio::select! {
        _ = &mut server.accept => unreachable!("accept ended"),
        output = tokio::task::spawn_blocking({
            let (url, a) = (server.url.clone(), a.clone());
            move || run_target(&a, &url, "//app:all", &[])
        }) => output.expect("aster thread"),
    };
    assert!(
        String::from_utf8_lossy(&first.stderr).contains("remote:"),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let restored = tokio::select! {
        _ = &mut server.accept => unreachable!("accept ended"),
        output = tokio::task::spawn_blocking({
            let (url, b) = (server.url.clone(), b.clone());
            move || run_target(&b, &url, "//app:all", &[])
        }) => output.expect("aster thread"),
    };
    let stderr = String::from_utf8_lossy(&restored.stderr);
    for index in 0..TARGETS {
        assert!(
            b.join(format!("app/out/t{index}/f")).is_file(),
            "t{index} was not restored: {stderr}"
        );
    }
    let io = io_line(&restored);
    assert!(
        io.contains("unresolved by the server"),
        "the budget never cut the lookup short, so this proves nothing: {io}\nA: {}\nB: {stderr}",
        String::from_utf8_lossy(&first.stderr)
    );
    eprintln!("budget-bound restore of {TARGETS} targets: {io}");
    assert!(!io.contains("missed on the server's time budget"), "{io}");
    let summary = stderr
        .lines()
        .find(|line| line.starts_with("remote: "))
        .unwrap_or_default();
    assert!(
        summary.starts_with(&format!("remote: {TARGETS} hits / 0 misses")),
        "every target must hit: {summary}\n{io}"
    );

    server.stop().await;
}

/// What one client build costs on the seven-hundred-file target.
#[derive(Debug)]
#[allow(dead_code)] // read through Debug
struct BenchRow {
    lookup_blobs: usize,
    lookup: std::time::Duration,
    upload: std::time::Duration,
    restore: std::time::Duration,
}

/// Run the many-file build with `binary` against a fresh server whose GCS
/// answers each HEAD in 20 ms, as production's does, and time each leg.
async fn bench_binary(binary: &Path) -> Option<BenchRow> {
    let signer = std::sync::Arc::new(FixedSigner {
        email: "cache-e2e@example.iam.gserviceaccount.com".into(),
        signature: b"e2e-signature".to_vec(),
        signed: Default::default(),
    });
    let store = BlobStore::memory_with(
        "aster-cache-e2e",
        signer,
        FanoutConfig::default(),
        std::time::Duration::from_millis(20),
    );
    let mut server = start_with(Some(store)).await?;
    let root = tempfile::tempdir().unwrap();
    let a = root.path().join("workspace-a");
    let b = root.path().join("other-machine").join("workspace-b");
    many_file_checkout(&a);
    many_file_checkout(&b);
    let run = |binary: PathBuf, dir: PathBuf, url: String, extra: Vec<&'static str>| {
        tokio::task::spawn_blocking(move || {
            let started = std::time::Instant::now();
            let output = Command::new(binary)
                .current_dir(&dir)
                .env("ASTER_SRV_URL", &url)
                .env("ASTER_SRV_TOKEN", TOKEN)
                .env("ASTER_SRV_GCS_BASE", &url)
                .env("GITHUB_REPOSITORY", REPO)
                .env_remove("ASTER_REMOTE_CACHE")
                .env_remove("ASTER_CACHE_MODE")
                .env_remove("ASTER_CACHE_DIR")
                .args(["run", "//app:build", "--no-deps"])
                .args(extra)
                .output()
                .expect("spawn aster");
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            started.elapsed()
        })
    };
    // The build with no cache at all is what a cache leg is measured against.
    let plain = tokio::select! {
        _ = &mut server.accept => unreachable!("accept ended"),
        took = run(binary.to_path_buf(), a.clone(), server.url.clone(), vec!["--no-cache"]) => took.unwrap(),
    };
    fs::remove_dir_all(a.join("app/out")).unwrap();
    let stored = tokio::select! {
        _ = &mut server.accept => unreachable!("accept ended"),
        took = run(binary.to_path_buf(), a.clone(), server.url.clone(), vec![]) => took.unwrap(),
    };
    // One lookup of the stored key, straight at the server.
    let key: String = server
        .app
        .pool
        .get()
        .await
        .unwrap()
        .query_one("SELECT key FROM cache.manifests", &[])
        .await
        .unwrap()
        .get(0);
    let (url, key_c) = (server.url.clone(), key.clone());
    let lookup = tokio::select! {
        _ = &mut server.accept => unreachable!("accept ended"),
        done = tokio::task::spawn_blocking(move || {
            let started = std::time::Instant::now();
            let reply: serde_json::Value = ureq::post(&format!("{url}/v1/cache/lookup"))
                .header("Authorization", &format!("Bearer {TOKEN}"))
                .send_json(serde_json::json!({"repo": REPO, "keys": [key_c]}))
                .unwrap()
                .into_body()
                .read_json()
                .unwrap();
            let blobs = reply["hits"][&key_c]["blobs"].as_object().map_or(0, |b| b.len());
            (blobs, started.elapsed())
        }) => done.unwrap(),
    };
    let restored = tokio::select! {
        _ = &mut server.accept => unreachable!("accept ended"),
        took = run(binary.to_path_buf(), b.clone(), server.url.clone(), vec![]) => took.unwrap(),
    };
    assert_eq!(runs(&b), 0, "B must restore, not run");
    server.stop().await;
    Some(BenchRow {
        lookup_blobs: lookup.0,
        lookup: lookup.1,
        upload: stored.saturating_sub(plain),
        restore: restored,
    })
}

/// Compare this build with another binary on the seven-hundred-file target:
/// blobs a lookup names, lookup time, upload time on top of the build, and the
/// time a fresh checkout takes to restore it.
///
/// Run by hand, in release, with `ASTER_BENCH_BEFORE` naming the binary to
/// compare against:
/// `ASTER_BENCH_BEFORE=/path/to/aster cargo test --release --test
/// cache_remote_e2e bench_ -- --ignored --nocapture`
#[tokio::test]
#[ignore = "a benchmark; run by hand with ASTER_BENCH_BEFORE set"]
async fn bench_many_file_target_against_another_binary() {
    let Some(before) = std::env::var_os("ASTER_BENCH_BEFORE") else {
        eprintln!("skipping: ASTER_BENCH_BEFORE is not set");
        return;
    };
    for round in 1..=3 {
        for (label, binary) in [
            ("before", PathBuf::from(before.clone())),
            ("after", PathBuf::from(env!("CARGO_BIN_EXE_aster"))),
        ] {
            let Some(row) = bench_binary(&binary).await else {
                return;
            };
            eprintln!("bench round {round} {label}: {row:?}");
        }
    }
}
