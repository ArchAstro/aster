# README screenshots

These images are rendered from a small, synthetic workspace shaped like
FirstLanding. The fixtures exercise the real Aster binary without requiring
private source code, credentials, language toolchains, or live services.

| Asset | Reader question | Fixture | Terminal | Assertion |
| --- | --- | --- | --- | --- |
| `../images/firstlanding-build.png` | How does Aster order a polyglot build? | `fixtures/firstlanding-build.yaml` | 112 × 24 | Six dependency-ordered targets pass. |
| `../images/firstlanding-services.png` | What does the supervised services dashboard look like? | `fixtures/firstlanding-services.yaml` | 126 × 32 | Four named services start and the platform log is visible. |

Regenerate from the repository root after building Aster and installing
Astroshot's Chromium runtime:

```console
cargo build
npx --@archastro:registry=https://registry.npmjs.org @archastro/astroshot install-browser
npx --@archastro:registry=https://registry.npmjs.org @archastro/astroshot pty docs/screenshots/fixtures/firstlanding-build.yaml -o docs/images/firstlanding-build.png
npx --@archastro:registry=https://registry.npmjs.org @archastro/astroshot pty docs/screenshots/fixtures/firstlanding-services.yaml -o docs/images/firstlanding-services.png
```

The PTY commands execute the fixture workspace with the current user's
permissions. Review fixture changes before regenerating the images.
