/** Names that must exist only in the spawned aster process. */
export const SCOPED_ENV = [
  "ACTIONS_RESULTS_URL",
  "ACTIONS_RUNTIME_TOKEN",
  "ASTER_REMOTE_CACHE",
  "ASTER_CACHE_MODE",
] as const;

export interface SpawnRequest {
  aster: string;
  command: string;
  cwd: string;
  parentEnv: NodeJS.ProcessEnv;
  resultsUrl: string;
  runtimeToken: string;
  cacheMode: string;
}

/** Child env: the parent env plus the cache token, never exported upward. */
export function childEnv(request: SpawnRequest): Record<string, string> {
  const env: Record<string, string> = {};
  for (const [key, value] of Object.entries(request.parentEnv)) {
    if (value !== undefined) {
      env[key] = value;
    }
  }
  env.ACTIONS_RESULTS_URL = request.resultsUrl;
  env.ACTIONS_RUNTIME_TOKEN = request.runtimeToken;
  env.ASTER_REMOTE_CACHE = "gha";
  env.ASTER_CACHE_MODE = request.cacheMode;
  return env;
}

/** Split a shell-less command. Quotes are not interpreted. */
export function splitCommand(command: string): string[] {
  const parts = command.trim().split(/\s+/);
  if (parts.length === 0 || parts[0] === "") {
    throw new Error("run input is empty");
  }
  return parts;
}
