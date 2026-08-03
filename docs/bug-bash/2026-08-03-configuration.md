# Configuration bug bash — 2026-08-03

Scope: every field in the `aster.toml` project/workspace models, strict unknown-field handling, malformed and boundary values, semantic dependency/capability/glob validation, and mutation fuzzing. Every scenario is sent through both public loading paths: `parse_aster_toml` and `WorkspaceConfig::load`.

## Harness

`tests/config_bug_bash.rs` is a deterministic regression harness with two layers:

- a named matrix of 38 valid, invalid, boundary, and cross-field scenarios;
- 10,000 reproducible mutations across seven representative seed documents, including delimiter, quote, newline, NUL, comment, address, and Unicode insertions.

The fuzz seed is fixed (`0x6a09e667f3bcc909`), inputs are bounded, and panics report the mutation index and PRNG state. The harness has no new dependencies and runs in about one second locally.

## Running log

| # | Configuration tried | Expected | Result / observation |
|---:|---|---|---|
| 1 | Empty document | accept | Both loaders returned defaults. |
| 2 | Comment-only document | accept | Both loaders ignored comments. |
| 3 | Unicode project name (`服务-🚀`) | accept | UTF-8 survived both loaders. |
| 4 | Simple string target | accept | Command preserved. |
| 5 | Rich target with all non-cache fields | accept | Capability, glob, streaming, invalidation, and exclusive resource fields parsed. |
| 6 | Alias target with `//self:` dependency | accept | Alias shape and dependency parsed. |
| 7 | Root dependencies with and without target suffix | accept | Both address forms parsed. |
| 8 | Multiple discovery ignore globs | accept | Lists parsed consistently. |
| 9 | Watch ignores, suppression, and maximum TOML integer debounce | accept | Signed TOML boundary (`i64::MAX`) converted safely to `u64`. |
| 10 | Affected ignore configuration | accept | Workspace/project views agreed. |
| 11 | Fixed port at 65535 | accept | Upper `u16` boundary accepted. |
| 12 | Resolved port with scalar `env` | accept | One-or-many scalar form accepted. |
| 13 | Resolved port with list `env` and empty `file_env` | accept | List form and explicit empty override accepted. |
| 14 | Derived port with saturation | accept | Offset fields and boolean parsed. |
| 15 | Full development service with minimum `i32` order | accept | Service strings, maps, lists, and order boundary parsed. |
| 16 | Full cache override | accept | Enabled/include/exclude/env/outputs parsed. |
| 17 | Quoted target containing a colon | accept | Legal TOML dotted-key escaping preserved the target name. |
| 18 | Literal multiline command | accept | Quotes and embedded newline preserved. |
| 19 | Unknown top-level key | reject | Strict root schema rejected it. |
| 20 | Unknown watch key | reject | Strict nested schema rejected it. |
| 21 | Unknown affected key | reject | Strict nested schema rejected it. |
| 22 | Unknown dev key | reject | Strict nested schema rejected it. |
| 23 | Unknown port key | reject | Untagged port variant rejected it. |
| 24 | Unknown service key | reject | Strict service schema rejected it. |
| 25 | Unknown rich-target key | reject | Strict target schema rejected it. |
| 26 | Unknown alias key | reject | No untagged target variant accepted it. |
| 27 | Unknown cache key | reject | Strict cache schema rejected it. |
| 28 | Malformed table header | reject | TOML parser returned an error, no panic. |
| 29 | Duplicate project-name key | reject | TOML parser rejected duplication. |
| 30 | Integer supplied for project name | reject | Serde type check rejected it. |
| 31 | Mixed string/integer dependency list | reject | Serde type check rejected it. |
| 32 | Root dependency without `//` | reject | **Bug found:** workspace loader originally accepted it while project loader rejected it. Fixed with shared semantic validation. |
| 33 | Rich-target dependency without `//` | reject | **Bug found:** both loaders originally skipped nested dependency validation. Fixed and retained as regression coverage. |
| 34 | Alias dependency without `//` | reject | Same nested-dependency bug; fixed and covered separately. |
| 35 | Misspelled capability (`file_list`) | reject | Workspace loader now matches project-loader semantic rejection. |
| 36 | Invalid `files_glob` | reject | Workspace loader now compiles and rejects the glob too. |
| 37 | Invalid cache include glob | reject | Workspace loader now compiles and rejects the glob too. |
| 38 | Unterminated string | reject | TOML parser returned an error, no panic. |
| 39 | 10,000 deterministic delimiter/Unicode/NUL mutations | accept or reject, never panic | Completed without panic in either loader. |

## Bugs and regression fixes

1. Root configuration validation depended on the command path. `WorkspaceConfig::load` only deserialized while `parse_aster_toml` also checked dependency addresses, capabilities, and globs. Shared `validate_aster_config` now runs after both deserializers. Scenarios 32 and 35–37 are regression cases.
2. Rich and alias target `depends_on` entries were never address-validated by either loader. They now receive the same contextual validation as top-level dependencies. Scenarios 33–34 are regression cases.

## Verification log

- Pre-change baseline: `cargo test --all-targets` — 553 passed, 0 failed.
- Harness after fixes: `cargo test --test config_bug_bash -- --nocapture` — 2 passed, including 38 matrix cases and 10,000 mutations.
- Focused existing suite: `cargo test config:: --lib` — 36 passed.
- Full suite: `cargo test --locked --all-targets --all-features` — 555 passed, 0 failed.
- Static/documentation gates: strict Clippy, rustfmt check, diff check, and rustdoc with warnings denied all passed.
- `cargo audit` was not available in the environment and was not installed as part of this bug bash.
