import * as tc from "@actions/tool-cache";
import * as exec from "@actions/exec";
import * as fs from "node:fs";
import * as path from "node:path";
import { releaseAsset, releaseTag } from "./platform";

const OWNER = "ArchAstro";
const REPO = "aster";
const TOOL = "aster";

export interface InstallRequest {
  version: string;
  token: string;
  asterPath: string;
}

interface Release {
  tag_name: string;
  draft: boolean;
  prerelease: boolean;
}

/** Local binary when `aster-path` is set; otherwise a cached release asset. */
export async function installAster(request: InstallRequest): Promise<string> {
  const override = request.asterPath.trim();
  if (override) {
    if (!fs.existsSync(override)) {
      throw new Error(`aster-path does not exist: ${override}`);
    }
    return override;
  }
  const tag = request.version.trim()
    ? releaseTag(request.version)
    : await latestTag(request.token);
  const asset = releaseAsset();
  const cached = tc.find(TOOL, tag);
  if (cached) {
    return path.join(cached, TOOL);
  }
  const archive = await tc.downloadTool(downloadUrl(tag, asset), undefined, request.token);
  const extracted = await tc.extractTar(archive);
  const binary = path.join(extracted, TOOL);
  await exec.exec("chmod", ["+x", binary]);
  const stored = await tc.cacheDir(extracted, TOOL, tag);
  return path.join(stored, TOOL);
}

export function downloadUrl(tag: string, asset: string): string {
  return `https://github.com/${OWNER}/${REPO}/releases/download/${tag}/${asset}`;
}

async function latestTag(token: string): Promise<string> {
  const headers: Record<string, string> = {
    accept: "application/vnd.github+json",
    "user-agent": "aster-action",
  };
  if (token) {
    headers.authorization = `Bearer ${token}`;
  }
  const response = await fetch(
    `https://api.github.com/repos/${OWNER}/${REPO}/releases?per_page=20`,
    { headers },
  );
  if (!response.ok) {
    const statusLine = `${response.status} ${response.statusText}`.trim();
    const hint =
      response.status === 403
        ? ". Unauthenticated release lookup is rate limited (60/hour). Pin version: to a release tag, or pass env: GITHUB_TOKEN: ${{ github.token }}"
        : "";
    throw new Error(`GitHub releases request failed: ${statusLine}${hint}`);
  }
  const releases = (await response.json()) as Release[];
  const stable = releases.find((release) => !release.draft && !release.prerelease);
  if (!stable) {
    throw new Error("no stable Aster release found");
  }
  return stable.tag_name;
}
