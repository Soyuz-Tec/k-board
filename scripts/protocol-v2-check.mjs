/**
 * Negotiated protocol-v2 and mixed-version end-to-end contract check.
 *
 * Run against a live server:
 *   node scripts/protocol-v2-check.mjs
 */

import { createHash, randomBytes } from "node:crypto";

const BASE = process.env.KBOARD_URL ?? "http://127.0.0.1:8080";
const SCOPE = process.env.KBOARD_SCOPE ?? `protocol-v2-${Date.now()}`;
const WS_URL = `${BASE.replace(/^http/, "ws")}/ws/${SCOPE}`;
const TIMEOUT_MS = 3_000;

let failures = 0;
function check(label, condition, detail = "") {
  if (condition) console.log(`  PASS  ${label}`);
  else {
    failures += 1;
    console.error(`  FAIL  ${label}${detail ? ` — ${detail}` : ""}`);
  }
}

function id() {
  return randomBytes(16).toString("hex");
}

function actorFor(scope, replica) {
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

function elementId(actor, counter) {
  return ((BigInt(actor) << 64n) | BigInt(counter)).toString(16).padStart(32, "0");
}

function operation(actor, counter, x, wall = Date.now()) {
  return {
    stamp: { wall, counter: 0, actor },
    op: {
      op: "set",
      element: elementId(actor, counter),
      key: "x",
      value: { t: "num", v: x },
    },
  };
}

function openClient({ replica = null } = {}) {
  const protocols = replica ? ["kboard.v2"] : undefined;
  const socket = new WebSocket(WS_URL, protocols);
  const queued = [];
  const waiters = [];

  socket.onmessage = (event) => {
    const message = JSON.parse(event.data);
    const index = waiters.findIndex(({ predicate }) => predicate(message));
    if (index >= 0) {
      const [{ resolve, timer }] = waiters.splice(index, 1);
      clearTimeout(timer);
      resolve(message);
    } else queued.push(message);
  };

  const opened = new Promise((resolve, reject) => {
    socket.onopen = () => {
      if (replica) socket.send(JSON.stringify({ type: "hello", version: 2, replica }));
      resolve();
    };
    socket.onerror = () => reject(new Error("WebSocket connection failed"));
  });

  function next(predicate = () => true, timeout = TIMEOUT_MS) {
    const index = queued.findIndex(predicate);
    if (index >= 0) return Promise.resolve(queued.splice(index, 1)[0]);
    return new Promise((resolve, reject) => {
      const waiter = { predicate, resolve, timer: null };
      waiter.timer = setTimeout(() => {
        const position = waiters.indexOf(waiter);
        if (position >= 0) waiters.splice(position, 1);
        reject(new Error("timed out waiting for protocol message"));
      }, timeout);
      waiters.push(waiter);
    });
  }

  function close() {
    if (socket.readyState >= WebSocket.CLOSING) return Promise.resolve();
    return new Promise((resolve) => {
      socket.addEventListener("close", resolve, { once: true });
      socket.close();
    });
  }

  return { socket, opened, next, close, queued };
}

console.log(`k-board protocol-v2 check against ${BASE} (scope ${SCOPE})\n`);

const legacy = openClient();
await legacy.opened;
const legacyInit = await legacy.next((message) => message.type === "init");

const replica = id();
const modern = openClient({ replica });
await modern.opened;
const modernInit = await modern.next((message) => message.type === "init");
const actor = actorFor(SCOPE, replica);
check("v2 subprotocol is negotiated", modern.socket.protocol === "kboard.v2");
check("v1 remains unversioned", legacyInit.version === undefined);
check(
  "v2 init binds the deterministic replica actor",
  modernInit.version === 2 && modernInit.replica === replica && modernInit.actor === actor,
  JSON.stringify(modernInit),
);

const clone = openClient({ replica });
await clone.opened;
const cloneRefusal = await clone.next((message) => message.type === "refused");
check(
  "one active context owns a scope replica",
  cloneRefusal.code === "replica_in_use" && cloneRefusal.retryable === true,
  JSON.stringify(cloneRefusal),
);
await clone.close();

const forgedBatch = id();
modern.socket.send(
  JSON.stringify({ type: "ops", batch: forgedBatch, ops: [operation(actor + 1, 1, 10)] }),
);
const forged = await modern.next((message) => message.batch === forgedBatch);
check("forged operation actors are refused", forged.code === "actor_mismatch");

const futureBatch = id();
modern.socket.send(
  JSON.stringify({
    type: "ops",
    batch: futureBatch,
    ops: [operation(actor, 2, 20, Date.now() + 5 * 60 * 1_000 + 5_000)],
  }),
);
const future = await modern.next((message) => message.batch === futureBatch);
check("unbounded future clocks are refused", future.code === "clock_skew");

const batch = id();
const ops = [operation(actor, 3, 30)];
modern.socket.send(JSON.stringify({ type: "ops", batch, ops }));
const [ack, relayed] = await Promise.all([
  modern.next((message) => message.type === "ack" && message.batch === batch),
  legacy.next((message) => message.type === "ops"),
]);
check("accepted v2 batches receive a durable sequence", Number.isInteger(ack.sequence));
check("a v1 peer receives a v2-originated operation", relayed.ops?.length === 1);

modern.socket.send(JSON.stringify({ type: "ops", batch, ops }));
const duplicateAck = await modern.next(
  (message) => message.type === "ack" && message.batch === batch,
);
await new Promise((resolve) => setTimeout(resolve, 250));
check("duplicate retry returns the original sequence", duplicateAck.sequence === ack.sequence);
check(
  "duplicate retry is not rebroadcast",
  !legacy.queued.some((message) => message.type === "ops"),
);

modern.socket.send(
  JSON.stringify({ type: "ops", batch, ops: [operation(actor, 4, 40)] }),
);
const conflict = await modern.next((message) => message.batch === batch);
check("batch identity reuse with changed content is refused", conflict.code === "batch_conflict");

const additiveBatch = id();
modern.socket.send(
  JSON.stringify({
    type: "ops",
    batch: additiveBatch,
    ops: [operation(actor, 5, 50)],
    future_additive_field: true,
  }),
);
const additiveAck = await modern.next((message) => message.batch === additiveBatch);
await legacy.next((message) => message.type === "ops");
check("unknown additive fields remain forward-compatible", additiveAck.type === "ack");

modern.socket.send(JSON.stringify({ type: "future_message", payload: true }));
const unknown = await modern.next(
  (message) => message.type === "refused" && message.code === "protocol",
);
check("unknown message types receive a typed protocol refusal", unknown.retryable === false);

const legacyOp = operation(legacyInit.actor, 1, 60);
legacy.socket.send(JSON.stringify({ type: "ops", ops: [legacyOp] }));
const mixed = await modern.next((message) => message.type === "ops");
check(
  "a v2 peer receives versioned events from a v1 origin",
  mixed.version === 2 && Number.isInteger(mixed.sequence),
  JSON.stringify(mixed),
);

const lostAckBatch = id();
const lostAckOps = [operation(actor, 6, 70)];
modern.socket.send(JSON.stringify({ type: "ops", batch: lostAckBatch, ops: lostAckOps }));
await legacy.next((message) => message.type === "ops");
// Peer delivery proves the append committed. Deliberately close without
// consuming the sender acknowledgement, reproducing commit-before-ack loss.
await modern.close();
const reconnected = openClient({ replica });
await reconnected.opened;
const reconnectedInit = await reconnected.next((message) => message.type === "init");
check("reconnect preserves the scope replica actor", reconnectedInit.actor === actor);
reconnected.socket.send(
  JSON.stringify({ type: "ops", batch: lostAckBatch, ops: lostAckOps }),
);
const recoveredAck = await reconnected.next(
  (message) => message.type === "ack" && message.batch === lostAckBatch,
);
await new Promise((resolve) => setTimeout(resolve, 250));
check(
  "commit-before-ack retry recovers the original durable outcome",
  Number.isInteger(recoveredAck.sequence),
);
check(
  "commit-before-ack retry is not rebroadcast",
  !legacy.queued.some((message) => message.type === "ops"),
);

await Promise.all([legacy.close(), reconnected.close()]);
if (failures > 0) {
  console.error(`\n${failures} protocol-v2 check(s) failed`);
  process.exit(1);
}
console.log("\nprotocol-v2 and mixed-version contract holds");
