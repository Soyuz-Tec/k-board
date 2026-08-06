/**
 * Repository architecture guardrails.
 *
 * These checks deliberately cover stable, high-value boundaries rather than
 * source layout preferences. The compiler and behavior tests own the rest.
 */

import { readdir, readFile, stat } from "node:fs/promises";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
let failures = 0;

function check(label, condition, detail = "") {
  if (condition) {
    console.log(`  PASS  ${label}`);
  } else {
    failures += 1;
    console.error(`  FAIL  ${label}${detail ? ` — ${detail}` : ""}`);
  }
}

async function text(relativePath) {
  return readFile(path.join(root, relativePath), "utf8");
}

async function filesBelow(relativePath) {
  const directory = path.join(root, relativePath);
  const entries = await readdir(directory);
  const found = [];
  for (const entry of entries) {
    const absolute = path.join(directory, entry);
    const info = await stat(absolute);
    if (info.isDirectory()) {
      found.push(
        ...(await filesBelow(path.relative(root, absolute))),
      );
    } else {
      found.push(path.relative(root, absolute));
    }
  }
  return found;
}

const architecture = await text("docs/ARCHITECTURE.md");
for (const heading of [
  "Purpose and quality goals",
  "Stakeholders and concerns",
  "System context",
  "Building-block view",
  "Runtime views",
  "Deployment view",
  "Architecture decisions",
  "Quality scenarios",
  "Known risks and technical debt",
  "Standards traceability",
]) {
  check(`architecture describes ${heading.toLowerCase()}`, architecture.includes(heading));
}

const workingAgreements = await text("AGENTS.md");
const reviewability = await text("docs/architecture/reviewability-standard.md");
const pullRequestTemplate = await text(".github/pull_request_template.md");
check(
  "working agreements classify review thresholds as advisory",
  workingAgreements.includes("not automatic rejection"),
);
check(
  "reviewability standard prohibits LOC-only CI failure",
  reviewability.includes("Do not fail CI solely because"),
);
check(
  "pull request template captures threshold justification",
  pullRequestTemplate.includes("Exceeded thresholds, excluded mechanical/generated files"),
);

const adrIndex = await text("docs/adr/README.md");
const adrFiles = (await filesBelow("docs/adr"))
  .map((file) => path.basename(file))
  .filter((file) => /^\d{4}-.*\.md$/.test(file));
for (const adr of adrFiles) {
  check(`ADR index includes ${adr}`, adrIndex.includes(`(${adr})`));
  const body = await text(path.join("docs/adr", adr));
  check(
    `${adr} declares a status`,
    /^\s*- \*\*Status:\*\* (Accepted|Proposed|Deprecated|Superseded)\s*$/m.test(body),
  );
}

const coreManifest = await text("crates/kboard-core/Cargo.toml");
check(
  "core has no workspace-crate adapter dependency",
  !/^kboard-(ffi|server|store)\s*=/m.test(coreManifest),
);

const rustFiles = (await filesBelow("crates")).filter(
  (file) => file.endsWith(".rs") && !file.startsWith(path.join("crates", "kboard-ffi")),
);
const unsafeOutsideFfi = [];
for (const file of rustFiles) {
  const body = await text(file);
  if (/\bunsafe\s+(?:fn|extern|trait|impl)|\bunsafe\s*\{/m.test(body)) {
    unsafeOutsideFfi.push(file);
  }
}
check(
  "executable unsafe Rust is confined to kboard-ffi",
  unsafeOutsideFfi.length === 0,
  unsafeOutsideFfi.join(", "),
);

if (failures > 0) {
  console.error(`\n${failures} architecture check(s) failed.`);
  process.exit(1);
}

console.log("\nArchitecture checks passed.");
