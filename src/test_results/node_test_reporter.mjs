// Aster's reporter for Node's built-in test runner (`node --test`).
//
// Aster loads it through NODE_OPTIONS next to the `spec` reporter, so the
// console output is unchanged. It appends one JSON line per test to the
// file named by ASTER_NODE_TEST_REPORT; several `node --test` runs in one
// command append to the same file. Suites, and tests that only group
// subtests, are not recorded: their own subtests are.
import { appendFileSync } from "node:fs";

const out = process.env.ASTER_NODE_TEST_REPORT;

export default async function* asterReporter(source) {
  // Per test file: the names of the tests now running, by nesting level,
  // and whether each has started a subtest.
  const files = new Map();
  for await (const event of source) {
    if (!out) continue;
    const data = event.data ?? {};
    const nesting = data.nesting ?? 0;
    let open = files.get(data.file);
    if (!open) {
      open = { names: [], parents: [] };
      files.set(data.file, open);
    }
    if (event.type === "test:start") {
      open.names.length = nesting;
      open.names[nesting] = data.name;
      open.parents.length = nesting;
      open.parents[nesting] = false;
      if (nesting > 0) open.parents[nesting - 1] = true;
      continue;
    }
    if (event.type !== "test:pass" && event.type !== "test:fail") continue;
    const details = data.details ?? {};
    const started = open.names[nesting] === data.name;
    if (details.type === "suite" || (started && open.parents[nesting])) continue;
    const error = details.error;
    const cause = error?.cause ?? error;
    let outcome = "pass";
    if (data.skip !== undefined || data.todo !== undefined) outcome = "skip";
    else if (event.type === "test:fail") {
      outcome = cause?.code === "ERR_ASSERTION" ? "fail" : "error";
    }
    const line = {
      file: data.file ?? null,
      name: [...(started ? open.names.slice(0, nesting) : []), data.name].join(" > "),
      outcome,
      duration_ms: details.duration_ms ?? null,
      message:
        event.type === "test:fail" && cause
          ? String(cause.stack ?? cause.message ?? cause)
          : null,
    };
    try {
      appendFileSync(out, JSON.stringify(line) + "\n");
    } catch {
      // Losing results must not fail the tests.
    }
  }
}
