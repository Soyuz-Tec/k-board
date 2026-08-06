const BASE = process.env.KBOARD_URL ?? "http://127.0.0.1:8080";

const response = await fetch(`${BASE}/api/storage/health`);
if (!response.ok) {
  throw new Error(`storage health returned HTTP ${response.status}`);
}

const health = await response.json();
const expected = {
  schema_version: 2,
  writer_concurrency: 1,
  max_operations_per_batch: 512,
  readable: true,
  writable: true,
};

for (const [field, value] of Object.entries(expected)) {
  if (health[field] !== value) {
    throw new Error(`${field}: expected ${value}, received ${health[field]}`);
  }
}

for (const field of ["wal_busy", "wal_log_frames", "wal_checkpointed_frames"]) {
  if (!Number.isInteger(health[field]) || health[field] < 0) {
    throw new Error(`${field}: expected a non-negative integer`);
  }
}

if (health.wal_checkpointed_frames > health.wal_log_frames) {
  throw new Error("checkpointed WAL frames exceed total WAL frames");
}

console.log("PASS  schema version is operationally visible");
console.log("PASS  WAL checkpoint health is numeric and internally consistent");
console.log("PASS  storage writer and batch bounds are operationally visible");
console.log("PASS  readiness proves readable and writable SQLite transactions");
if (
  !health.metrics ||
  !Number.isInteger(health.metrics.queue_depth) ||
  !Number.isInteger(health.metrics.queue_wait?.count) ||
  !Number.isInteger(health.metrics.sql?.count)
) {
  throw new Error("storage queue and SQL timing metrics are missing");
}
console.log("PASS  storage queue wait is observable independently of SQL time");
