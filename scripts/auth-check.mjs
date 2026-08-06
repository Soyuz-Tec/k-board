/**
 * Proves the server refuses what it claims to refuse.
 *
 * The unit tests prove the token maths — that a signature verifies, that an
 * expired grant is rejected. None of that is the security claim. The claim is
 * that *the running server* turns an unauthenticated connection away, and that
 * it will not start in the one configuration where being open is dangerous.
 * Only a real process, refusing a real handshake, demonstrates either.
 *
 *   node scripts/auth-check.mjs
 *
 * Exits non-zero on failure so CI can gate on it.
 */

import { spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import { request } from "node:http";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { loadEngine } from "../web/kboard.js";

const PORT = process.env.KBOARD_AUTH_PORT ?? "8098";
const BASE = `http://127.0.0.1:${PORT}`;
const SECRET = "an unusually long check secret, not a real one";
// Scopes are alphanumeric plus `- _ . :` — a slash is refused as path
// traversal, so the tenant separator is a colon.
const SCOPE = "tenant-a:board";
const BINARY =
  process.env.KBOARD_BIN ??
  (process.platform === "win32"
    ? "target/release/kboard-server.exe"
    : "target/release/kboard-server");

const workspace = mkdtempSync(join(tmpdir(), "kboard-auth-"));
const database = join(workspace, "boards.db");

const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

let failures = 0;
function check(label, condition, detail = "") {
  if (condition) {
    console.log(`  PASS  ${label}`);
  } else {
    failures += 1;
    console.error(`  FAIL  ${label}${detail ? ` — ${detail}` : ""}`);
  }
}

function isolatedEnvironment(overrides = {}) {
  const env = { ...process.env };
  for (const name of ["KBOARD_SECRET", "KBOARD_BIND", "KBOARD_ALLOWED_ORIGINS", "KBOARD_DB"]) {
    delete env[name];
  }
  return { ...env, ...overrides };
}

/** Run the server to completion, collecting what it said. */
function run(args, env) {
  return new Promise((resolve) => {
    // The parent's own KBOARD_* settings must not leak in, or a check that
    // depends on a secret being absent would silently pass for the wrong
    // reason.
    const child = spawn(BINARY, args, {
      env: isolatedEnvironment(env),
      stdio: ["ignore", "pipe", "pipe"],
    });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => (stdout += chunk));
    child.stderr.on("data", (chunk) => (stderr += chunk));
    child.on("exit", (code) => resolve({ code, stdout, stderr }));
  });
}

async function startServer(env) {
  const child = spawn(BINARY, [], {
    env: isolatedEnvironment({
      PORT,
      KBOARD_DB: database,
      ...env,
    }),
    stdio: ["ignore", "pipe", "pipe"],
  });
  child.stdout.resume();
  child.kboardStderr = "";
  child.stderr.on("data", (chunk) => (child.kboardStderr += chunk));

  for (let attempt = 0; attempt < 60; attempt += 1) {
    if (child.exitCode !== null) {
      throw new Error(
        `server exited before becoming healthy (exit=${child.exitCode}): ${child.kboardStderr.trim()}`,
      );
    }
    try {
      const response = await fetch(`${BASE}/health`);
      if (response.ok) return child;
    } catch {
      /* not listening yet */
    }
    await wait(250);
  }
  throw new Error(`server did not become healthy: ${child.kboardStderr.trim()}`);
}

function stopServer(server) {
  return new Promise((resolve) => {
    server.once("exit", resolve);
    server.kill("SIGKILL");
  });
}

/**
 * Perform a WebSocket handshake by hand and report only the status line.
 *
 * A WebSocket client reports a rejected upgrade as an opaque error event, which
 * cannot distinguish "refused" from "the server is not running" — precisely the
 * confusion a check like this must not make. The raw status can: 101 accepted,
 * 401 refused.
 */
function handshakeStatus(scope, token, extraHeaders = {}) {
  return new Promise((resolve, reject) => {
    const headers = {
      Connection: "Upgrade",
      Upgrade: "websocket",
      "Sec-WebSocket-Version": "13",
      "Sec-WebSocket-Key": randomBytes(16).toString("base64"),
      ...extraHeaders,
    };
    if (token !== undefined) headers["Sec-WebSocket-Protocol"] = `kboard.token.${token}`;

    const attempt = request(
      { host: "127.0.0.1", port: Number(PORT), path: `/ws/${scope}`, headers },
      (response) => {
        response.resume();
        resolve(response.statusCode);
      },
    );
    attempt.on("upgrade", (response, socket) => {
      socket.destroy();
      resolve(response.statusCode);
    });
    attempt.on("error", reject);
    attempt.end();
  });
}

/** Connect as a real client with a grant, and draw something. */
function drawWithToken(engine, token) {
  return new Promise((resolve, reject) => {
    const socket = new WebSocket(`ws://127.0.0.1:${PORT}/ws/${SCOPE}`, [
      "kboard.v1",
      `kboard.token.${token}`,
    ]);
    socket.onerror = () => reject(new Error("authorised client was refused"));
    socket.onmessage = (event) => {
      const message = JSON.parse(event.data);
      if (message.type !== "init") return;
      const board = engine.open(SCOPE, message.actor);
      engine.load(board, JSON.stringify(message.doc));
      engine.exec(board, { cmd: "add", kind: "rectangle", x: 5, y: 5, w: 50, h: 50, stroke: 0xff });
      socket.send(JSON.stringify({ type: "ops", ops: engine.pending(board) }));
      setTimeout(() => {
        socket.close();
        resolve();
      }, 400);
    };
  });
}

/** Report the accepted upgrade headers so absence of extension negotiation is provable. */
function handshakeResult(scope, token, extraHeaders = {}) {
  return new Promise((resolve, reject) => {
    const headers = {
      Connection: "Upgrade",
      Upgrade: "websocket",
      "Sec-WebSocket-Version": "13",
      "Sec-WebSocket-Key": randomBytes(16).toString("base64"),
      ...extraHeaders,
    };
    if (token !== undefined) headers["Sec-WebSocket-Protocol"] = `kboard.token.${token}`;
    const attempt = request(
      { host: "127.0.0.1", port: Number(PORT), path: `/ws/${scope}`, headers },
      (response) => {
        let body = "";
        response.on("data", (chunk) => (body += chunk));
        response.on("end", () =>
          resolve({ status: response.statusCode, headers: response.headers, body }),
        );
      },
    );
    attempt.on("upgrade", (response, socket) => {
      socket.destroy();
      resolve({ status: response.statusCode, headers: response.headers, body: "" });
    });
    attempt.on("error", reject);
    attempt.end();
  });
}

function closesAtExpiry(token) {
  return new Promise((resolve, reject) => {
    const socket = new WebSocket(`ws://127.0.0.1:${PORT}/ws/${SCOPE}`, [
      "kboard.v2",
      `kboard.token.${token}`,
    ]);
    const timeout = setTimeout(() => {
      socket.close();
      reject(new Error("session remained open beyond bearer expiry"));
    }, 5_000);
    socket.onerror = () => {};
    socket.onopen = () => {
      socket.send(
        JSON.stringify({
          type: "hello",
          version: 2,
          replica: randomBytes(16).toString("hex"),
        }),
      );
    };
    socket.onclose = () => {
      clearTimeout(timeout);
      resolve();
    };
  });
}

/** A v2 client presents capability and bearer subprotocols together. */
function negotiateV2WithToken(token) {
  return new Promise((resolve, reject) => {
    const socket = new WebSocket(`ws://127.0.0.1:${PORT}/ws/${SCOPE}`, [
      "kboard.v2",
      `kboard.token.${token}`,
    ]);
    socket.onerror = () => reject(new Error("authorised v2 client was refused"));
    socket.onopen = () => {
      socket.send(
        JSON.stringify({
          type: "hello",
          version: 2,
          replica: randomBytes(16).toString("hex"),
        }),
      );
    };
    socket.onmessage = (event) => {
      const message = JSON.parse(event.data);
      if (message.type !== "init") return;
      const result = { protocol: socket.protocol, version: message.version };
      socket.close();
      resolve(result);
    };
  });
}

console.log("k-board authentication check\n");

let server = null;
try {
  // 1. The configuration that must not start. An open server on loopback is a
  //    development convenience; an open server on any other interface is an
  //    unauthenticated writable store on a network.
  const exposed = await run([], { KBOARD_BIND: "0.0.0.0", PORT: "8097" });
  check("an open server refuses a public bind", exposed.code !== 0, `exit=${exposed.code}`);
  check(
    "and says why",
    /refusing to bind/.test(exposed.stderr),
    JSON.stringify(exposed.stderr.slice(0, 120)),
  );

  const loopback = await run(["--token", SCOPE], {});
  check(
    "minting without a secret fails rather than handing back a useless grant",
    loopback.code !== 0 && loopback.stdout.trim() === "",
    `exit=${loopback.code} stdout=${JSON.stringify(loopback.stdout)}`,
  );

  // 2. Mint the grants this check will present.
  const minted = await run(["--token", SCOPE], { KBOARD_SECRET: SECRET });
  const token = minted.stdout.trim();
  check("a secret mints a grant", minted.code === 0 && token.includes("."), token.slice(0, 40));

  const otherScope = (await run(["--token", "tenant-b:board"], { KBOARD_SECRET: SECRET })).stdout.trim();
  const otherSecret = (
    await run(["--token", SCOPE], { KBOARD_SECRET: "an entirely different secret" })
  ).stdout.trim();
  const shortToken = (
    await run(["--token", SCOPE, "--ttl", "2"], { KBOARD_SECRET: SECRET })
  ).stdout.trim();

  // 3. The claims that matter, against a running server.
  server = await startServer({ KBOARD_SECRET: SECRET });

  // Reported with the status, not just pass/fail: a refusal for the wrong
  // reason — a 400 from a malformed request, say — looks identical to a
  // refusal for the right one.
  const refuses = async (label, presented) => {
    const status = await handshakeStatus(SCOPE, presented);
    check(label, status === 401, `HTTP ${status}`);
  };

  await refuses("an unauthenticated connection is refused", undefined);
  await refuses("a grant for another scope is refused", otherScope);
  await refuses("a grant signed with another secret is refused", otherSecret);
  await refuses("a forged token is refused", "not.atoken");

  const admitted = await handshakeStatus(SCOPE, token);
  check("a valid grant is admitted", admitted === 101, `HTTP ${admitted}`);
  const tokenOnlyUpgrade = await handshakeResult(SCOPE, token);
  check(
    "the bearer is never echoed as a negotiated protocol",
    tokenOnlyUpgrade.status === 101 &&
      tokenOnlyUpgrade.headers["sec-websocket-protocol"] === undefined,
    JSON.stringify(tokenOnlyUpgrade),
  );

  const publicWithoutOrigins = await run([], {
    KBOARD_BIND: "0.0.0.0",
    KBOARD_SECRET: SECRET,
    PORT: "8096",
  });
  check(
    "an authenticated public server still requires an explicit Origin allowlist",
    publicWithoutOrigins.code !== 0 && /KBOARD_ALLOWED_ORIGINS/.test(publicWithoutOrigins.stderr),
    `exit=${publicWithoutOrigins.code}`,
  );
  const forgedRefusal = await handshakeResult(SCOPE, "not.atoken");
  check(
    "authentication errors never echo presented bearer data",
    forgedRefusal.status === 401 && !forgedRefusal.body.includes("not.atoken"),
    JSON.stringify(forgedRefusal),
  );
  const allowedOrigin = await handshakeStatus(SCOPE, token, {
    Origin: BASE,
  });
  check("the configured same-origin browser is admitted", allowedOrigin === 101, `HTTP ${allowedOrigin}`);
  const hostileOrigin = await handshakeStatus(SCOPE, token, {
    Origin: "https://hostile.example",
  });
  check("a hostile browser Origin is refused", hostileOrigin === 403, `HTTP ${hostileOrigin}`);
  const compressed = await handshakeResult(SCOPE, token, {
    "Sec-WebSocket-Extensions": "permessage-deflate",
  });
  check(
    "offered WebSocket compression is not negotiated",
    compressed.status === 101 && compressed.headers["sec-websocket-extensions"] === undefined,
    JSON.stringify(compressed),
  );

  // 4. Admitted is not the same as usable — the negotiated subprotocol has to
  //    be echoed back or the browser closes the connection it just opened.
  const engine = await loadEngine(`${BASE}/kboard.wasm`);
  await drawWithToken(engine, token);
  const negotiated = await negotiateV2WithToken(token);
  check(
    "an authorised v2 client negotiates capability without exposing its token",
    negotiated.protocol === "kboard.v2" && negotiated.version === 2,
    JSON.stringify(negotiated),
  );
  const stats = await (await fetch(`${BASE}/api/rooms/${SCOPE}/stats`)).json();
  check("an authorised client can actually draw", stats.elements === 1, JSON.stringify(stats));
  await closesAtExpiry(shortToken);
  check("an active session closes when its bearer expires", true);
  check(
    "server diagnostics contain neither grants nor raw scope names",
    ![token, otherScope, otherSecret, shortToken, SCOPE].some((secret) =>
      server.kboardStderr.includes(secret),
    ),
    JSON.stringify(server.kboardStderr.slice(0, 240)),
  );
} catch (error) {
  failures += 1;
  console.error(`  FAIL  ${error.message}`);
} finally {
  if (server) await stopServer(server);
  rmSync(workspace, { recursive: true, force: true });
}

console.log(failures === 0 ? "\nAll checks passed." : `\n${failures} check(s) failed.`);
process.exit(failures === 0 ? 0 : 1);
