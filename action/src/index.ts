import * as core from "@actions/core";
import * as fs from "node:fs";
import * as path from "node:path";
import { installAster } from "./install";
import { selectCacheMode } from "./mode";
import { spawnAster } from "./run";

interface EventPayload {
  repository?: { default_branch?: string };
}

async function run(): Promise<void> {
  const command = core.getInput("run", { required: true });
  const cacheMode = selectCacheMode({
    cacheMode: core.getInput("cache-mode") || "auto",
    eventName: process.env.GITHUB_EVENT_NAME ?? "",
    ref: process.env.GITHUB_REF ?? "",
    defaultBranch: defaultBranch(),
  });
  const aster = await installAster({
    version: core.getInput("version"),
    token: process.env.GITHUB_TOKEN ?? "",
    asterPath: core.getInput("aster-path"),
  });
  const cwd = workingDirectory();
  core.info(`Running ${command} with ASTER_CACHE_MODE=${cacheMode}`);
  const result = await spawnAster({
    aster,
    command,
    cwd,
    parentEnv: process.env,
    resultsUrl: process.env.ACTIONS_RESULTS_URL ?? "",
    runtimeToken: process.env.ACTIONS_RUNTIME_TOKEN ?? "",
    cacheMode,
  });
  if (result.exitCode !== 0) {
    core.setFailed(`aster exited ${result.exitCode}`);
  }
}

function defaultBranch(): string {
  const file = process.env.GITHUB_EVENT_PATH;
  if (!file || !fs.existsSync(file)) {
    return "";
  }
  const payload = JSON.parse(fs.readFileSync(file, "utf8")) as EventPayload;
  return payload.repository?.default_branch ?? "";
}

function workingDirectory(): string {
  const requested = core.getInput("working-directory");
  if (!requested) {
    return process.env.GITHUB_WORKSPACE ?? process.cwd();
  }
  return path.resolve(process.env.GITHUB_WORKSPACE ?? process.cwd(), requested);
}

run().catch((error: unknown) => {
  core.setFailed(error instanceof Error ? error.message : String(error));
});
