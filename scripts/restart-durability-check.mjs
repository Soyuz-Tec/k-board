/**
 * Proves a board survives the process that was holding it.
 *
 * The unit tests prove the store round-trips. This proves the whole path: a
 * client draws over a WebSocket, the server is killed, a new server opens the
 * same database, and the board is still there.
 *
 * That is the defect this work exists to fix — "restarting the server loses
 * every board" — and it cannot be demonstrated by anything short of an actual
 * restart.
 *
 *   node scripts/restart-durability-check.mjs
 *
 * Exits non-zero on failure so CI can gate on it.
 */

import { spawn } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { loadEngine } from "../web/kboard.js";

const PORT = process.env.KBOARD_PORT ?? "8099";
const BASE = `http://127.0.0.1:${PORT}`;
const SCOPE = "restart-check";
const BINARY =
  process.env.KBOARD_BIN ??
  (process.platform === "win32"
    ? "target/release/kboard-server.exe"
    : "target/release/kboard-server");

const workspace = mkdtempSync(join(tmpdir(), "kboard-restart-"));
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

async function startServer() {
  const server = spawn(BINARY, [], {
    env: { ...process.env, PORT, KBOARD_DB: database },
    stdio: ["ignore", "pipe", "pipe"],
  });
  server.stdout.resume();
  server.stderr.resume();

  for (let attempt = 0; attempt < 60; attempt += 1) {
    try {
      const response = await fetch(`${BASE}/health`);
      if (response.ok) return server;
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

/** Draw one rectangle through a real client session. */
async function drawShape(engine) {
  return new Promise((resolve, reject) => {
    const socket = new WebSocket(`${BASE.replace(/^http/, "ws")}/ws/${SCOPE}`);
    socket.onerror = () => reject(new Error("client socket error"));
    socket.onmessage = (event) => {
      const message = JSON.parse(event.data);
      if (message.type !== "init") return;

      const board = engine.open(SCOPE, message.actor);
      engine.load(board, JSON.stringify(message.doc));
      engine.exec(board, {
        cmd: "add",
        kind: "rectangle",
        x: 10,
        y: 20,
        w: 100,
        h: 60,
        stroke: 0x1e1e1eff,
      });
      const ops = engine.pending(board);
      socket.send(JSON.stringify({ type: "ops", ops }));

      // The server persists inside the same critical section that accepts, so
      // once it has processed this frame the write has already landed.
      setTimeout(() => {
        socket.close();
        resolve(ops.length);
      }, 400);
    };
  });
}

console.log(`k-board restart durability check (${database})\n`);

try {
  const first = await startServer();
  const engine = await loadEngine(`${BASE}/kboard.wasm`);

  const written = await drawShape(engine);
  check("a client drew and the operations were sent", written > 0);

  let stats = await (await fetch(`${BASE}/api/rooms/${SCOPE}/stats`)).json();
  check("the live server holds the shape", stats.elements === 1, JSON.stringify(stats));

  await stopServer(first);
  check("the server process is gone", true);

  // A fresh process, the same database. Nothing is carried over in memory.
  const second = await startServer();
  const engineAfter = await loadEngine(`${BASE}/kboard.wasm`);

  // Stats never restore a room — only a join does — so connect first, the way
  // a returning user would.
  await new Promise((resolve, reject) => {
    const socket = new WebSocket(`${BASE.replace(/^http/, "ws")}/ws/${SCOPE}`);
    socket.onerror = () => reject(new Error("rejoin socket error"));
    socket.onmessage = (event) => {
      const message = JSON.parse(event.data);
      if (message.type !== "init") return;
      const board = engineAfter.open(SCOPE, message.actor);
      engineAfter.load(board, JSON.stringify(message.doc));
      const scene = engineAfter.scene(board);

      check(
        "the restarted server served the board back",
        scene.length === 1 && scene[0].kind === "rectangle",
        JSON.stringify(scene),
      );
      check(
        "the shape kept its geometry",
        scene[0]?.x === 10 && scene[0]?.w === 100,
        JSON.stringify(scene[0]),
      );
      socket.close();
      resolve();
    };
  });

  stats = await (await fetch(`${BASE}/api/rooms/${SCOPE}/stats`)).json();
  check("the restored room reports the element", stats.elements === 1, JSON.stringify(stats));

  await stopServer(second);
} catch (error) {
  failures += 1;
  console.error(`  FAIL  ${error.message}`);
} finally {
  rmSync(workspace, { recursive: true, force: true });
}

console.log(failures === 0 ? "\nAll checks passed." : `\n${failures} check(s) failed.`);
process.exit(failures === 0 ? 0 : 1);
