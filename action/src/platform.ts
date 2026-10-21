export interface AssetRequest {
  platform: NodeJS.Platform;
  arch: string;
}

const ASSETS: Record<string, string> = {
  "linux:x64": "aster-linux-x64.tar.gz",
  "darwin:x64": "aster-darwin-x64.tar.gz",
  "darwin:arm64": "aster-darwin-arm64.tar.gz",
};

/** Release asset published by .github/workflows/release.yml for this host. */
export function releaseAsset(host: AssetRequest = process): string {
  const name = ASSETS[`${host.platform}:${host.arch}`];
  if (!name) {
    throw new Error(
      `no Aster release asset for ${host.platform}/${host.arch}; published assets are linux/x64, darwin/x64, and darwin/arm64`,
    );
  }
  return name;
}

/** Tag used in the release download URL. `v1.2.3` and `1.2.3` both work. */
export function releaseTag(version: string): string {
  const trimmed = version.trim();
  if (!trimmed) {
    throw new Error("version is empty");
  }
  return trimmed.startsWith("v") ? trimmed : `v${trimmed}`;
}
