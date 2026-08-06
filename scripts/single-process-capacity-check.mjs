import { spawnSync } from "node:child_process";

const phases = (process.env.KBOARD_CAPACITY_PHASES ?? "8,32,64,96")
  .split(",")
  .map(Number);
const results = [];

for (const scopes of phases) {
  const run = spawnSync(process.execPath, ["scripts/room-cell-slo-check.mjs"], {
    env: {
      ...process.env,
      KBOARD_SCOPE: `capacity-${Date.now()}-${scopes}`,
      KBOARD_SLO_SCOPES: String(scopes),
      KBOARD_SLO_COLD_COMMANDS: process.env.KBOARD_CAPACITY_COMMANDS ?? "10",
      KBOARD_SLO_HOT_COMMANDS: process.env.KBOARD_CAPACITY_HOT_COMMANDS ?? "40",
      KBOARD_SLO_P99_LIMIT_MS: process.env.KBOARD_CAPACITY_P99_MS ?? "250",
      KBOARD_SLO_MAX_LIMIT_MS: process.env.KBOARD_CAPACITY_MAX_MS ?? "1000",
    },
    encoding: "utf8",
    timeout: 120_000,
  });
  let evidence;
  try {
    evidence = JSON.parse(run.stdout);
  } catch {
    evidence = { output: run.stdout.trim(), error: run.stderr.trim() };
  }
  results.push({ scopes, qualified: run.status === 0, evidence });
  if (run.status !== 0) break;
}

const highestQualified = results.filter((result) => result.qualified).at(-1)?.scopes ?? 0;
const report = {
  meaning:
    "measured lower bound for one process on this machine; not permission to exceed configured resource ceilings",
  highestQualifiedConcurrentScopes: highestQualified,
  phases: results,
};
console.log(JSON.stringify(report, null, 2));
if (highestQualified === 0) process.exit(1);
