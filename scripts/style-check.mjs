/**
 * Proves a restyle changes what it was asked to change, and nothing else.
 *
 * The quiet failure here is a client that sends its whole idea of an element's
 * look on every change. It works perfectly alone, and on a shared board it
 * silently reverts a colleague's concurrent restyle to whatever this client
 * happened to be holding — a conflict the CRDT cannot help with, because both
 * writes are genuine and the later one wins on merit.
 *
 * So the assertion that matters is negative: styling the fill must leave the
 * stroke, the width and the opacity exactly as they were.
 *
 *   node scripts/style-check.mjs
 *
 * Exits non-zero on failure so CI can gate on it.
 */

import { loadEngine } from "../web/kboard.js";
import { toSvg } from "../web/scene.js";

const BASE = process.env.KBOARD_URL ?? "http://127.0.0.1:8080";

let failures = 0;
function check(label, condition, detail = "") {
  if (condition) {
    console.log(`  PASS  ${label}`);
  } else {
    failures += 1;
    console.error(`  FAIL  ${label}${detail ? ` — ${detail}` : ""}`);
  }
}

console.log("k-board style check\n");

const engine = await loadEngine(`${BASE}/kboard.wasm`);
const board = engine.open("style-check", 1);

const id = engine.exec(board, {
  cmd: "add",
  kind: "rectangle",
  x: 0,
  y: 0,
  w: 10,
  h: 10,
  stroke: 0x1e1e1eff,
  fill: 0,
  stroke_width: 2,
});
const look = () => engine.scene(board).find((item) => item.id === id);

check("a fresh element is fully opaque", look().opacity === 1, `${look().opacity}`);

engine.exec(board, { cmd: "style", id, fill: 0xff0000ff });
check(
  "styling one property leaves the others alone",
  look().fill === 0xff0000ff && look().stroke === 0x1e1e1eff && look().stroke_width === 2,
  JSON.stringify({ fill: look().fill, stroke: look().stroke, width: look().stroke_width }),
);

engine.exec(board, { cmd: "style", id, stroke_width: 5 });
check("stroke width is settable", look().stroke_width === 5, `${look().stroke_width}`);
check("and setting it did not disturb the fill", look().fill === 0xff0000ff);

engine.exec(board, { cmd: "style", id, opacity: 0.3 });
check("opacity round-trips", look().opacity === 0.3, `${look().opacity}`);

for (const [given, expected] of [
  [1.4, 1],
  [-2, 0],
]) {
  engine.exec(board, { cmd: "style", id, opacity: given });
  check(
    `an opacity of ${given} is clamped to ${expected} rather than refused`,
    look().opacity === expected,
    // A slider that overshoots by a rounding error is not a mistake worth
    // failing an edit for, and every value past the range means its nearest edge.
    `${look().opacity}`,
  );
}

for (const bad of [0, -1]) {
  const before = look().stroke_width;
  try {
    engine.exec(board, { cmd: "style", id, stroke_width: bad });
  } catch {
    /* the engine refuses it; what matters is that nothing changed */
  }
  check(
    `a stroke width of ${bad} is refused`,
    look().stroke_width === before,
    // Unlike opacity there is no nearest sensible edge: a zero-width stroke is
    // not a faint one, it is an invisible element that cannot be found again.
    `${look().stroke_width}`,
  );
}

engine.exec(board, { cmd: "style", id, opacity: 0.5 });
engine.undo(board);
check("undoing a fade restores the previous value", look().opacity === 1, `${look().opacity}`);

// -- and what an export makes of it ---------------------------------------

engine.exec(board, { cmd: "style", id, opacity: 0.25 });
check("a faded element exports its opacity", toSvg(engine.scene(board)).includes('opacity="0.25"'));

engine.exec(board, { cmd: "style", id, opacity: 1 });
check(
  "a solid one exports without the attribute",
  !toSvg(engine.scene(board)).includes("opacity="),
  // Otherwise every element in every file carries an attribute saying
  // "unchanged".
);

console.log(failures === 0 ? "\nAll checks passed." : `\n${failures} check(s) failed.`);
process.exit(failures === 0 ? 0 : 1);
