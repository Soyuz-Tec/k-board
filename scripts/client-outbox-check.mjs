/** Deterministic durable-client recovery contract, without a browser server. */

import { webcrypto } from "node:crypto";
import {
  MAX_OUTBOX_OPERATIONS,
  MemoryStore,
  OutboxLimitError,
  createClientOutbox,
} from "../web/outbox.js";

class SessionStore {
  values = new Map();
  getItem(key) {
    return this.values.get(key) ?? null;
  }
  setItem(key, value) {
    this.values.set(key, value);
  }
}

class QuotaStore extends MemoryStore {
  async commit() {
    throw new DOMException("quota exhausted", "QuotaExceededError");
  }
}

const operation = (index) => ({
  stamp: { wall: index + 1, counter: 0, actor: 1 },
  op: { op: "set", element: String(index).padStart(32, "0"), key: "x", value: { t: "num", v: index } },
});

let failures = 0;
function check(label, condition, detail = "") {
  if (condition) console.log(`  PASS  ${label}`);
  else {
    failures += 1;
    console.error(`  FAIL  ${label}${detail ? ` — ${detail}` : ""}`);
  }
}

console.log("k-board durable client outbox check\n");

const store = new MemoryStore({ durable: true });
const session = new SessionStore();
const first = await createClientOutbox("t:board", { store, sessionStorage: session, crypto: webcrypto });
const [batch] = await first.outbox.capture([operation(1)]);
await first.outbox.markSending(batch.id);
check("socket send retains the batch", first.outbox.snapshot().sending === 1);

const reloaded = await createClientOutbox("t:board", {
  store,
  sessionStorage: session,
  crypto: webcrypto,
});
check("reload restores an outcome-unknown batch", reloaded.outbox.snapshot().pending === 1);
check("reload preserves the original batch id", reloaded.outbox.nextReady().id === batch.id);
check("reload preserves the stable replica", reloaded.replica === first.replica);
check("the stable replica has a deterministic actor", reloaded.actor === first.actor && first.actor > 0);

await reloaded.outbox.markSending(batch.id);
await reloaded.outbox.resetSending();
check(
  "close before commit retains the original batch for retry",
  reloaded.outbox.snapshot().pending === 1 && reloaded.outbox.nextReady().id === batch.id,
);
check(
  "commit followed by a lost ack resolves through the original batch id",
  (await reloaded.outbox.acknowledge(batch.id, 7)) === "durable",
);
check("duplicate or expired acknowledgements are harmless", (await reloaded.outbox.acknowledge(batch.id, 7)) === "unknown");

const closeBeforeAck = (await reloaded.outbox.capture([operation(11)]))[0];
await reloaded.outbox.markSending(closeBeforeAck.id);
await reloaded.outbox.resetSending();
check(
  "close before acknowledgement returns outcome-unknown work to pending",
  reloaded.outbox.nextReady().id === closeBeforeAck.id,
);
await reloaded.outbox.acknowledge(closeBeforeAck.id, 8);

const retryBatch = (await reloaded.outbox.capture([operation(2)]))[0];
await reloaded.outbox.markSending(retryBatch.id);
check(
  "overload schedules bounded retry instead of busy retry",
  (await reloaded.outbox.refuse(retryBatch.id, "overloaded", true, 1_000)) === "retrying" &&
    reloaded.outbox.exportData().batches.find((candidate) => candidate.id === retryBatch.id).retryAt >
      1_000,
);
const permanentBatch = (await reloaded.outbox.capture([operation(3)]))[0];
check(
  "permanent refusal is retained for recovery export",
  (await reloaded.outbox.refuse(permanentBatch.id, "invalid_operation", false)) === "permanent" &&
    reloaded.outbox.exportData().batches.some((candidate) => candidate.id === permanentBatch.id),
);

const otherTab = await createClientOutbox("t:board", {
  store,
  sessionStorage: new SessionStore(),
  crypto: webcrypto,
});
check("two tabs receive distinct stable replicas", otherTab.replica !== first.replica);

const bounded = await createClientOutbox("t:bounded", {
  store: new MemoryStore({ durable: true }),
  sessionStorage: new SessionStore(),
  crypto: webcrypto,
});
let boundedFailure = false;
try {
  await bounded.outbox.capture(Array.from({ length: MAX_OUTBOX_OPERATIONS + 1 }, (_, index) => operation(index)));
} catch (error) {
  boundedFailure = error instanceof OutboxLimitError;
}
check("operation capacity fails atomically", boundedFailure && bounded.outbox.snapshot().batches === 0);

const quota = await createClientOutbox("t:quota", {
  store: new QuotaStore({ durable: true }),
  sessionStorage: new SessionStore(),
  crypto: webcrypto,
});
let quotaFailure = false;
try {
  await quota.outbox.capture([operation(9)]);
} catch (error) {
  quotaFailure = error.name === "QuotaExceededError";
}
check("storage quota failure retains no false durable record", quotaFailure && quota.outbox.snapshot().batches === 0);

const privateMode = await createClientOutbox("t:private", {
  indexedDB: null,
  sessionStorage: null,
  crypto: webcrypto,
});
check(
  "private or unavailable storage is explicit",
  !privateMode.outbox.snapshot().durableStorage && Boolean(privateMode.outbox.snapshot().limitation),
);

if (failures > 0) {
  console.error(`\n${failures} client outbox check(s) failed`);
  process.exit(1);
}
console.log("\nAll durable client outbox checks passed.");
