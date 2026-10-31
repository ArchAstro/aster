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

/**
 * Split `run` into aster arguments.
 * Single quotes are literal. Double quotes honor backslash escapes.
 * A leading `aster` token is the program name, which this action already supplies.
 */
export function splitCommand(command: string): string[] {
  const parts = splitQuoted(command);
  if (parts.length === 0) {
    throw new Error("run input is empty");
  }
  if (parts[0] === "aster") {
    throw new Error("run takes aster arguments, not the program name");
  }
  return parts;
}

function splitQuoted(command: string): string[] {
  const parts: string[] = [];
  let current = "";
  let inWord = false;
  let quote: "'" | '"' | null = null;
  for (let i = 0; i < command.length; i++) {
    const ch = command[i];
    if (quote === "'") {
      if (ch === "'") {
        quote = null;
      } else {
        current += ch;
      }
      continue;
    }
    if (quote === '"') {
      if (ch === '"') {
        quote = null;
        continue;
      }
      if (ch === "\\") {
        const next = command[i + 1];
        if (next === undefined) {
          throw new Error("run input has a trailing backslash");
        }
        if (next === "$" || next === "`" || next === '"' || next === "\\" || next === "\n") {
          if (next !== "\n") {
            current += next;
          }
          i++;
          continue;
        }
      }
      current += ch;
      continue;
    }
    if (ch === "'" || ch === '"') {
      quote = ch;
      inWord = true;
      continue;
    }
    if (ch === "\\") {
      const next = command[i + 1];
      if (next === undefined) {
        throw new Error("run input has a trailing backslash");
      }
      if (next !== "\n") {
        current += next;
        inWord = true;
      }
      i++;
      continue;
    }
    if (isSpace(ch)) {
      if (inWord) {
        parts.push(current);
        current = "";
        inWord = false;
      }
      continue;
    }
    current += ch;
    inWord = true;
  }
  if (quote !== null) {
    throw new Error("run input has an unbalanced quote");
  }
  if (inWord) {
    parts.push(current);
  }
  return parts;
}

function isSpace(ch: string): boolean {
  return ch === " " || ch === "\t" || ch === "\n" || ch === "\r" || ch === "\v" || ch === "\f";
}
