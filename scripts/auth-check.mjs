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
const SCOPE = "tenant-a/board";
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

/** Run the server to completion, collecting what it said. */
function run(args, env) {
  return new Promise((resolve) => {
    // The parent's own KBOARD_* settings must not leak in, or a check that
    // depends on a secret being absent would silently pass for the wrong
    // reason.
    const child = spawn(BINARY, args, {
      env: { ...process.env, KBOARD_SECRET: "", KBOARD_BIND: "", ...env },
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
    env: { ...process.env, KBOARD_SECRET: "", KBOARD_BIND: "", PORT, KBOARD_DB: database, ...env },
    stdio: ["ignore", "pipe", "pipe"],
  });
  child.stdout.resume();
  child.stderr.resume();

  for (let attempt = 0; attempt < 60; attempt += 1) {
    try {
      const response = await fetch(`${BASE}/health`);
      if (response.ok) return child;
    } catch {
      /* not listening yet */
    }
    await wait(250);
  }
  throw new Error("server did not become healthy");
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
function handshakeStatus(scope, token) {
  return new Promise((resolve, reject) => {
    const headers = {
      Connection: "Upgrade",
      Upgrade: "websocket",
      "Sec-WebSocket-Version": "13",
      "Sec-WebSocket-Key": randomBytes(16).toString("base64"),
    };
    if (token !== undefined) headers["Sec-WebSocket-Protocol"] = `kboard.token.${token}`;

    const attempt = request(
      { host: "127.0.0.1", port: Number(PORT), path: `/ws/${encodeURIComponent(scope)}`, headers },
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
    const socket = new WebSocket(`ws://127.0.0.1:${PORT}/ws/${encodeURIComponent(SCOPE)}`, [
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

  const otherScope = (await run(["--token", "tenant-b/board"], { KBOARD_SECRET: SECRET })).stdout.trim();
  const otherSecret = (
    await run(["--token", SCOPE], { KBOARD_SECRET: "an entirely different secret" })
  ).stdout.trim();

  // 3. The claims that matter, against a running server.
  server = await startServer({ KBOARD_SECRET: SECRET });

  check("an unauthenticated connection is refused", (await handshakeStatus(SCOPE)) === 401);
  check("a grant for another scope is refused", (await handshakeStatus(SCOPE, otherScope)) === 401);
  check(
    "a grant signed with another secret is refused",
    (await handshakeStatus(SCOPE, otherSecret)) === 401,
  );
  check("a forged token is refused", (await handshakeStatus(SCOPE, "not.atoken")) === 401);
  check("a valid grant is admitted", (await handshakeStatus(SCOPE, token)) === 101);

  // 4. Admitted is not the same as usable — the negotiated subprotocol has to
  //    be echoed back or the browser closes the connection it just opened.
  const engine = await loadEngine(`${BASE}/kboard.wasm`);
  await drawWithToken(engine, token);
  const stats = await (await fetch(`${BASE}/api/rooms/${encodeURIComponent(SCOPE)}/stats`)).json();
  check("an authorised client can actually draw", stats.elements === 1, JSON.stringify(stats));
} catch (error) {
  failures += 1;
  console.error(`  FAIL  ${error.message}`);
} finally {
  if (server) await stopServer(server);
  rmSync(workspace, { recursive: true, force: true });
}

console.log(failures === 0 ? "\nAll checks passed." : `\n${failures} check(s) failed.`);
process.exit(failures === 0 ? 0 : 1);
