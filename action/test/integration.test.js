const assert = require("node:assert/strict");
const { spawnSync } = require("node:child_process");
const fs = require("node:fs");
const tls = require("node:tls");
const os = require("node:os");
const path = require("node:path");
const { describe, it } = require("node:test");

const repoRoot = path.resolve(__dirname, "../..");
const fixture = path.join(__dirname, "fixture");
const actionMain = path.join(__dirname, "../dist/index.js");

describe("action integration", () => {
  it("second aster run reports the fixture target cached", async () => {
    const aster = await builtAster();
    const server = await startCache();
    const workspace = fs.mkdtempSync(path.join(os.tmpdir(), "aster-action-"));
    try {
      copyFixture(workspace);
      const eventPath = path.join(workspace, "event.json");
      fs.writeFileSync(
        eventPath,
        JSON.stringify({ repository: { default_branch: "main" } }),
      );
      const token = "runtime-token-integration";
      const baseEnv = {
        PATH: process.env.PATH,
        HOME: process.env.HOME,
        RUNNER_TEMP: workspace,
        RUNNER_TOOL_CACHE: path.join(workspace, "tool-cache"),
        ASTER_GHA_EXTRA_CA: server.caPath,
        GITHUB_EVENT_PATH: eventPath,
        GITHUB_EVENT_NAME: "push",
        GITHUB_REF: "refs/heads/main",
        GITHUB_WORKSPACE: workspace,
        INPUT_RUN: "run //app:build --no-deps --json",
        INPUT_CACHE_MODE: "auto",
        "INPUT_CACHE-MODE": "auto",
        INPUT_ASTER_PATH: aster,
        "INPUT_ASTER-PATH": aster,
        INPUT_RUN: "run //app:build --no-deps --json",
        INPUT_VERSION: "",
        INPUT_WORKING_DIRECTORY: "",
        "INPUT_WORKING-DIRECTORY": "",
      };
      const first = await runAction(baseEnv, server.url, token);
      assert.equal(first.status, 0, first.stderr || first.stdout);
      const firstJson = jsonFrom(first.stdout);
      assert.equal(firstJson.results[0].cached, false, JSON.stringify(firstJson));
      assert.equal(server.seenToken, `Bearer ${token}`);

      fs.rmSync(path.join(workspace, "app/out"), { recursive: true, force: true });
      const second = await runAction(baseEnv, server.url, token);
      assert.equal(second.status, 0, second.stderr);
      const secondJson = jsonFrom(second.stdout);
      const target = secondJson.results.find((result) => result.address === "//app:build");
      assert.equal(target.status, "cached", JSON.stringify(secondJson));
      assert.equal(target.cached, true);
      assert.equal(fs.readFileSync(path.join(workspace, "app/out/file.txt"), "utf8"), "hi\n");
      assert.equal(
        fs.readFileSync(path.join(workspace, "app/runs.txt"), "utf8").trim().split("\n").length,
        1,
      );
      assert.equal(process.env.ACTIONS_RUNTIME_TOKEN, undefined);
      assert.equal(process.env.ACTIONS_RESULTS_URL, undefined);
    } finally {
      server.close();
      fs.rmSync(workspace, { recursive: true, force: true });
    }
  });
});

function runAction(baseEnv, resultsUrl, token) {
  const child = require("node:child_process").spawn(process.execPath, [actionMain], {
    cwd: repoRoot,
    env: { ...baseEnv, ACTIONS_RESULTS_URL: resultsUrl, ACTIONS_RUNTIME_TOKEN: token },
  });
  return new Promise((resolve) => {
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => (stdout += chunk));
    child.stderr.on("data", (chunk) => (stderr += chunk));
    child.on("exit", (status) => resolve({ status, stdout, stderr }));
  });
}

function jsonFrom(stdout) {
  const start = stdout.indexOf("{");
  const end = stdout.lastIndexOf("}");
  assert.notEqual(start, -1, stdout);
  return JSON.parse(stdout.slice(start, end + 1));
}

function copyFixture(workspace) {
  fs.cpSync(fixture, workspace, { recursive: true });
  fs.mkdirSync(path.join(workspace, ".git"));
}

async function builtAster() {
  const binary = path.join(repoRoot, "target/debug/aster");
  if (!fs.existsSync(binary)) {
    const build = spawnSync("cargo", ["build", "--locked", "--bin", "aster"], {
      cwd: repoRoot,
      stdio: "inherit",
    });
    assert.equal(build.status, 0, "cargo build failed");
  }
  return binary;
}

async function startCache() {
  const certs = forgeCert();
  const blobs = new Map();
  const state = { seenToken: "" };
  const httpsServer = tls.createServer({ cert: certs.cert, key: certs.key }, (socket) => {
    const chunks = [];
    socket.on("data", (chunk) => {
      chunks.push(chunk);
      const text = Buffer.concat(chunks).toString();
      const headerEnd = text.indexOf("\r\n\r\n");
      if (headerEnd === -1) return;
      const head = text.slice(0, headerEnd);
      const bodyStart = headerEnd + 4;
      const headers = Object.fromEntries(
        head.split("\r\n").slice(1).filter(Boolean).map((line) => {
          const index = line.indexOf(":");
          return [line.slice(0, index).toLowerCase(), line.slice(index + 1).trim()];
        }),
      );
      const length = Number(headers["content-length"] ?? 0);
      const raw = Buffer.concat(chunks);
      if (raw.length < bodyStart + length) return;
      const body = raw.subarray(bodyStart, bodyStart + length).toString();
      const requestLine = head.split("\r\n")[0];
      if (headers.authorization) state.seenToken = headers.authorization;
      const path = requestLine.split(" ")[1] ?? "/";
      const method = requestLine.split(" ")[0];
      const respond = (status, payload) => {
        const bytes = Buffer.from(payload);
        socket.write(`HTTP/1.1 ${status} OK\r\ncontent-type: application/json\r\ncontent-length: ${bytes.length}\r\nconnection: close\r\n\r\n`);
        socket.end(bytes);
      };
      if (path.endsWith("/GetCacheEntryDownloadURL")) {
        const key = JSON.parse(body).key;
        const hit = blobs.has(key);
        respond(200, JSON.stringify({ ok: hit, signed_download_url: hit ? `https://127.0.0.1:${httpsServer.address().port}/blob/${encodeURIComponent(key)}` : "" }));
        return;
      }
      if (path.endsWith("/CreateCacheEntry")) {
        const key = JSON.parse(body).key;
        respond(200, JSON.stringify({ ok: true, signedUploadUrl: `https://127.0.0.1:${httpsServer.address().port}/blob/${encodeURIComponent(key)}?sig=1` }));
        return;
      }
      if (path.endsWith("/FinalizeCacheEntryUpload")) {
        respond(200, JSON.stringify({ ok: true, entry_id: "1" }));
        return;
      }
      if (method === "PUT" && path.startsWith("/blob/")) {
        blobs.set(decodeURIComponent(path.slice("/blob/".length).split("?")[0]), Buffer.from(body));
        socket.end("HTTP/1.1 201 Created\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        return;
      }
      if (method === "GET" && path.startsWith("/blob/")) {
        const bytes = blobs.get(decodeURIComponent(path.slice("/blob/".length).split("?")[0])) ?? Buffer.alloc(0);
        socket.write(`HTTP/1.1 200 OK\r\ncontent-length: ${bytes.length}\r\nconnection: close\r\n\r\n`);
        socket.end(bytes);
        return;
      }
      socket.end("HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
    });
  });
  await listen(httpsServer);
  return {
    url: `https://127.0.0.1:${httpsServer.address().port}`,
    caPath: certs.caPath,
    get seenToken() {
      return state.seenToken;
    },
    close: () => httpsServer.close(),
  };
}

function json(res, status, value) {
  res.writeHead(status, { "content-type": "application/json" });
  res.end(JSON.stringify(value));
}

function listen(server) {
  return new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
}

function forgeCert() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "aster-action-ca-"));
  const config = path.join(dir, "openssl.cnf");
  fs.writeFileSync(
    config,
    "[req]\ndistinguished_name=dn\nx509_extensions=ext\nprompt=no\n[dn]\nCN=aster-action-test\n[ext]\nbasicConstraints=critical,CA:FALSE\nsubjectAltName=IP:127.0.0.1\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n",
  );
  const signed = spawnSync(
    "openssl",
    [
      "req",
      "-new",
      "-x509",
      "-newkey",
      "ec",
      "-pkeyopt",
      "ec_paramgen_curve:prime256v1",
      "-nodes",
      "-keyout",
      path.join(dir, "key.pem"),
      "-out",
      path.join(dir, "cert.pem"),
      "-days",
      "1",
      "-config",
      config,
    ],
    { encoding: "utf8" },
  );
  assert.equal(signed.status, 0, signed.stderr);
  fs.copyFileSync(path.join(dir, "cert.pem"), path.join(dir, "ca.pem"));
  return {
    cert: fs.readFileSync(path.join(dir, "cert.pem")),
    key: fs.readFileSync(path.join(dir, "key.pem")),
    caPath: path.join(dir, "ca.pem"),
  };
}
