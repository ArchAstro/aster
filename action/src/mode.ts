export type CacheMode = "rw" | "read" | "off";

export interface ModeInput {
  cacheMode: string;
  eventName: string;
  ref: string;
  defaultBranch: string;
}

const OVERRIDES = new Set<CacheMode>(["rw", "read", "off"]);

/**
 * `auto` writes on push, schedule, and workflow_dispatch to the default
 * branch. Every other event, including a missing payload, is read-only.
 * `rw`, `read`, and `off` override that.
 */
export function selectCacheMode(input: ModeInput): CacheMode {
  const requested = input.cacheMode.trim();
  if (OVERRIDES.has(requested as CacheMode)) {
    return requested as CacheMode;
  }
  if (requested !== "auto" && requested !== "") {
    throw new Error(
      `invalid cache-mode ${JSON.stringify(requested)}; expected auto, rw, read, or off`,
    );
  }
  const writable =
    (input.eventName === "push" ||
      input.eventName === "schedule" ||
      input.eventName === "workflow_dispatch") &&
    input.defaultBranch !== "" &&
    input.ref === `refs/heads/${input.defaultBranch}`;
  return writable ? "rw" : "read";
}
