import { createHash, randomBytes } from "node:crypto";
import { performance } from "node:perf_hooks";

const BASE = process.env.KBOARD_URL ?? "http://127.0.0.1:8080";
const RUN = process.env.KBOARD_SCOPE ?? `room-cell-slo-${Date.now()}`;
const SCOPES = Number(process.env.KBOARD_SLO_SCOPES ?? 8);
const COLD_COMMANDS = Number(process.env.KBOARD_SLO_COLD_COMMANDS ?? 40);
const HOT_COMMANDS = Number(process.env.KBOARD_SLO_HOT_COMMANDS ?? 160);
const HOT_PACING_MS = Number(process.env.KBOARD_SLO_HOT_PACING_MS ?? 20);
const P99_LIMIT_MS = Number(process.env.KBOARD_SLO_P99_LIMIT_MS ?? 250);
const MAX_LIMIT_MS = Number(process.env.KBOARD_SLO_MAX_LIMIT_MS ?? 1_000);

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

function percentile(samples, value) {
  const ordered = [...samples].sort((left, right) => left - right);
  return ordered[Math.min(ordered.length - 1, Math.ceil(ordered.length * value) - 1)];
}

async function connect(index) {
  const scope = `${RUN}-${index}`;
  const replica = randomBytes(16).toString("hex");
  const actor = actorFor(scope, replica);
  const socket = new WebSocket(`${BASE.replace(/^http/, "ws")}/ws/${scope}`, ["kboard.v2"]);
  const acks = new Map();
  let sequence = 0;

  await new Promise((resolve, reject) => {
    socket.onopen = () => socket.send(JSON.stringify({ type: "hello", version: 2, replica }));
    socket.onerror = () => reject(new Error(`scope ${index} failed to connect`));
    socket.onmessage = (event) => {
      const message = JSON.parse(event.data);
      if (message.type === "init") resolve();
      if (message.type === "ack" || message.type === "refused") {
        acks.get(message.batch)?.(message);
      }
    };
  });

  async function commit(counter) {
    const batch = randomBytes(16).toString("hex");
    const element = ((BigInt(actor) << 64n) | BigInt(counter + 1))
      .toString(16)
      .padStart(32, "0");
    const operation = {
      stamp: { wall: Date.now(), counter: sequence++, actor },
      op: { op: "set", element, key: "x", value: { t: "num", v: counter } },
    };
    const started = performance.now();
    const reply = new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`scope ${index} ack timeout`)), 2_000);
      acks.set(batch, (message) => {
        clearTimeout(timer);
        acks.delete(batch);
        resolve(message);
      });
    });
    socket.send(JSON.stringify({ type: "ops", batch, ops: [operation] }));
    const outcome = await reply;
    if (outcome.type !== "ack") {
      throw new Error(`scope ${index} refused ${outcome.code}`);
    }
    return performance.now() - started;
  }

  return { index, scope, socket, commit };
}

const clients = await Promise.all(Array.from({ length: SCOPES }, (_, index) => connect(index)));
const samples = await Promise.all(
  clients.map(async (client) => {
    const count = client.index === 0 ? HOT_COMMANDS : COLD_COMMANDS;
    const timings = [];
    for (let index = 0; index < count; index += 1) {
      timings.push(await client.commit(index));
      if (client.index === 0 && HOT_PACING_MS > 0) {
        await new Promise((resolve) => setTimeout(resolve, HOT_PACING_MS));
      }
    }
    return timings;
  }),
);
for (const client of clients) client.socket.close();

const perScope = samples.map((timings, index) => ({
  scope: index === 0 ? "hot" : `cold-${index}`,
  commands: timings.length,
  medianMs: percentile(timings, 0.5),
  p99Ms: percentile(timings, 0.99),
  maxMs: Math.max(...timings),
}));
const coldSamples = samples.slice(1).flat();
const result = {
  workload: { scopes: SCOPES, hotCommands: HOT_COMMANDS, coldCommandsPerScope: COLD_COMMANDS },
  objective: { crossScopeP99Ms: P99_LIMIT_MS, commandMaxMs: MAX_LIMIT_MS },
  crossScope: {
    coldP99Ms: percentile(coldSamples, 0.99),
    coldMaxMs: Math.max(...coldSamples),
  },
  perScope,
};
console.log(JSON.stringify(result, null, 2));

if (
  result.crossScope.coldP99Ms > P99_LIMIT_MS ||
  perScope.some((scope) => scope.maxMs > MAX_LIMIT_MS)
) {
  throw new Error("room-cell latency objective exceeded");
}
