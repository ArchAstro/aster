# Phase 2: Language Plugins - Research

**Researched:** 2026-01-22
**Domain:** Elixir mix.exs parsing, Python pyproject.toml parsing, target mapping
**Confidence:** HIGH

## Summary

Phase 2 extends Aster with Elixir and Python plugins, following the established `LanguagePlugin` trait from Phase 1. The core challenge is parsing native config formats to extract path dependencies without executing language runtimes. For Elixir, this means regex extraction from mix.exs (an Elixir script). For Python, this means TOML parsing of pyproject.toml with support for both Poetry and PEP 621 formats.

The key architectural decision from CONTEXT.md is to use regex for mix.exs parsing rather than invoking `mix` - this ensures portability but requires careful pattern matching. For Python, the existing `toml` crate in Cargo.toml can parse pyproject.toml directly, with serde structs for Poetry's `tool.poetry.dependencies` section.

Target mapping (test/build/lint to native commands) is straightforward: define default commands per language and allow aster.toml overrides. The architecture already supports this via the `targets` HashMap in `DiscoveredProject`.

**Primary recommendation:** Use the Rust `regex` crate for mix.exs parsing with named capture groups, extend TOML parsing with custom structs for Poetry/PEP 621, and implement target defaults in a new `TargetResolver` component.

## Standard Stack

The established libraries/tools for this domain:

### Core
| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| regex | 1.10.x | Elixir mix.exs pattern matching | Standard Rust regex, named captures, guaranteed O(n) |
| toml | 0.8.x | pyproject.toml parsing | Already in Cargo.toml, mature TOML parser |
| serde | 1.0.x | Struct (de)serialization | Already in Cargo.toml |

### Supporting
| Library | Version | Purpose | When to Use |
|---------|---------|---------|-------------|
| lazy_static or once_cell | latest | Compile regex once | Avoid recompiling regex on each call |

### Alternatives Considered
| Instead of | Could Use | Tradeoff |
|------------|-----------|----------|
| regex for mix.exs | tree-sitter-elixir | More accurate but heavyweight dependency |
| Custom Poetry structs | pyproject-toml crate | pyproject-toml adds dependency for minor benefit |
| Manual TOML parsing | Direct serde_json style | Already using toml crate, consistent approach |

**Installation:**
```toml
# Add to existing Cargo.toml
[dependencies]
regex = "1.10"
# toml, serde already present
```

## Architecture Patterns

### Recommended Project Structure
```
src/plugins/
├── mod.rs           # Plugin trait + registry (existing)
├── nodejs.rs        # Node.js plugin (existing)
├── elixir.rs        # NEW: Elixir mix.exs parser
├── python.rs        # NEW: Python pyproject.toml parser
└── targets.rs       # NEW: Target resolution logic
```

### Pattern 1: Regex with Named Captures for Elixir

**What:** Use Rust regex with named capture groups to extract dependencies from mix.exs without parsing Elixir AST.

**When to use:** All mix.exs dependency extraction.

**Example:**
```rust
// Source: Rust regex docs + Elixir mix.exs format
use regex::Regex;
use std::sync::LazyLock;

// Compile regex once at startup
static PATH_DEP_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\{:(?<name>\w+),\s*path:\s*"(?<path>[^"]+)"(?:,\s*in_umbrella:\s*(?<umbrella>true|false))?\}"#)
        .expect("Invalid regex")
});

static IN_UMBRELLA_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\{:(?<name>\w+),\s*in_umbrella:\s*true\}"#)
        .expect("Invalid regex")
});

static APP_NAME_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"app:\s*:(?<name>\w+)"#)
        .expect("Invalid regex")
});

pub fn parse_mix_dependencies(content: &str) -> Vec<LocalDependency> {
    let mut deps = Vec::new();

    // Match path: dependencies
    for caps in PATH_DEP_REGEX.captures_iter(content) {
        let name = caps.name("name").unwrap().as_str();
        let path = caps.name("path").unwrap().as_str();
        deps.push(LocalDependency {
            name: name.to_string(),
            path: PathBuf::from(path),
        });
    }

    // Match in_umbrella: true (implicit path ../name)
    for caps in IN_UMBRELLA_REGEX.captures_iter(content) {
        let name = caps.name("name").unwrap().as_str();
        deps.push(LocalDependency {
            name: name.to_string(),
            path: PathBuf::from(format!("../{}", name)),
        });
    }

    deps
}
```

### Pattern 2: Serde Structs for pyproject.toml

**What:** Define serde structs matching Poetry and PEP 621 formats, use toml crate for parsing.

**When to use:** All pyproject.toml parsing.

**Example:**
```rust
// Source: Poetry docs + PEP 621 spec
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Deserialize, Default)]
pub struct PyProjectToml {
    pub project: Option<Project>,
    pub tool: Option<Tool>,
}

#[derive(Deserialize, Default)]
pub struct Project {
    pub name: Option<String>,
    pub version: Option<String>,
    pub dependencies: Option<Vec<String>>,  // PEP 508 strings
}

#[derive(Deserialize, Default)]
pub struct Tool {
    pub poetry: Option<ToolPoetry>,
    pub hatch: Option<ToolHatch>,
}

#[derive(Deserialize, Default)]
pub struct ToolPoetry {
    pub name: Option<String>,
    pub version: Option<String>,
    pub dependencies: Option<HashMap<String, PoetryDependency>>,
    #[serde(rename = "dev-dependencies")]
    pub dev_dependencies: Option<HashMap<String, PoetryDependency>>,
}

// Poetry dependency can be string "^1.0" or table {path = "../lib"}
#[derive(Deserialize)]
#[serde(untagged)]
pub enum PoetryDependency {
    Version(String),
    Table(PoetryDepTable),
}

#[derive(Deserialize, Default)]
pub struct PoetryDepTable {
    pub path: Option<String>,
    pub develop: Option<bool>,
    pub version: Option<String>,
    pub git: Option<String>,
}
```

### Pattern 3: Target Resolution with Defaults

**What:** Resolve target names to commands with per-language defaults and aster.toml overrides.

**When to use:** When mapping standard targets (test, build, lint) to native commands.

**Example:**
```rust
// Source: CONTEXT.md target decisions
use std::collections::HashMap;

pub struct TargetDefaults {
    defaults: HashMap<(&'static str, &'static str), &'static str>,
}

impl TargetDefaults {
    pub fn new() -> Self {
        let mut defaults = HashMap::new();

        // Node.js
        defaults.insert(("nodejs", "test"), "npm test");
        defaults.insert(("nodejs", "build"), "npm run build");
        defaults.insert(("nodejs", "lint"), "npm run lint");

        // Elixir
        defaults.insert(("elixir", "test"), "mix test");
        defaults.insert(("elixir", "build"), "mix compile");
        defaults.insert(("elixir", "lint"), "mix credo");

        // Python
        defaults.insert(("python", "test"), "pytest");
        defaults.insert(("python", "build"), "python -m build");
        defaults.insert(("python", "lint"), "ruff check .");

        Self { defaults }
    }

    pub fn resolve(
        &self,
        plugin_name: &str,
        target: &str,
        overrides: &HashMap<String, String>,
    ) -> Option<String> {
        // Override takes precedence
        if let Some(cmd) = overrides.get(target) {
            return Some(cmd.clone());
        }

        // Fall back to default
        self.defaults
            .get(&(plugin_name, target))
            .map(|s| s.to_string())
    }
}
```

### Anti-Patterns to Avoid

- **Executing mix/python for parsing:** Decision from CONTEXT.md - regex/TOML only. No shelling out to runtimes.

- **Single monolithic regex for mix.exs:** Use multiple focused regexes. Elixir syntax has many variations.

- **Ignoring Poetry 2.0 format:** Poetry 2.0+ supports both `[project]` and `[tool.poetry]`. Check both.

- **Hardcoding target commands in multiple places:** Use a central `TargetDefaults` struct.

## Don't Hand-Roll

Problems that look simple but have existing solutions:

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| TOML parsing | String manipulation | `toml` crate | TOML has complex escaping, multiline strings, inline tables |
| Regex compilation | Re-compile each call | `LazyLock` or `lazy_static` | Regex compilation is expensive (microseconds to milliseconds) |
| PEP 508 parsing | Custom parser | Check for `@ file:` prefix | PEP 508 is complex but path deps use simple `@ file:` prefix |
| Elixir AST parsing | Full parser | Regex for deps only | We only need deps, not full AST |

**Key insight:** Parsing mix.exs as a full Elixir AST would require a parser generator or tree-sitter. For dependency extraction, targeted regexes are sufficient and much simpler.

## Common Pitfalls

### Pitfall 1: Mix.exs Multiline Dependency Declarations

**What goes wrong:** Regex fails when dependencies span multiple lines.

**Why it happens:** Elixir allows:
```elixir
{:dep_name,
  path: "../path",
  in_umbrella: true}
```

**How to avoid:** Use `(?s)` flag (dot matches newline) or normalize whitespace before matching.

**Warning signs:** Missing dependencies in umbrella projects.

### Pitfall 2: Poetry Dependency Format Variations

**What goes wrong:** Parser handles `{path = "..."}` but not `{version = "^1.0", path = "..."}`.

**Why it happens:** Poetry dependencies can have many optional fields.

**How to avoid:** Use `#[serde(untagged)]` enum that tries string first, then table. Accept any table with `path` field.

**Warning signs:** "Failed to parse" errors on valid pyproject.toml files.

### Pitfall 3: Umbrella App Discovery

**What goes wrong:** Elixir umbrella projects aren't discovered because `apps/*/mix.exs` structure isn't recognized.

**Why it happens:** Umbrella projects have root mix.exs AND apps/*/mix.exs.

**How to avoid:** The existing scanner already finds all mix.exs files. Just ensure in_umbrella: true resolves to correct path.

**Warning signs:** Umbrella apps missing from graph.

### Pitfall 4: PEP 621 vs Poetry Format Confusion

**What goes wrong:** Parser reads wrong section, gets wrong project name.

**Why it happens:** Poetry 2.0+ has BOTH `[project].name` and `[tool.poetry].name`.

**How to avoid:** Priority: `project.name` > `tool.poetry.name`. PEP 621 is the standard.

**Warning signs:** Projects named wrong in graph output.

### Pitfall 5: Missing Target Warning Instead of Error

**What goes wrong:** User runs `aster test` on Python project without pytest installed; silent failure.

**Why it happens:** CONTEXT.md says "warn and skip if missing" but implementation just silently skips.

**How to avoid:** Always print warning: "Skipping target 'test' for //path/to/project: no command configured"

**Warning signs:** User confusion about why project wasn't tested.

### Pitfall 6: Relative Path Resolution in Umbrella/Workspace

**What goes wrong:** `../sibling` dependency resolves incorrectly when umbrella apps are nested.

**Why it happens:** Path resolution must be relative to the config file's directory, not workspace root.

**How to avoid:** Phase 1 already handles this correctly in `resolve_dependency_address`. Follow same pattern.

**Warning signs:** "dependency not found" warnings for valid dependencies.

## Code Examples

Verified patterns from official sources:

### Elixir Plugin Implementation

```rust
// Source: Pattern from nodejs.rs + mix.exs format research
use regex::Regex;
use std::sync::LazyLock;

pub struct ElixirPlugin;

static DEPS_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    // Match {:name, path: "path"} and {:name, path: "path", in_umbrella: true}
    Regex::new(r#"\{:(\w+),\s*path:\s*"([^"]+)"(?:,\s*in_umbrella:\s*true)?\}"#).unwrap()
});

static UMBRELLA_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    // Match {:name, in_umbrella: true}
    Regex::new(r#"\{:(\w+),\s*in_umbrella:\s*true\}"#).unwrap()
});

static APP_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    // Match app: :name in project definition
    Regex::new(r#"app:\s*:(\w+)"#).unwrap()
});

impl LanguagePlugin for ElixirPlugin {
    fn name(&self) -> &str {
        "elixir"
    }

    fn marker_files(&self) -> &[&str] {
        &["mix.exs"]
    }

    fn parse_project(&self, _root: &Path, config_path: &Path) -> Result<ProjectMetadata> {
        let content = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;

        let name = APP_REGEX
            .captures(&content)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
            .ok_or_else(|| anyhow!("Could not find app name in {}", config_path.display()))?;

        Ok(ProjectMetadata {
            name,
            version: None, // Could extract version: "x.y.z" if needed
        })
    }

    fn parse_dependencies(&self, config_path: &Path) -> Result<Vec<LocalDependency>> {
        let content = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;

        let mut deps = Vec::new();

        // Extract path: dependencies
        for caps in DEPS_REGEX.captures_iter(&content) {
            let name = caps.get(1).unwrap().as_str();
            let path = caps.get(2).unwrap().as_str();
            deps.push(LocalDependency {
                name: name.to_string(),
                path: PathBuf::from(path),
            });
        }

        // Extract in_umbrella: true (without explicit path)
        for caps in UMBRELLA_REGEX.captures_iter(&content) {
            let name = caps.get(1).unwrap().as_str();
            // in_umbrella: true implies path: "../{name}"
            deps.push(LocalDependency {
                name: name.to_string(),
                path: PathBuf::from(format!("../{}", name)),
            });
        }

        Ok(deps)
    }
}
```

### Python Plugin Implementation

```rust
// Source: Poetry docs + PEP 621 spec + toml crate patterns
use serde::Deserialize;
use std::collections::HashMap;

pub struct PythonPlugin;

#[derive(Deserialize, Default)]
struct PyProjectToml {
    project: Option<PepProject>,
    tool: Option<ToolSection>,
}

#[derive(Deserialize, Default)]
struct PepProject {
    name: Option<String>,
    dependencies: Option<Vec<String>>,
}

#[derive(Deserialize, Default)]
struct ToolSection {
    poetry: Option<PoetrySection>,
}

#[derive(Deserialize, Default)]
struct PoetrySection {
    name: Option<String>,
    dependencies: Option<HashMap<String, PoetryDep>>,
    #[serde(rename = "dev-dependencies")]
    dev_dependencies: Option<HashMap<String, PoetryDep>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PoetryDep {
    Version(String),
    Table { path: Option<String>, #[serde(flatten)] _other: HashMap<String, toml::Value> },
}

impl LanguagePlugin for PythonPlugin {
    fn name(&self) -> &str {
        "python"
    }

    fn marker_files(&self) -> &[&str] {
        &["pyproject.toml"]
    }

    fn parse_project(&self, _root: &Path, config_path: &Path) -> Result<ProjectMetadata> {
        let content = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;

        let pyproject: PyProjectToml = toml::from_str(&content)
            .with_context(|| format!("Failed to parse {}", config_path.display()))?;

        // Priority: project.name > tool.poetry.name
        let name = pyproject.project
            .as_ref()
            .and_then(|p| p.name.clone())
            .or_else(|| pyproject.tool
                .as_ref()
                .and_then(|t| t.poetry.as_ref())
                .and_then(|p| p.name.clone()))
            .ok_or_else(|| anyhow!("Could not find project name in {}", config_path.display()))?;

        Ok(ProjectMetadata {
            name,
            version: None,
        })
    }

    fn parse_dependencies(&self, config_path: &Path) -> Result<Vec<LocalDependency>> {
        let content = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;

        let pyproject: PyProjectToml = toml::from_str(&content)
            .with_context(|| format!("Failed to parse {}", config_path.display()))?;

        let mut deps = Vec::new();

        // Extract PEP 621 path dependencies (format: "pkg @ file:../path")
        if let Some(project) = &pyproject.project {
            if let Some(dependencies) = &project.dependencies {
                for dep in dependencies {
                    if let Some(path) = extract_file_path(dep) {
                        let name = dep.split('@').next().unwrap_or("").trim();
                        deps.push(LocalDependency {
                            name: name.to_string(),
                            path: PathBuf::from(path),
                        });
                    }
                }
            }
        }

        // Extract Poetry path dependencies
        if let Some(tool) = &pyproject.tool {
            if let Some(poetry) = &tool.poetry {
                for dep_map in [&poetry.dependencies, &poetry.dev_dependencies].into_iter().flatten() {
                    for (name, dep) in dep_map {
                        if let PoetryDep::Table { path: Some(p), .. } = dep {
                            deps.push(LocalDependency {
                                name: name.clone(),
                                path: PathBuf::from(p),
                            });
                        }
                    }
                }
            }
        }

        Ok(deps)
    }
}

fn extract_file_path(dep: &str) -> Option<&str> {
    // Extract path from "pkg @ file:../relative/path" or "pkg @ file:///absolute/path"
    if let Some(idx) = dep.find("@ file:") {
        let path_start = idx + 7; // len("@ file:")
        let path = dep[path_start..].trim();
        // Handle file:///absolute/path vs file:relative/path
        Some(path.strip_prefix("//").unwrap_or(path))
    } else {
        None
    }
}
```

### Unsupported Pattern Detection

```rust
// Source: CONTEXT.md decision - error on unsupported patterns
use regex::Regex;
use std::sync::LazyLock;

// Patterns we support
static SUPPORTED_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| vec![
    Regex::new(r#"\{:\w+,\s*path:\s*"[^"]+""#).unwrap(),          // path: "..."
    Regex::new(r#"\{:\w+,\s*in_umbrella:\s*true\}"#).unwrap(),    // in_umbrella: true
]);

// Pattern that might be a local dep we don't support
static MAYBE_LOCAL_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    // Detect runtime-evaluated paths: path: Path.expand("...")
    Regex::new(r#"path:\s*[A-Z]\w+\."#).unwrap()
});

pub fn check_unsupported_patterns(content: &str, config_path: &Path) -> Result<()> {
    for cap in MAYBE_LOCAL_REGEX.find_iter(content) {
        return Err(anyhow!(
            "Unsupported path dependency pattern in {}: {}\n\
             Hint: Use a literal string path or declare dependency in aster.toml",
            config_path.display(),
            cap.as_str()
        ));
    }
    Ok(())
}
```

## State of the Art

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|--------------|--------|
| Poetry 1.x `[tool.poetry]` only | Poetry 2.0 supports `[project]` + `[tool.poetry]` | January 2025 | Must check both sections |
| setup.py for Python projects | pyproject.toml (PEP 517/621) | 2020-2023 migration | pyproject.toml is primary |
| Regex without LazyLock | `std::sync::LazyLock` (stable) | Rust 1.80 (2024) | No external crate needed |
| mix credo as built-in | credo is optional dependency | Always | Warn if mix credo fails |

**Deprecated/outdated:**
- `lazy_static` crate: Use `std::sync::LazyLock` instead (stable since Rust 1.80)
- Python setup.py as primary: pyproject.toml is now standard, setup.py is fallback
- Poetry 1.x only parsing: Poetry 2.0 has dual format support

## Open Questions

Things that couldn't be fully resolved:

1. **requirements.txt parsing depth**
   - What we know: `-e ../path` syntax for editable installs
   - What's unclear: CONTEXT.md marks as "Claude's discretion"
   - Recommendation: Skip for initial implementation. Most projects using path deps use pyproject.toml. Can add later if needed.

2. **setup.py regex reliability**
   - What we know: CONTEXT.md wants regex extraction, no Python execution
   - What's unclear: setup.py is Python code with arbitrary complexity
   - Recommendation: Implement basic pattern matching for `install_requires` list. Warn and skip if pattern not recognized. Users can use aster.toml override.

3. **Hatch workspace detection**
   - What we know: Hatch uses `workspace.members = ["packages/*"]` in pyproject.toml
   - What's unclear: How common is Hatch vs Poetry?
   - Recommendation: Support Poetry first (more common). Hatch support can be added by parsing `tool.hatch.envs.default.workspace.members`.

4. **Cross-language dependency validation**
   - What we know: CONTEXT.md says "error if target missing"
   - What's unclear: When exactly to validate - during discovery or graph building?
   - Recommendation: Validate during graph building (existing warning in `build_graph`). Change to error for cross-lang deps declared in aster.toml.

## Sources

### Primary (HIGH confidence)
- [Elixir Mix.Tasks.Deps docs](https://hexdocs.pm/mix/Mix.Tasks.Deps.html) - Official path dependency syntax
- [Poetry dependency specification](https://python-poetry.org/docs/dependency-specification/) - Path dependency format
- [PEP 621](https://peps.python.org/pep-0621/) - Python project metadata standard
- [Rust regex crate docs](https://docs.rs/regex/latest/regex/) - Named captures, LazyLock pattern
- [toml crate docs](https://docs.rs/toml) - TOML parsing with serde
- [Elixir umbrella projects](https://elixir-lang.org/getting-started/mix-otp/dependencies-and-umbrella-projects.html) - Umbrella structure
- [Ruff linter docs](https://docs.astral.sh/ruff/linter/) - ruff check command
- [Credo GitHub](https://github.com/rrrene/credo) - mix credo usage

### Secondary (MEDIUM confidence)
- [pyproject-toml-rs crate](https://github.com/PyO3/pyproject-toml-rs) - Rust pyproject.toml parsing approach
- [Python build module](https://packaging.python.org/en/latest/guides/writing-pyproject-toml/) - python -m build command

### Tertiary (LOW confidence)
- requirements.txt `-e` syntax - limited documentation on relative path behavior
- setup.py regex patterns - highly variable, custom code may not match

## Metadata

**Confidence breakdown:**
- Elixir parsing: HIGH - Official docs + straightforward regex patterns
- Python/Poetry parsing: HIGH - Official docs + serde structs are well-understood
- Target mapping: HIGH - Simple lookup table, clear from CONTEXT.md
- setup.py fallback: LOW - Python code is arbitrary, regex may miss cases
- requirements.txt: LOW - Deferred, optional feature

**Research date:** 2026-01-22
**Valid until:** 60 days (Elixir/Python ecosystems stable, Poetry 2.0 is recent but documented)
