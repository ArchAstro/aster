const fs = require("node:fs");
const path = require("node:path");

for (const [dir, name] of [
  ["mode", "mode.js"],
  ["run", "run.js"],
]) {
  const from = path.join(__dirname, "../dist", dir, "index.js");
  fs.copyFileSync(from, path.join(__dirname, "../dist", name));
  fs.rmSync(path.join(__dirname, "../dist", dir), { recursive: true, force: true });
}
