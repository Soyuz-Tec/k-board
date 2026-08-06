/**
 * Advisory review-size report.
 *
 * Threshold findings never set a failing exit status. Operational failures do:
 * a broken advisory is different from code that merely deserves extra review.
 */

import { execFileSync } from "node:child_process";
import { readdir, readFile, stat } from "node:fs/promises";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const FILE_LINES = 600;
const CHANGE_LINES = 500;
const CHANGE_FILES = 20;
const sourceExtensions = new Set([
  ".rs",
  ".js",
  ".mjs",
  ".jsx",
  ".ts",
  ".tsx",
  ".css",
  ".html",
]);
const excludedSegments = new Set([
  "fixtures",
  "generated",
  "node_modules",
  "target",
  "vendor",
]);

function isMaintainedSource(relativePath) {
  const segments = relativePath.split(/[\\/]/);
  return (
    sourceExtensions.has(path.extname(relativePath)) &&
    !segments.some((segment) => excludedSegments.has(segment))
  );
}

function physicalLineCount(body) {
  if (body.length === 0) return 0;
  const boundaries = body.match(/\r?\n/g)?.length ?? 0;
  return boundaries + (body.endsWith("\n") ? 0 : 1);
}

async function filesBelow(relativePath) {
  const absolute = path.join(root, relativePath);
  const entries = await readdir(absolute);
  const files = [];
  for (const entry of entries) {
    const childRelative = path.join(relativePath, entry);
    const child = path.join(root, childRelative);
    if ((await stat(child)).isDirectory()) {
      if (!excludedSegments.has(entry)) {
        files.push(...(await filesBelow(childRelative)));
      }
    } else if (isMaintainedSource(childRelative)) {
      files.push(childRelative);
    }
  }
  return files;
}

function warning(message, file = "") {
  console.warn(`  REVIEW  ${message}`);
  if (process.env.GITHUB_ACTIONS === "true") {
    const location = file ? ` file=${file.replaceAll("\\", "/")}` : "";
    console.log(`::warning${location}::${message}`);
  }
}

function requestedBase() {
  const index = process.argv.indexOf("--base");
  if (index >= 0) {
    if (!process.argv[index + 1]) throw new Error("--base requires a Git ref");
    return process.argv[index + 1];
  }
  return process.env.GITHUB_BASE_REF
    ? `origin/${process.env.GITHUB_BASE_REF}`
    : null;
}

console.log("Reviewability advisory\n");

const oversized = [];
for (const directory of ["crates", "scripts", "web"]) {
  for (const file of await filesBelow(directory)) {
    const lines = physicalLineCount(await readFile(path.join(root, file), "utf8"));
    if (lines > FILE_LINES) oversized.push({ file, lines });
  }
}
oversized.sort((left, right) => right.lines - left.lines);
if (oversized.length === 0) {
  console.log(`  PASS    no maintained source file exceeds ${FILE_LINES} physical lines`);
} else {
  for (const { file, lines } of oversized) {
    warning(`${file} has ${lines} physical lines (review threshold ${FILE_LINES})`, file);
  }
}

const base = requestedBase();
if (base) {
  const output = execFileSync(
    "git",
    ["diff", "--numstat", `${base}...HEAD`, "--"],
    { cwd: root, encoding: "utf8" },
  );
  const changed = output
    .trim()
    .split(/\r?\n/)
    .filter(Boolean)
    .map((line) => {
      const [added, deleted, file] = line.split("\t");
      return { added, deleted, file };
    })
    .filter(({ file }) => isMaintainedSource(file));
  const changedLines = changed.reduce(
    (total, { added, deleted }) =>
      total +
      (added === "-" ? 0 : Number(added)) +
      (deleted === "-" ? 0 : Number(deleted)),
    0,
  );

  console.log(`\nChange comparison: ${base}...HEAD`);
  if (changedLines > CHANGE_LINES) {
    warning(`${changedLines} changed source lines exceed review threshold ${CHANGE_LINES}`);
  } else {
    console.log(`  PASS    ${changedLines} changed source lines (threshold ${CHANGE_LINES})`);
  }
  if (changed.length > CHANGE_FILES) {
    warning(`${changed.length} changed source files exceed review threshold ${CHANGE_FILES}`);
  } else {
    console.log(`  PASS    ${changed.length} changed source files (threshold ${CHANGE_FILES})`);
  }
} else {
  console.log("\n  INFO    no --base supplied; change-size advisory was skipped");
}

console.log(
  "\nAdvisory only: explain or split threshold crossings; do not fail solely on these counts.",
);
