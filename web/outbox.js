/** Durable protocol-v2 client identity and acknowledgement-retained outbox. */

export const MAX_OUTBOX_BYTES = 8 * 1024 * 1024;
export const MAX_OUTBOX_OPERATIONS = 4_096;
export const MAX_BATCH_OPERATIONS = 512;
export const MAX_RETRY_DELAY_MS = 30_000;

const DATABASE = "kboard-client-v1";
const DATABASE_VERSION = 1;
const TAB_KEY = "kboard.tab.v1";
const PERMANENT_CODES = new Set([
  "actor_mismatch",
  "batch_conflict",
  "batch_too_large",
  "clock_skew",
  "invalid_batch",
  "invalid_operation",
  "protocol",
  "room_full",
]);

export class OutboxLimitError extends Error {
  constructor(message) {
    super(message);
    this.name = "OutboxLimitError";
  }
}

function clone(value) {
  return JSON.parse(JSON.stringify(value));
}

function randomHex(source) {
  const bytes = new Uint8Array(16);
  source.getRandomValues(bytes);
  return [...bytes].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

function recordBytes(record) {
  return new TextEncoder().encode(JSON.stringify(record.operations)).length;
}

function retryDelay(attempts, random = Math.random) {
  const ceiling = Math.min(MAX_RETRY_DELAY_MS, 500 * 2 ** Math.min(attempts, 6));
  return Math.floor(ceiling / 2 + random() * (ceiling / 2));
}

function requestResult(request) {
  return new Promise((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error ?? new Error("IndexedDB request failed"));
  });
}

function transactionDone(transaction) {
  return new Promise((resolve, reject) => {
    transaction.oncomplete = () => resolve();
    transaction.onabort = () => reject(transaction.error ?? new Error("IndexedDB transaction aborted"));
    transaction.onerror = () => reject(transaction.error ?? new Error("IndexedDB transaction failed"));
  });
}

export class IndexedDbStore {
  constructor(database) {
    this.database = database;
    this.durable = true;
    this.limitation = null;
  }

  static async open(factory) {
    if (!factory) throw new Error("IndexedDB is unavailable");
    const request = factory.open(DATABASE, DATABASE_VERSION);
    request.onupgradeneeded = () => {
      const database = request.result;
      if (!database.objectStoreNames.contains("meta")) database.createObjectStore("meta");
      if (!database.objectStoreNames.contains("batches")) {
        const batches = database.createObjectStore("batches", { keyPath: "id" });
        batches.createIndex("owner", ["scope", "replica"]);
      }
    };
    return new IndexedDbStore(await requestResult(request));
  }

  async getMeta(key) {
    const transaction = this.database.transaction("meta", "readonly");
    return requestResult(transaction.objectStore("meta").get(key));
  }

  async putMeta(key, value) {
    const transaction = this.database.transaction("meta", "readwrite");
    transaction.objectStore("meta").put(value, key);
    await transactionDone(transaction);
  }

  async list(scope, replica) {
    const transaction = this.database.transaction("batches", "readonly");
    const rows = await requestResult(
      transaction.objectStore("batches").index("owner").getAll([scope, replica]),
    );
    return rows.sort((left, right) => left.createdAt - right.createdAt || left.id.localeCompare(right.id));
  }

  async commit(upserts, removals) {
    const transaction = this.database.transaction("batches", "readwrite");
    const batches = transaction.objectStore("batches");
    for (const id of removals) batches.delete(id);
    for (const record of upserts) batches.put(record);
    await transactionDone(transaction);
  }
}

/** Deterministic adapter for tests and the explicit non-durable fallback. */
export class MemoryStore {
  constructor({ durable = false, limitation = "browser storage unavailable" } = {}) {
    this.meta = new Map();
    this.batches = new Map();
    this.durable = durable;
    this.limitation = durable ? null : limitation;
  }

  async getMeta(key) {
    return this.meta.get(key);
  }

  async putMeta(key, value) {
    this.meta.set(key, value);
  }

  async list(scope, replica) {
    return [...this.batches.values()]
      .filter((record) => record.scope === scope && record.replica === replica)
      .sort((left, right) => left.createdAt - right.createdAt || left.id.localeCompare(right.id))
      .map(clone);
  }

  async commit(upserts, removals) {
    for (const id of removals) this.batches.delete(id);
    for (const record of upserts) this.batches.set(record.id, clone(record));
  }
}

export class DurableOutbox {
  constructor(scope, replica, store, source, random = Math.random) {
    this.scope = scope;
    this.replica = replica;
    this.store = store;
    this.source = source;
    this.random = random;
    this.records = [];
  }

  async load() {
    this.records = await this.store.list(this.scope, this.replica);
    let changed = false;
    for (const record of this.records) {
      if (record.state === "sending") {
        record.state = "pending";
        record.lastError = "delivery outcome unknown after reload";
        record.retryAt = 0;
        changed = true;
      }
    }
    if (changed) await this.store.commit(this.records, []);
    return this.snapshot();
  }

  async capture(operations, now = Date.now()) {
    if (operations.length === 0) return [];
    const additions = [];
    for (let offset = 0; offset < operations.length; offset += MAX_BATCH_OPERATIONS) {
      const batchOperations = operations.slice(offset, offset + MAX_BATCH_OPERATIONS);
      const record = {
        id: randomHex(this.source),
        scope: this.scope,
        replica: this.replica,
        operations: batchOperations,
        state: "pending",
        attempts: 0,
        retryAt: 0,
        lastError: null,
        sequence: null,
        createdAt: now + offset,
        bytes: 0,
      };
      record.bytes = recordBytes(record);
      additions.push(record);
    }
    const records = [...this.records, ...additions];
    const operationsTotal = records.reduce((total, record) => total + record.operations.length, 0);
    const bytesTotal = records.reduce((total, record) => total + record.bytes, 0);
    if (operationsTotal > MAX_OUTBOX_OPERATIONS || bytesTotal > MAX_OUTBOX_BYTES) {
      throw new OutboxLimitError(
        `offline queue limit reached (${operationsTotal} operations, ${bytesTotal} bytes)`,
      );
    }
    await this.store.commit(additions, []);
    this.records = records;
    return additions.map(clone);
  }

  nextReady(now = Date.now()) {
    return this.records.find(
      (record) => record.state === "pending" && (record.retryAt ?? 0) <= now,
    );
  }

  nextRetryAt() {
    const retries = this.records
      .filter((record) => record.state === "pending" && record.retryAt > Date.now())
      .map((record) => record.retryAt);
    return retries.length === 0 ? null : Math.min(...retries);
  }

  async markSending(id) {
    const record = this.records.find((candidate) => candidate.id === id);
    if (!record || record.state === "refused") return false;
    record.state = "sending";
    record.attempts += 1;
    record.lastError = null;
    await this.store.commit([record], []);
    return true;
  }

  async acknowledge(id, sequence) {
    const index = this.records.findIndex((record) => record.id === id);
    if (index < 0) return "unknown";
    await this.store.commit([], [id]);
    this.records.splice(index, 1);
    return Number.isSafeInteger(sequence) ? "durable" : "durable_without_sequence";
  }

  async refuse(id, code, retryable, now = Date.now()) {
    const record = this.records.find((candidate) => candidate.id === id);
    if (!record) return "unknown";
    const permanent = !retryable || PERMANENT_CODES.has(code);
    record.state = permanent ? "refused" : "pending";
    record.lastError = code;
    record.retryAt = permanent ? 0 : now + retryDelay(record.attempts, this.random);
    await this.store.commit([record], []);
    return permanent ? "permanent" : "retrying";
  }

  async resetSending(reason = "connection closed before durable acknowledgement") {
    const changed = this.records.filter((record) => record.state === "sending");
    for (const record of changed) {
      record.state = "pending";
      record.lastError = reason;
      record.retryAt = 0;
    }
    if (changed.length > 0) await this.store.commit(changed, []);
    return changed.length;
  }

  allOperations() {
    return this.records.flatMap((record) => record.operations).map(clone);
  }

  snapshot() {
    const states = { pending: 0, sending: 0, refused: 0 };
    let bytes = 0;
    let operations = 0;
    for (const record of this.records) {
      states[record.state] += 1;
      bytes += record.bytes;
      operations += record.operations.length;
    }
    return {
      ...states,
      batches: this.records.length,
      bytes,
      operations,
      durableStorage: this.store.durable,
      limitation: this.store.limitation,
    };
  }

  exportData() {
    return {
      format: "kboard-recovery/v1",
      exportedAt: new Date().toISOString(),
      scope: this.scope,
      replica: this.replica,
      limits: { bytes: MAX_OUTBOX_BYTES, operations: MAX_OUTBOX_OPERATIONS },
      batches: this.records.map(clone),
    };
  }
}

function tabIdentity(storage, source) {
  try {
    let value = storage?.getItem(TAB_KEY);
    if (!value) {
      value = randomHex(source);
      storage?.setItem(TAB_KEY, value);
    }
    return { value, stable: Boolean(storage) };
  } catch {
    return { value: randomHex(source), stable: false };
  }
}

export async function actorForReplica(scope, replica, source = globalThis.crypto) {
  const prefix = new TextEncoder().encode(`kboard-replica-v2\0${scope}\0${replica}`);
  const digest = new Uint8Array(await source.subtle.digest("SHA-256", prefix));
  let actor = 0n;
  for (const byte of digest.subarray(0, 7)) actor = (actor << 8n) | BigInt(byte);
  actor &= (1n << 53n) - 1n;
  return Number(actor === 0n ? 1n : actor);
}

export async function createClientOutbox(
  scope,
  {
    indexedDB = globalThis.indexedDB,
    sessionStorage = globalThis.sessionStorage,
    crypto = globalThis.crypto,
    store = null,
    random = Math.random,
  } = {},
) {
  const tab = tabIdentity(sessionStorage, crypto);
  let selected = store;
  if (!selected) {
    try {
      selected = await IndexedDbStore.open(indexedDB);
    } catch (error) {
      selected = new MemoryStore({ limitation: error?.message ?? "browser storage unavailable" });
    }
  }
  const replicaKey = `replica:${scope}:${tab.value}`;
  let replica = await selected.getMeta(replicaKey);
  if (!replica) {
    replica = randomHex(crypto);
    await selected.putMeta(replicaKey, replica);
  }
  if (!tab.stable && selected.durable) {
    selected.limitation = "tab identity storage unavailable; reload identity is not guaranteed";
  }
  const outbox = new DurableOutbox(scope, replica, selected, crypto, random);
  await outbox.load();
  return { outbox, replica, actor: await actorForReplica(scope, replica, crypto) };
}
