import * as exec from "@actions/exec";
import { childEnv, splitCommand, type SpawnRequest } from "./run-env";

export { SCOPED_ENV, childEnv } from "./run-env";

export interface SpawnResult {
  exitCode: number;
  childEnv: Record<string, string>;
}

export async function spawnAster(request: SpawnRequest): Promise<SpawnResult> {
  const env = childEnv(request);
  const exitCode = await exec.exec(request.aster, splitCommand(request.command), {
    cwd: request.cwd,
    env,
    ignoreReturnCode: true,
  });
  return { exitCode, childEnv: env };
}
