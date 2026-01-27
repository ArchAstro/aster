# Clean Target and Cache Invalidation Design

## Overview

Add clean target support to language plugins and a mechanism for targets to invalidate project cache entries when run.

## Motivation

Users need a way to:
1. Clean build artifacts for their projects (node_modules, dist, _build, etc.)
2. Reset the cache state for a project after cleaning

## Design

### Target Schema Changes

Add `invalidates_cache` field to `Target` and `RichTargetConfig`:

```rust
// src/plugins/mod.rs
pub struct Target {
    pub command: String,
    pub depends_on: Vec<String>,
    pub capabilities: HashSet<TargetCapability>,
    pub files_glob: Option<String>,
    pub stream: bool,
    pub cache: Option<CacheConfig>,
    pub invalidates_cache: bool,  // NEW
}
```

```rust
// src/config/project.rs
pub struct RichTargetConfig {
    // ... existing fields ...
    #[serde(default)]
    pub invalidates_cache: bool,  // NEW
}
```

### aster.toml Usage

Users can define or override clean targets with cache invalidation:

```toml
[targets.clean]
command = "rm -rf node_modules .next dist"
invalidates_cache = true

[targets.reset-db]
command = "npm run db:reset"
invalidates_cache = true
```

### Plugin Interface Changes

Add `clean_target()` method to `LanguagePlugin` trait:

```rust
pub trait LanguagePlugin: Send + Sync {
    // ... existing methods ...

    /// Provide the default clean target for this language
    ///
    /// Returns None if the language doesn't have a standard clean operation.
    /// The returned target should have `invalidates_cache: true`.
    fn clean_target(&self, ctx: &TargetContext) -> Option<Target> {
        None
    }
}
```

### Plugin Clean Commands

| Plugin  | Clean Command                                              |
|---------|------------------------------------------------------------|
| Node.js | `rm -rf node_modules` + detect `.next`, `dist`, `.turbo`   |
| Elixir  | `mix clean`                                                |
| Rust    | `cargo clean`                                              |
| Go      | `go clean`                                                 |
| Python  | `rm -rf .venv __pycache__ .pytest_cache dist *.egg-info`   |

Node.js plugin detects framework-specific directories at runtime:
- `.next` (Next.js)
- `dist` (common build output)
- `.turbo` (Turborepo)

### Cache Invalidation Logic

When executor runs a target with `invalidates_cache = true`:

1. Execute the command
2. If successful (exit code 0), delete all cache entries for that project
3. Continue with remaining execution

```rust
// src/executor/runner.rs
async fn run_target(...) -> Result<...> {
    // ... execute the command ...

    if target.invalidates_cache && result.success {
        cache_store.invalidate_project(&project_address)?;
    }
}
```

New method on `CacheStore`:

```rust
// src/cache/store.rs
impl CacheStore {
    /// Remove all cache entries for a project
    pub fn invalidate_project(&self, project_address: &str) -> Result<()> {
        // Delete entries matching: {project_address}:*
    }
}
```

### Integration with Target Detection

Each plugin's `detect_targets()` includes the clean target:

```rust
fn detect_targets(&self, ctx: &TargetContext) -> Result<HashMap<String, Target>> {
    let mut targets = HashMap::new();

    // ... existing target detection ...

    if let Some(clean) = self.clean_target(ctx) {
        targets.insert("clean".to_string(), clean);
    }

    Ok(targets)
}
```

User-defined `[targets.clean]` in aster.toml takes precedence over plugin defaults.

## Behaviors

- Cache invalidation only happens on **successful** execution
- Invalidation happens **after** the command completes
- Only affects the project containing the target (not dependents)
- Any target can use `invalidates_cache`, not just `clean`
