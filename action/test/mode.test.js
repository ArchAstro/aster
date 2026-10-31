const assert = require("node:assert/strict");
const { describe, it } = require("node:test");
const { selectCacheMode } = require("../dist/mode.js");
const { childEnv, SCOPED_ENV, splitCommand } = require("../dist/run.js");

describe("selectCacheMode", () => {
  it("push to the default branch selects rw", () => {
    assert.equal(
      selectCacheMode({
        cacheMode: "auto",
        eventName: "push",
        ref: "refs/heads/main",
        defaultBranch: "main",
      }),
      "rw",
    );
  });

  it("schedule on the default branch selects rw", () => {
    assert.equal(
      selectCacheMode({
        cacheMode: "auto",
        eventName: "schedule",
        ref: "refs/heads/trunk",
        defaultBranch: "trunk",
      }),
      "rw",
    );
  });

  it("workflow_dispatch on the default branch selects rw", () => {
    assert.equal(
      selectCacheMode({
        cacheMode: "auto",
        eventName: "workflow_dispatch",
        ref: "refs/heads/main",
        defaultBranch: "main",
      }),
      "rw",
    );
  });

  it("pull_request selects read", () => {
    assert.equal(
      selectCacheMode({
        cacheMode: "auto",
        eventName: "pull_request",
        ref: "refs/pull/7/merge",
        defaultBranch: "main",
      }),
      "read",
    );
  });

  it("push to a non-default branch selects read", () => {
    assert.equal(
      selectCacheMode({
        cacheMode: "auto",
        eventName: "push",
        ref: "refs/heads/feature",
        defaultBranch: "main",
      }),
      "read",
    );
  });

  it("cache-mode input overrides auto", () => {
    assert.equal(
      selectCacheMode({
        cacheMode: "off",
        eventName: "push",
        ref: "refs/heads/main",
        defaultBranch: "main",
      }),
      "off",
    );
    assert.equal(
      selectCacheMode({
        cacheMode: "rw",
        eventName: "pull_request",
        ref: "refs/pull/1/merge",
        defaultBranch: "main",
      }),
      "rw",
    );
  });
});

describe("child env", () => {
  it("run does not export ACTIONS_RUNTIME_TOKEN", () => {
    const source = require("node:fs").readFileSync(
      require("node:path").join(__dirname, "../src/index.ts"),
      "utf8",
    );
    assert.doesNotMatch(source, /exportVariable\s*\(/);
    assert.doesNotMatch(source, /exportSecret\s*\(/);
    const env = childEnv({
      aster: "aster",
      command: "run //app:build",
      cwd: ".",
      parentEnv: { PATH: "/usr/bin" },
      resultsUrl: "https://results.example",
      runtimeToken: "runtime-token",
      cacheMode: "read",
    });
    assert.equal(env.ACTIONS_RUNTIME_TOKEN, "runtime-token");
    assert.equal(env.ASTER_REMOTE_CACHE, "gha");
    assert.equal(env.ASTER_CACHE_MODE, "read");
    for (const name of SCOPED_ENV) {
      assert.equal(process.env[name], undefined, `${name} leaked to the parent`);
    }
  });
});

describe("splitCommand", () => {
  it("rejects a leading aster program name", () => {
    assert.throws(
      () => splitCommand("aster build //app"),
      /run takes aster arguments, not the program name/,
    );
    assert.throws(
      () => splitCommand("  aster build //app"),
      /run takes aster arguments, not the program name/,
    );
  });

  it("passes a quoted glob as one argument", () => {
    assert.deepEqual(splitCommand("test 'apps/*'"), ["test", "apps/*"]);
    assert.deepEqual(splitCommand('test "apps/*"'), ["test", "apps/*"]);
    assert.deepEqual(splitCommand("test 'apps/foo bar'"), ["test", "apps/foo bar"]);
  });

  it("rejects an unbalanced quote", () => {
    assert.throws(() => splitCommand("test 'apps/*"), /unbalanced quote/);
    assert.throws(() => splitCommand('test "apps/*'), /unbalanced quote/);
  });

  it("keeps an unquoted command and a later aster token", () => {
    assert.deepEqual(splitCommand("run //app:build --no-deps --json"), [
      "run",
      "//app:build",
      "--no-deps",
      "--json",
    ]);
    assert.deepEqual(splitCommand("build //pkg:aster"), ["build", "//pkg:aster"]);
  });

  it("rejects an empty run input", () => {
    assert.throws(() => splitCommand("   "), /run input is empty/);
  });
});
