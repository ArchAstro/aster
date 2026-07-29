# Open-source readiness

This checklist tracks the work required before changing the repository to public.

## Release blockers

- [x] Add an explicit license and complete Cargo package metadata.
- [x] Update dependencies covered by active RustSec advisories.
- [x] Use one quoting-aware, direct-execution command parser in every execution mode.
- [x] Reject duplicate project addresses and missing target dependencies.
- [x] Stop downstream targets after prerequisite failures.
- [x] Preserve relevant filesystem events received during watch builds.
- [x] Run non-stream prerequisites before starting streaming watch targets.
- [x] Reject invalid cache/watch patterns and unsupported capability names.
- [x] Make generated clean targets safe and truthful.
- [x] Define safe cache defaults and provide per-target cache controls.
- [x] Replace private-repository installation and Homebrew behavior.
- [x] Gate releases on tests, lint, dependency audit, and tag/version validation.
- [x] Restrict workflow permissions and pin third-party actions.
- [x] Remove internal-only fixtures and machine-specific paths from the public tree.
- [x] Run an all-history secret scan before changing visibility.

## Public project baseline

- [x] Document every supported language and configuration field.
- [x] Publish valid, tested configuration examples.
- [x] Document command execution, cache, affected, platform, and MSRV semantics.
- [x] Add contribution, security, conduct, support, and governance documents.
- [x] Add issue forms, a pull-request template, and dependency automation.
- [x] Limit the crates.io package to intentional files.
- [x] Test the supported OS/toolchain matrix and stop hiding core watch tests.
- [x] Upload checksums and build provenance with releases.
- [x] Enable Dependabot updates, code scanning, secret scanning, validity checks, and
  push protection.
- [ ] Enable required reviews/checks after the replacement CI lands on `main`.
- [ ] Enable private vulnerability reporting when the repository becomes public.

## Explicitly separate operations

- Repository visibility changes require a deliberate final go/no-go.
- Git history rewriting requires a reviewed list of material that must be removed.
- Branch protection should be enabled only after the replacement check names exist on
  the default branch.
