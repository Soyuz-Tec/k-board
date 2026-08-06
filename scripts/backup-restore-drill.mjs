import { createHash, randomBytes } from "node:crypto";
import { spawn, spawnSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { performance } from "node:perf_hooks";

import { loadEngine } from "../web/kboard.js";

const PORT = process.env.KBOARD_DRILL_PORT ?? "8101";
const BASE = `http://127.0.0.1:${PORT}`;
const BINARY =
  process.env.KBOARD_BIN ??
  (process.platform === "win32"
    ? "target/release/kboard-server.exe"
    : "target/release/kboard-server");
const work = mkdtempSync(join(tmpdir(), "kboard-recovery-drill-"));
const source = join(work, "source.sqlite3");
const backup = join(work, "backup.sqlite3");
const scope = "recovery-game-day";
const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function actorFor(replica) {
  const digest = createHash("sha256")
    .update("kboard-replica-v2\0")
    .update(scope)
    .update("\0")
    .update(replica)
    .digest();
  let actor = 0n;
  for (const byte of digest.subarray(0, 7)) actor = (actor << 8n) | BigInt(byte);
  actor &= (1n << 53n) - 1n;
  return Number(actor === 0n ? 1n : actor);
}

async function start(database) {
  const process = spawn(BINARY, [], {
    env: { ...globalThis.process.env, PORT, KBOARD_DB: database },
    stdio: ["ignore", "pipe", "pipe"],
  });
  process.stdout.resume();
  process.stderr.resume();
  for (let attempt = 0; attempt < 80; attempt += 1) {
    try {
      if ((await fetch(`${BASE}/health/ready`)).ok) return process;
    } catch {
      // The process has not bound yet.
    }
    await wait(100);
  }
  throw new Error("recovery drill server did not become ready");
}

function stop(process) {
  return new Promise((resolve) => {
    process.once("exit", resolve);
    process.kill("SIGKILL");
  });
}

async function commitOne() {
  const replica = randomBytes(16).toString("hex");
  const batch = randomBytes(16).toString("hex");
  const actor = actorFor(replica);
  const element = ((BigInt(actor) << 64n) | 1n).toString(16).padStart(32, "0");
  return new Promise((resolve, reject) => {
    const socket = new WebSocket(`${BASE.replace(/^http/, "ws")}/ws/${scope}`, ["kboard.v2"]);
    const timer = setTimeout(() => reject(new Error("durable acknowledgement timed out")), 4_000);
    socket.onopen = () => socket.send(JSON.stringify({ type: "hello", version: 2, replica }));
    socket.onerror = () => reject(new Error("recovery drill socket failed"));
    socket.onmessage = (event) => {
      const message = JSON.parse(event.data);
      if (message.type === "init") {
        socket.send(
          JSON.stringify({
            type: "ops",
            batch,
            ops: [
              {
                stamp: { wall: Date.now(), counter: 0, actor },
                op: { op: "set", element, key: "x", value: { t: "num", v: 42 } },
              },
            ],
          }),
        );
      } else if (message.type === "ack" && message.batch === batch) {
        clearTimeout(timer);
        socket.close();
        resolve({ sequence: message.sequence, acknowledgedAt: Date.now() });
      }
    };
  });
}

async function restoredScene() {
  const engine = await loadEngine(`${BASE}/kboard.wasm`);
  return new Promise((resolve, reject) => {
    const socket = new WebSocket(`${BASE.replace(/^http/, "ws")}/ws/${scope}`);
    socket.onerror = () => reject(new Error("restored room socket failed"));
    socket.onmessage = (event) => {
      const message = JSON.parse(event.data);
      if (message.type !== "init") return;
      const board = engine.open(scope, message.actor);
      engine.load(board, JSON.stringify(message.doc));
      const scene = engine.scene(board);
      socket.close();
      resolve(scene);
    };
  });
}

let active;
try {
  console.log(`k-board backup/restore game day (${work})\n`);
  active = await start(source);
  const durable = await commitOne();
  const backupStartedAt = Date.now();
  const backupStarted = performance.now();
  const command = spawnSync(BINARY, ["--backup", backup], {
    env: { ...process.env, KBOARD_DB: source },
    encoding: "utf8",
  });
  if (command.status !== 0) throw new Error(command.stderr || "backup command failed");
  const evidence = JSON.parse(command.stdout);
  const backupDurationMs = performance.now() - backupStarted;
  await stop(active);
  active = undefined;

  const recoveryStarted = performance.now();
  active = await start(backup);
  const scene = await restoredScene();
  const recoveryTimeMs = performance.now() - recoveryStarted;
  const report = {
    objective: { acknowledgedRpoOperations: 0, recoveryTimeMs: 30_000 },
    observed: {
      acknowledgedRpoOperations: scene.length === 1 && durable.acknowledgedAt <= backupStartedAt ? 0 : 1,
      recoveryTimeMs,
      backupDurationMs,
      durableSequence: durable.sequence,
      backupBytes: evidence.backup.bytes,
      verifiedScopes: evidence.isolated_restore.scopes,
    },
  };
  console.log(JSON.stringify(report, null, 2));
  if (
    report.observed.acknowledgedRpoOperations !== 0 ||
    report.observed.recoveryTimeMs > report.objective.recoveryTimeMs
  ) {
    throw new Error("recovery objective missed");
  }
  await stop(active);
  active = undefined;
} finally {
  if (active) await stop(active);
  rmSync(work, { recursive: true, force: true });
}
