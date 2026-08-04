# Getting started with Aster

This tutorial builds a small two-project Node.js workspace from scratch. By the
end, Aster will discover both projects, install their dependencies, build them
in dependency order, run their tests, reuse cached results, watch for changes,
and select only projects affected by a Git diff.

## 1. Install the prerequisites

You need:

- Aster
- Git
- Node.js 20 or newer with npm

On macOS, install Aster from the private Homebrew tap:

```console
export HOMEBREW_GITHUB_API_TOKEN="$(gh auth token)"
brew install ArchAstro/tap/aster
aster --version
```

To build Aster from source instead, install Rust 1.88 or newer and run:

```console
cargo install --git https://github.com/ArchAstro/aster.git --locked
aster --version
```

## 2. Create a workspace

Create an empty Git repository and initialize Aster at its root:

```console
mkdir aster-demo
cd aster-demo
git init
aster init
mkdir -p packages/core/src packages/core/test
mkdir -p apps/api/src apps/api/test
```

`aster init` creates the root `aster.toml`. That file marks the workspace
boundary and can later hold workspace-wide settings for affected-project
analysis, watch mode, and development services.

## 3. Add the shared library

Create `packages/core/package.json`:

```json
{
  "name": "@demo/core",
  "version": "1.0.0",
  "private": true,
  "type": "module",
  "scripts": {
    "build": "node build.mjs",
    "test": "node --test"
  }
}
```

Create `packages/core/src/index.js`:

```js
export function greet(name) {
  return `Hello, ${name}!`;
}
```

Create `packages/core/build.mjs`:

```js
import { cp, mkdir } from "node:fs/promises";

await mkdir("dist", { recursive: true });
await cp("src/index.js", "dist/index.js");
console.log("built @demo/core");
```

Create `packages/core/test/core.test.js`:

```js
import assert from "node:assert/strict";
import test from "node:test";
import { greet } from "../src/index.js";

test("greets by name", () => {
  assert.equal(greet("Aster"), "Hello, Aster!");
});
```

Finally, create `packages/core/aster.toml`. The explicit test dependency makes
the project build before its tests run:

```toml
[targets.build]
command = "npm run build"
depends_on = ["//self:deps"]

[targets.build.cache]
outputs = ["dist/index.js"]

[targets.test]
command = "npm test"
depends_on = ["//self:deps", "//self:build"]
```

## 4. Add an application that depends on the library

Create `apps/api/package.json`:

```json
{
  "name": "@demo/api",
  "version": "1.0.0",
  "private": true,
  "type": "module",
  "scripts": {
    "build": "node build.mjs",
    "test": "node --test"
  }
}
```

Create `apps/api/build.mjs`:

```js
import { mkdir, writeFile } from "node:fs/promises";
import { greet } from "../../packages/core/dist/index.js";

await mkdir("dist", { recursive: true });
await writeFile(
  "dist/index.js",
  `export const message = ${JSON.stringify(greet("from the API"))};\n`,
);
console.log("built @demo/api");
```

Create `apps/api/test/api.test.js`:

```js
import assert from "node:assert/strict";
import test from "node:test";
import { message } from "../dist/index.js";

test("uses the shared library", () => {
  assert.equal(message, "Hello, from the API!");
});
```

Create `apps/api/aster.toml`. Its build target points to the shared library's
build target, while the project-level edge lets commands such as
`affected --dependents` include the API when the library changes:

```toml
depends_on = ["//packages/core"]

[targets.build]
command = "npm run build"
depends_on = ["//self:deps", "//packages/core:build"]

[targets.build.cache]
outputs = ["dist/index.js"]

[targets.test]
command = "npm test"
depends_on = ["//self:deps", "//self:build"]
```

Project addresses are always workspace-relative. `//packages/core` identifies
the project, while `//packages/core:build` identifies one of its targets.
`//self` refers to the project containing the current `aster.toml`.

## 5. Discover and inspect the graph

Ask Aster what it found:

```console
aster list
```

The output should include `//packages/core` and `//apps/api`, with their
detected `deps`, `build`, and `test` targets.

Inspect the API build graph and the path back to the library:

```console
aster graph //apps/api:build
aster why //apps/api:build //packages/core:build
```

This is a useful check before running a large or unfamiliar workspace.

## 6. Build and test

Build the API:

```console
aster build //apps/api
```

Aster installs dependencies, builds `//packages/core`, and only then builds
`//apps/api`. Dependencies are included by default.

Run every available test target:

```console
aster test --all
```

Useful selection forms include:

```console
aster test //packages/core       # one project plus its dependencies
aster build //apps/...           # every project below apps/
aster test .                     # the project containing the current directory
aster build --all --no-deps      # omit dependency targets explicitly
```

If a target fails, inspect its stored output:

```console
aster logs
aster logs //apps/api:build
```

Use `--full-logs` on the original command when the default failure excerpt is
not enough.

## 7. See caching in action

Run the same build again:

```console
aster build //apps/api
aster cache status
```

Successful build results are reused while their inputs and declared outputs
remain valid. Aster's cache memoizes successful execution; it does not restore
deleted artifacts. Force a fresh run or clear all local entries with:

```console
aster build //apps/api --no-cache
aster cache clear
```

## 8. Run only what changed

Create a baseline commit:

```console
git add .
git commit -m "create Aster demo"
```

Now change the greeting in `packages/core/src/index.js`, then preview the tests
affected by that uncommitted edit:

```console
aster affected test --base=HEAD --dependents --dry-run
```

The library is directly affected, and the API is included because it depends
on the library. Remove `--dry-run` to execute the selected targets:

```console
aster affected test --base=HEAD --dependents
```

In CI, use the target branch as the base and fetch enough history for Git to
find the merge base:

```console
aster affected test --base=origin/main --dependents
```

## 9. Rebuild while editing

Start watch mode for the API build:

```console
aster watch //apps/api:build
```

Edit either project while the command is running. Aster watches the requested
target and its transitive dependencies, then reruns the relevant work. Press
Ctrl-C to stop.

## 10. Apply this to an existing repository

For a real repository:

1. Run `aster init` once at the Git root.
2. Run `aster list` to review automatically detected projects and targets.
3. Add a project-level `aster.toml` only where you need custom commands,
   dependency edges, caching inputs or outputs, aliases, or file capabilities.
4. Start with a narrow command such as `aster test //path/to/project`, then
   expand to `--all` or `aster affected` after the graph looks correct.
5. Add the verified command to CI.

From there, see the main [README](README.md) for supported languages, richer
project configuration, affected-path exclusions, watch settings, and the
`aster services up` development dashboard. Run `aster --help` or
`aster <command> --help` for the complete command reference.
