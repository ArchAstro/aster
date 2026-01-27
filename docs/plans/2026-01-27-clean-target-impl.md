# Clean Target Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add clean target support to language plugins with cache invalidation capability.

**Architecture:** Add `invalidates_cache` field to Target struct, add `clean_target()` method to LanguagePlugin trait, implement in each plugin, and integrate cache invalidation into the executor's run loop.

**Tech Stack:** Rust, existing aster codebase

---

## Task 1: Add `invalidates_cache` Field to Target Struct

**Files:**
- Modify: `src/plugins/mod.rs:49-66` (Target struct)

**Step 1: Add the field to Target struct**

In `src/plugins/mod.rs`, add `invalidates_cache` field to the `Target` struct:

```rust
/// A build target with its command and dependencies
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Target {
    /// The command to execute for this target
    /// May contain {files} placeholder for file injection
    pub command: String,
    /// Target addresses that must run before this one (e.g., "//libs/shared:build", "//self:deps")
    /// Use "//self:target" to reference targets in the same project
    pub depends_on: Vec<String>,
    /// Capabilities this target supports
    pub capabilities: HashSet<TargetCapability>,
    /// Optional glob pattern to filter files for FilesList capability
    /// e.g., "*_test.go" or "*.spec.ts"
    pub files_glob: Option<String>,
    /// Stream output to stdout in real-time (for long-running processes like dev servers)
    pub stream: bool,
    /// Cache configuration overrides from aster.toml
    pub cache: Option<crate::config::CacheConfig>,
    /// When true, invalidates all cache entries for this project after successful execution
    pub invalidates_cache: bool,
}
```

**Step 2: Run `cargo check` to find all places that need updating**

Run: `cargo check 2>&1 | head -50`
Expected: Compilation errors showing struct initialization sites that need the new field

**Step 3: Fix compilation errors in plugin files**

Add `invalidates_cache: false` to every `Target` instantiation in:
- `src/plugins/nodejs.rs`
- `src/plugins/elixir.rs`
- `src/plugins/rust.rs`
- `src/plugins/go.rs`
- `src/plugins/python.rs`

Example fix pattern:
```rust
Target {
    command: "npm install".to_string(),
    depends_on: vec![],
    capabilities: HashSet::new(),
    files_glob: None,
    stream: false,
    cache: None,
    invalidates_cache: false,  // ADD THIS
}
```

**Step 4: Fix compilation errors in resolver**

In `src/targets/resolver.rs`, add `invalidates_cache` to the Target construction:

```rust
// In resolve() method, around line 42-52
targets.insert(
    name.clone(),
    Target {
        command: target.command.clone(),
        depends_on: resolved_deps,
        capabilities: target.capabilities.clone(),
        files_glob: target.files_glob.clone(),
        stream: target.stream,
        cache: target.cache.clone(),
        invalidates_cache: target.invalidates_cache,  // ADD THIS
    },
);
```

Also update all other Target constructions in resolver.rs (simple format, rich format).

**Step 5: Verify compilation passes**

Run: `cargo check`
Expected: No errors

**Step 6: Commit**

```bash
git add src/plugins/mod.rs src/plugins/*.rs src/targets/resolver.rs
git commit -m "$(cat <<'EOF'
feat: add invalidates_cache field to Target struct

Adds a boolean field that, when true, will invalidate all cache entries
for a project after the target runs successfully.

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Add `invalidates_cache` to Config Schema

**Files:**
- Modify: `src/config/project.rs:63-87` (RichTargetConfig struct)
- Modify: `src/config/project.rs:89-136` (TargetConfig impl)

**Step 1: Add field to RichTargetConfig**

In `src/config/project.rs`, add to `RichTargetConfig`:

```rust
/// Rich target configuration with all options
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RichTargetConfig {
    /// The command to execute (may contain {files} placeholder)
    pub command: String,

    /// Target dependencies: ["//self:deps", "//libs/shared:build"]
    #[serde(default)]
    pub depends_on: Vec<String>,

    /// Capabilities: ["files_list"]
    #[serde(default)]
    pub capabilities: Vec<String>,

    /// Glob pattern to filter files for files_list capability
    /// e.g., "*_test.py" or "*.spec.ts"
    pub files_glob: Option<String>,

    /// Stream output to stdout in real-time (for long-running processes like dev servers)
    #[serde(default)]
    pub stream: bool,

    /// Cache configuration overrides
    #[serde(default)]
    pub cache: Option<CacheConfig>,

    /// Invalidate all project cache entries after successful execution
    #[serde(default)]
    pub invalidates_cache: bool,
}
```

**Step 2: Add accessor method to TargetConfig**

Add to the `impl TargetConfig` block:

```rust
/// Get invalidates_cache flag (false for simple format)
pub fn invalidates_cache(&self) -> bool {
    match self {
        TargetConfig::Simple(_) => false,
        TargetConfig::Rich(rich) => rich.invalidates_cache,
    }
}
```

**Step 3: Verify compilation passes**

Run: `cargo check`
Expected: No errors

**Step 4: Add test for parsing invalidates_cache**

In `src/config/project.rs` tests, add:

```rust
#[test]
fn test_parse_target_with_invalidates_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let toml_path = tmp.path().join("aster.toml");
    std::fs::write(
        &toml_path,
        r#"
[targets.clean]
command = "rm -rf node_modules"
invalidates_cache = true

[targets.build]
command = "npm run build"
"#,
    )
    .unwrap();

    let config = parse_aster_toml(&toml_path).unwrap();

    let clean = config.targets.get("clean").unwrap();
    assert_eq!(clean.command(), "rm -rf node_modules");
    assert!(clean.invalidates_cache());

    let build = config.targets.get("build").unwrap();
    assert_eq!(build.command(), "npm run build");
    assert!(!build.invalidates_cache());
}
```

**Step 5: Run tests**

Run: `cargo test test_parse_target_with_invalidates_cache`
Expected: PASS

**Step 6: Commit**

```bash
git add src/config/project.rs
git commit -m "$(cat <<'EOF'
feat: add invalidates_cache to aster.toml schema

Allows targets to specify `invalidates_cache = true` in aster.toml
to clear project cache after successful execution.

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Update Target Resolver to Handle invalidates_cache

**Files:**
- Modify: `src/targets/resolver.rs`

**Step 1: Update rich format handling**

In `src/targets/resolver.rs`, in the `TargetConfig::Rich` match arm, add handling for `invalidates_cache`:

```rust
TargetConfig::Rich(rich) => {
    // ... existing code for depends_on, capabilities, files_glob, stream, cache ...

    // invalidates_cache from rich config (no fallback - explicit only)
    let invalidates_cache = rich.invalidates_cache;

    targets.insert(
        name.clone(),
        Target {
            command: rich.command.clone(),
            depends_on,
            capabilities,
            files_glob,
            stream,
            cache,
            invalidates_cache,
        },
    );
}
```

**Step 2: Update simple format handling**

For simple format overriding existing target, preserve `invalidates_cache`:

```rust
TargetConfig::Simple(command) => {
    if let Some(existing) = existing {
        targets.insert(
            name.clone(),
            Target {
                command: command.clone(),
                depends_on: existing.depends_on.clone(),
                capabilities: existing.capabilities.clone(),
                files_glob: existing.files_glob.clone(),
                stream: existing.stream,
                cache: existing.cache.clone(),
                invalidates_cache: existing.invalidates_cache,  // PRESERVE
            },
        );
    } else {
        targets.insert(
            name.clone(),
            Target {
                command: command.clone(),
                depends_on: vec![],
                capabilities: HashSet::new(),
                files_glob: None,
                stream: false,
                cache: None,
                invalidates_cache: false,  // DEFAULT
            },
        );
    }
}
```

**Step 3: Update test helper**

Update the `target()` helper function in tests:

```rust
fn target(command: &str, depends_on: Vec<&str>) -> Target {
    Target {
        command: command.to_string(),
        depends_on: depends_on.into_iter().map(|s| s.to_string()).collect(),
        capabilities: HashSet::new(),
        files_glob: None,
        stream: false,
        cache: None,
        invalidates_cache: false,
    }
}
```

And `rich()` helper:

```rust
fn rich(
    command: &str,
    depends_on: Vec<&str>,
    capabilities: Vec<&str>,
    files_glob: Option<&str>,
) -> TargetConfig {
    TargetConfig::Rich(RichTargetConfig {
        command: command.to_string(),
        depends_on: depends_on.into_iter().map(|s| s.to_string()).collect(),
        capabilities: capabilities.into_iter().map(|s| s.to_string()).collect(),
        files_glob: files_glob.map(|s| s.to_string()),
        stream: false,
        cache: None,
        invalidates_cache: false,
    })
}
```

**Step 4: Verify all tests pass**

Run: `cargo test --lib`
Expected: All tests pass

**Step 5: Commit**

```bash
git add src/targets/resolver.rs
git commit -m "$(cat <<'EOF'
feat: handle invalidates_cache in target resolver

Rich format targets can specify invalidates_cache explicitly.
Simple format preserves existing value or defaults to false.

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Implement Cache Invalidation in Executor

**Files:**
- Modify: `src/executor/runner.rs`

**Step 1: Find where cache is updated after successful execution**

The cache update happens around line 441-487 in the thread spawn block. We need to add cache invalidation logic after successful execution.

**Step 2: Pass invalidates_cache flag to thread**

Add to the variables cloned for the thread (around line 422-436):

```rust
let invalidates_cache = project
    .targets
    .get(&target_name)
    .map(|t| t.invalidates_cache)
    .unwrap_or(false);
```

**Step 3: Add cache invalidation after successful execution**

In the thread's execution block, after the existing cache update logic (around line 441-487), add:

```rust
// Invalidate project cache if target has invalidates_cache flag
if result.success && invalidates_cache {
    if let Some(ref workspace_root) = cache_store_path {
        let store = CacheStore::new(workspace_root);
        // Extract project address from target address (//project:target -> //project)
        let project_addr = addr.rsplit_once(':')
            .map(|(proj, _)| proj)
            .unwrap_or(&addr);
        if let Err(e) = store.clear_matching(project_addr) {
            eprintln!(
                "[aster] Warning: Failed to invalidate cache for {project_addr}: {e}"
            );
        }
    }
}
```

**Step 4: Verify compilation passes**

Run: `cargo check`
Expected: No errors

**Step 5: Commit**

```bash
git add src/executor/runner.rs
git commit -m "$(cat <<'EOF'
feat: invalidate project cache when target has invalidates_cache

After successful execution of a target with invalidates_cache=true,
all cache entries for that project are cleared using clear_matching.

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Add clean_target Method to Plugin Trait

**Files:**
- Modify: `src/plugins/mod.rs:86-167` (LanguagePlugin trait)

**Step 1: Add the method to the trait**

Add after `cache_inputs()` method (around line 166):

```rust
/// Provide the default clean target for this language
///
/// Returns None if the language doesn't have a standard clean operation.
/// The returned target should have `invalidates_cache: true` set.
///
/// - ctx: Target context with project paths and dependencies
fn clean_target(&self, _ctx: &TargetContext) -> Option<Target> {
    None
}
```

**Step 2: Verify compilation passes**

Run: `cargo check`
Expected: No errors (default implementation means no changes needed in plugins yet)

**Step 3: Commit**

```bash
git add src/plugins/mod.rs
git commit -m "$(cat <<'EOF'
feat: add clean_target method to LanguagePlugin trait

Plugins can optionally implement this to provide language-specific
clean targets. Default implementation returns None.

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Implement clean_target for Elixir Plugin

**Files:**
- Modify: `src/plugins/elixir.rs`

**Step 1: Add clean_target implementation**

Add to the `impl LanguagePlugin for ElixirPlugin` block, after `cache_inputs()`:

```rust
fn clean_target(&self, _ctx: &TargetContext) -> Option<Target> {
    Some(Target {
        command: "mix clean".to_string(),
        depends_on: vec![],
        capabilities: HashSet::new(),
        files_glob: None,
        stream: false,
        cache: None,
        invalidates_cache: true,
    })
}
```

**Step 2: Update detect_targets to include clean**

At the end of `detect_targets()`, before `Ok(targets)`:

```rust
// Add clean target
if let Some(clean) = self.clean_target(ctx) {
    targets.insert("clean".to_string(), clean);
}
```

**Step 3: Add test for clean target**

```rust
#[test]
fn test_detect_targets_has_clean() {
    let tmp = tempfile::tempdir().unwrap();
    let mix_exs = tmp.path().join("mix.exs");
    std::fs::write(
        &mix_exs,
        r#"
defmodule MyApp.MixProject do
  use Mix.Project
  def project do
    [app: :my_app]
  end
end
"#,
    )
    .unwrap();

    let plugin = ElixirPlugin;
    let ctx = make_context(&mix_exs, tmp.path(), &[]);
    let targets = plugin.detect_targets(&ctx).unwrap();

    let clean = targets.get("clean").unwrap();
    assert_eq!(clean.command, "mix clean");
    assert!(clean.depends_on.is_empty());
    assert!(clean.invalidates_cache);
}
```

**Step 4: Run tests**

Run: `cargo test elixir`
Expected: All tests pass

**Step 5: Commit**

```bash
git add src/plugins/elixir.rs
git commit -m "$(cat <<'EOF'
feat(elixir): add clean target with mix clean

Elixir projects now have a clean target that runs mix clean
and invalidates the project cache.

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Implement clean_target for Node.js Plugin

**Files:**
- Modify: `src/plugins/nodejs.rs`

**Step 1: Add clean_target implementation with detection**

Add to the `impl LanguagePlugin for NodeJsPlugin` block:

```rust
fn clean_target(&self, ctx: &TargetContext) -> Option<Target> {
    let mut dirs_to_clean = vec!["node_modules"];

    // Detect framework-specific directories
    if ctx.project_dir.join(".next").exists() {
        dirs_to_clean.push(".next");
    }
    if ctx.project_dir.join("dist").exists() {
        dirs_to_clean.push("dist");
    }
    if ctx.project_dir.join(".turbo").exists() {
        dirs_to_clean.push(".turbo");
    }
    if ctx.project_dir.join("build").exists() {
        dirs_to_clean.push("build");
    }

    let command = format!("rm -rf {}", dirs_to_clean.join(" "));

    Some(Target {
        command,
        depends_on: vec![],
        capabilities: HashSet::new(),
        files_glob: None,
        stream: false,
        cache: None,
        invalidates_cache: true,
    })
}
```

**Step 2: Update detect_targets to include clean**

At the end of `detect_targets()`, before `Ok(targets)`:

```rust
// Add clean target
if let Some(clean) = self.clean_target(ctx) {
    targets.insert("clean".to_string(), clean);
}
```

**Step 3: Add test for clean target**

```rust
#[test]
fn test_detect_targets_has_clean() {
    let tmp = tempfile::tempdir().unwrap();
    let pkg_json = tmp.path().join("package.json");
    std::fs::write(&pkg_json, r#"{"name": "my-app"}"#).unwrap();

    let plugin = NodeJsPlugin;
    let ctx = make_context(&pkg_json, tmp.path(), &[]);
    let targets = plugin.detect_targets(&ctx).unwrap();

    let clean = targets.get("clean").unwrap();
    assert_eq!(clean.command, "rm -rf node_modules");
    assert!(clean.depends_on.is_empty());
    assert!(clean.invalidates_cache);
}

#[test]
fn test_clean_target_detects_next() {
    let tmp = tempfile::tempdir().unwrap();
    let pkg_json = tmp.path().join("package.json");
    std::fs::write(&pkg_json, r#"{"name": "my-app"}"#).unwrap();

    // Create .next directory
    std::fs::create_dir(tmp.path().join(".next")).unwrap();

    let plugin = NodeJsPlugin;
    let ctx = make_context(&pkg_json, tmp.path(), &[]);
    let targets = plugin.detect_targets(&ctx).unwrap();

    let clean = targets.get("clean").unwrap();
    assert!(clean.command.contains("node_modules"));
    assert!(clean.command.contains(".next"));
}
```

**Step 4: Run tests**

Run: `cargo test nodejs`
Expected: All tests pass

**Step 5: Commit**

```bash
git add src/plugins/nodejs.rs
git commit -m "$(cat <<'EOF'
feat(nodejs): add clean target with framework detection

Node.js projects now have a clean target that removes node_modules
and detects framework-specific directories (.next, dist, .turbo, build).

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Implement clean_target for Rust Plugin

**Files:**
- Modify: `src/plugins/rust.rs`

**Step 1: Add clean_target implementation**

```rust
fn clean_target(&self, _ctx: &TargetContext) -> Option<Target> {
    Some(Target {
        command: "cargo clean".to_string(),
        depends_on: vec![],
        capabilities: HashSet::new(),
        files_glob: None,
        stream: false,
        cache: None,
        invalidates_cache: true,
    })
}
```

**Step 2: Update detect_targets to include clean**

At the end of `detect_targets()`, before `Ok(targets)`:

```rust
// Add clean target
if let Some(clean) = self.clean_target(ctx) {
    targets.insert("clean".to_string(), clean);
}
```

**Step 3: Add test**

```rust
#[test]
fn test_detect_targets_has_clean() {
    let tmp = tempfile::tempdir().unwrap();
    let cargo_toml = tmp.path().join("Cargo.toml");
    std::fs::write(
        &cargo_toml,
        r#"
[package]
name = "my-crate"
version = "0.1.0"
"#,
    )
    .unwrap();

    let plugin = RustPlugin;
    let ctx = make_context(&cargo_toml, tmp.path(), &[]);
    let targets = plugin.detect_targets(&ctx).unwrap();

    let clean = targets.get("clean").unwrap();
    assert_eq!(clean.command, "cargo clean");
    assert!(clean.invalidates_cache);
}
```

**Step 4: Run tests**

Run: `cargo test rust`
Expected: All tests pass

**Step 5: Commit**

```bash
git add src/plugins/rust.rs
git commit -m "$(cat <<'EOF'
feat(rust): add clean target with cargo clean

Rust projects now have a clean target that runs cargo clean
and invalidates the project cache.

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Implement clean_target for Go Plugin

**Files:**
- Modify: `src/plugins/go.rs`

**Step 1: Add clean_target implementation**

```rust
fn clean_target(&self, _ctx: &TargetContext) -> Option<Target> {
    Some(Target {
        command: "go clean".to_string(),
        depends_on: vec![],
        capabilities: HashSet::new(),
        files_glob: None,
        stream: false,
        cache: None,
        invalidates_cache: true,
    })
}
```

**Step 2: Update detect_targets to include clean**

At the end of `detect_targets()`, before `Ok(targets)`:

```rust
// Add clean target
if let Some(clean) = self.clean_target(ctx) {
    targets.insert("clean".to_string(), clean);
}
```

**Step 3: Add test**

```rust
#[test]
fn test_detect_targets_has_clean() {
    let tmp = tempfile::tempdir().unwrap();
    let go_mod = tmp.path().join("go.mod");
    std::fs::write(
        &go_mod,
        r#"module myapp

go 1.21
"#,
    )
    .unwrap();

    let plugin = GoPlugin;
    let ctx = make_context(&go_mod, tmp.path(), &[]);
    let targets = plugin.detect_targets(&ctx).unwrap();

    let clean = targets.get("clean").unwrap();
    assert_eq!(clean.command, "go clean");
    assert!(clean.invalidates_cache);
}
```

**Step 4: Run tests**

Run: `cargo test go`
Expected: All tests pass

**Step 5: Commit**

```bash
git add src/plugins/go.rs
git commit -m "$(cat <<'EOF'
feat(go): add clean target with go clean

Go projects now have a clean target that runs go clean
and invalidates the project cache.

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: Implement clean_target for Python Plugin

**Files:**
- Modify: `src/plugins/python.rs`

**Step 1: Add clean_target implementation**

```rust
fn clean_target(&self, ctx: &TargetContext) -> Option<Target> {
    let mut dirs_to_clean = vec!["__pycache__", ".pytest_cache", "*.egg-info", "dist", "build"];

    // Add .venv if it exists
    if ctx.project_dir.join(".venv").exists() {
        dirs_to_clean.insert(0, ".venv");
    }

    let command = format!("rm -rf {}", dirs_to_clean.join(" "));

    Some(Target {
        command,
        depends_on: vec![],
        capabilities: HashSet::new(),
        files_glob: None,
        stream: false,
        cache: None,
        invalidates_cache: true,
    })
}
```

**Step 2: Update detect_targets to include clean**

At the end of `detect_targets()`, before `Ok(targets)`:

```rust
// Add clean target
if let Some(clean) = self.clean_target(ctx) {
    targets.insert("clean".to_string(), clean);
}
```

**Step 3: Add test**

```rust
#[test]
fn test_detect_targets_has_clean() {
    let tmp = tempfile::tempdir().unwrap();
    let pyproject = tmp.path().join("pyproject.toml");
    std::fs::write(
        &pyproject,
        r#"
[project]
name = "mypackage"
"#,
    )
    .unwrap();

    let plugin = PythonPlugin;
    let ctx = make_context(&pyproject, tmp.path(), &[]);
    let targets = plugin.detect_targets(&ctx).unwrap();

    let clean = targets.get("clean").unwrap();
    assert!(clean.command.contains("__pycache__"));
    assert!(clean.command.contains(".pytest_cache"));
    assert!(clean.invalidates_cache);
}

#[test]
fn test_clean_target_detects_venv() {
    let tmp = tempfile::tempdir().unwrap();
    let pyproject = tmp.path().join("pyproject.toml");
    std::fs::write(
        &pyproject,
        r#"
[project]
name = "mypackage"
"#,
    )
    .unwrap();

    // Create .venv directory
    std::fs::create_dir(tmp.path().join(".venv")).unwrap();

    let plugin = PythonPlugin;
    let ctx = make_context(&pyproject, tmp.path(), &[]);
    let targets = plugin.detect_targets(&ctx).unwrap();

    let clean = targets.get("clean").unwrap();
    assert!(clean.command.contains(".venv"));
}
```

**Step 4: Run tests**

Run: `cargo test python`
Expected: All tests pass

**Step 5: Commit**

```bash
git add src/plugins/python.rs
git commit -m "$(cat <<'EOF'
feat(python): add clean target with venv detection

Python projects now have a clean target that removes common Python
build artifacts and optionally .venv if it exists.

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 11: Add Integration Test for Cache Invalidation

**Files:**
- Modify: `src/cache/store.rs` (add test)

**Step 1: Write integration test**

Add to `src/cache/store.rs` tests:

```rust
#[test]
fn test_invalidate_project_clears_all_project_targets() {
    let tmp = TempDir::new().unwrap();
    let store = CacheStore::new(tmp.path());

    // Set up cache entries for multiple projects
    let entry = CacheEntry {
        hash: "abc123".to_string(),
        timestamp: "2024-01-01T00:00:00Z".to_string(),
        success: true,
    };

    store.set("//apps/web:build", entry.clone()).unwrap();
    store.set("//apps/web:test", entry.clone()).unwrap();
    store.set("//apps/web:lint", entry.clone()).unwrap();
    store.set("//libs/shared:build", entry.clone()).unwrap();
    store.set("//libs/shared:test", entry).unwrap();

    // Invalidate //apps/web
    let removed = store.clear_matching("//apps/web").unwrap();
    assert_eq!(removed, 3);

    // Verify only //libs/shared entries remain
    let state = store.load().unwrap();
    assert_eq!(state.targets.len(), 2);
    assert!(state.targets.contains_key("//libs/shared:build"));
    assert!(state.targets.contains_key("//libs/shared:test"));
    assert!(!state.targets.contains_key("//apps/web:build"));
}
```

**Step 2: Run tests**

Run: `cargo test test_invalidate_project`
Expected: PASS

**Step 3: Commit**

```bash
git add src/cache/store.rs
git commit -m "$(cat <<'EOF'
test: add integration test for cache invalidation

Verifies that clear_matching properly invalidates all targets
for a specific project while leaving other projects' caches intact.

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 12: Run Full Test Suite and Verify

**Step 1: Run all tests**

Run: `cargo test`
Expected: All tests pass

**Step 2: Run clippy**

Run: `cargo clippy -- -D warnings`
Expected: No warnings

**Step 3: Build release**

Run: `cargo build --release`
Expected: Build succeeds

**Step 4: Manual verification**

Create a test project with:
```toml
# aster.toml
[targets.clean]
command = "echo 'cleaning...'"
invalidates_cache = true
```

Run `aster clean` and verify it executes and invalidates cache.

**Step 5: Final commit if needed**

If any fixes were required, commit them.
