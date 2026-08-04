/**
 * Proves a selection moves, scales and turns as one thing.
 *
 * The arithmetic here is easy to get subtly wrong in ways that look fine on a
 * rectangle and fall apart on anything else — a freehand stroke whose box moves
 * without its path, a label whose font size stretches on one axis, a group
 * rotation that spins each piece where it stands instead of orbiting the
 * centre. Each of those is checked by name.
 *
 * It also pins a defect this work uncovered: `move` writes only x and y, and
 * freehand geometry lives in its path, so dragging a pen stroke moved its
 * bounding box and left the ink behind.
 *
 *   node scripts/transform-check.mjs
 *
 * Exits non-zero on failure so CI can gate on it.
 */

import { loadEngine } from "../web/kboard.js";
import { bounds, corners, intoLocal } from "../web/scene.js";
import * as transform from "../web/transform.js";

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

const near = (a, b, slack = 1e-6) => Math.abs(a - b) <= slack;

console.log("k-board transform check\n");

// -- the box and its handles -----------------------------------------------

const rect = { id: "a", kind: "rectangle", x: 0, y: 0, w: 100, h: 50, angle: 0, font_size: 20 };
const box = transform.unionBounds([rect]);
check("an upright box is its own bounds", box.w === 100 && box.h === 50, JSON.stringify(box));

const turned = { ...rect, w: 100, h: 100, angle: Math.PI / 4 };
const turnedBox = transform.unionBounds([turned]);
check(
  "a turned box covers the shape's real extent",
  near(turnedBox.w, Math.SQRT2 * 100, 0.01),
  // A square turned 45 degrees reaches its own diagonal. A box that ignored
  // rotation would report 100 and crop the corners off every export.
  `${turnedBox.w.toFixed(2)}`,
);

check("a corner handle is grabbable", transform.handleAt(box, 100, 50) === "se");
check("an edge handle is grabbable", transform.handleAt(box, 50, 0) === "n");
check("the rotate handle sits clear of the box", transform.handleAt(box, 50, -24) === "rotate");
check("empty space grabs nothing", transform.handleAt(box, 500, 500) === null);

// -- dragging a handle ------------------------------------------------------

check(
  "dragging a corner holds the opposite one still",
  JSON.stringify(transform.boxFromDrag(box, "se", 200, 100)) ===
    JSON.stringify({ x: 0, y: 0, w: 200, h: 100 }),
);
check(
  "an edge handle moves one axis only",
  JSON.stringify(transform.boxFromDrag(box, "e", 200, 999)) ===
    JSON.stringify({ x: 0, y: 0, w: 200, h: 50 }),
);
const flipped = transform.boxFromDrag(box, "e", -20, 0);
check(
  "dragging a handle past its opposite flips rather than going negative",
  flipped.w === 20 && flipped.x === -20,
  // A negative size is something every consumer downstream then has to defend
  // against, one at a time, forever.
  JSON.stringify(flipped),
);

// -- scaling ----------------------------------------------------------------

const stroke = {
  id: "s",
  kind: "freedraw",
  x: 0,
  y: 0,
  w: 10,
  h: 10,
  angle: 0,
  points: [
    [0, 0],
    [5, 5],
    [10, 10],
  ],
};
const [scaled] = transform.resize([stroke], { x: 0, y: 0, w: 10, h: 10 }, { x: 0, y: 0, w: 20, h: 20 });
check(
  "scaling a freehand stroke scales its path",
  JSON.stringify(scaled.points) === JSON.stringify([[0, 0], [10, 10], [20, 20]]),
  // The path *is* the shape. A box that grew without it leaves the ink its
  // original size inside a box claiming otherwise.
  JSON.stringify(scaled.points),
);

const label = { id: "t", kind: "text", x: 0, y: 0, w: 100, h: 25, font_size: 20, angle: 0 };
const [stretched] = transform.resize(
  [label],
  { x: 0, y: 0, w: 100, h: 25 },
  { x: 0, y: 0, w: 400, h: 50 },
);
check(
  "a label scales uniformly however it is dragged",
  stretched.font_size === 40 && stretched.w === 200 && stretched.h === 50,
  // The stored box and the glyphs have to agree. A font size stretched on one
  // axis only leaves words that no longer fit the box recorded for them.
  `${stretched.w}x${stretched.h} at ${stretched.font_size}`,
);

const [shrunk] = transform.resize([rect], box, { x: 0, y: 0, w: 0, h: 0 });
check(
  "a shape cannot be resized out of existence",
  shrunk.w >= transform.MIN_EXTENT && shrunk.h >= transform.MIN_EXTENT,
  // Zero-sized, it can never be grabbed again to be made larger.
  `${shrunk.w}x${shrunk.h}`,
);

// -- turning ----------------------------------------------------------------

const [spun] = transform.rotate([rect], { x: 50, y: 25 }, Math.PI / 2);
check(
  "rotating about its own centre leaves a shape in place",
  near(spun.x, 0) && near(spun.y, 0) && near(spun.angle, Math.PI / 2),
  JSON.stringify(spun),
);

const pair = [rect, { ...rect, id: "b", x: 200 }];
const rotated = transform.rotate(pair, { x: 150, y: 25 }, Math.PI);
check(
  "rotating a group orbits its members rather than spinning each in place",
  near(rotated[0].x, 200) && near(rotated[1].x, 0),
  // Half a turn about the midpoint swaps them. Members that only spun would
  // stay exactly where they were.
  `${rotated[0].x.toFixed(1)} and ${rotated[1].x.toFixed(1)}`,
);

const twice = transform.rotate(transform.rotate([rect], { x: 50, y: 25 }, 1), { x: 50, y: 25 }, 1);
check("turns accumulate", near(twice[0].angle, 2), `${twice[0].angle}`);

// -- hit testing through a rotation ----------------------------------------

const [localX, localY] = intoLocal(turned, turned.x + turned.w / 2, turned.y + turned.h / 2);
check(
  "the centre of a turned shape maps to itself",
  near(localX, 50) && near(localY, 50),
  `${localX}, ${localY}`,
);
const outside = intoLocal(turned, turnedBox.x + 1, turnedBox.y + 1);
const inner = bounds(turned);
check(
  "a point in the corner of a turned shape's box is outside the shape",
  outside[0] < inner.x || outside[1] < inner.y,
  // Without this, the clickable region of a turned shape drifts away from the
  // drawn one and grows as it turns.
  JSON.stringify(outside),
);
check("a turned shape reports four moved corners", corners(turned).length === 4);

// -- marquee ---------------------------------------------------------------

check("an overlapping marquee catches a shape", transform.intersects(rect, { x: -5, y: -5, w: 20, h: 20 }));
check("a distant marquee does not", !transform.intersects(rect, { x: 900, y: 900, w: 10, h: 10 }));

// -- and the same thing through the real engine ----------------------------

const engine = await loadEngine(`${BASE}/kboard.wasm`);
const board = engine.open("transform-check", 1);

const penId = engine.exec(board, {
  cmd: "stroke",
  points: [
    [0, 0],
    [10, 10],
  ],
  stroke: 0x1e1e1eff,
  stroke_width: 2,
});

// The defect this uncovered: `move` writes x and y, and a freehand stroke is
// drawn from its path, so the box walked off and left the ink behind.
engine.exec(board, { cmd: "move", id: penId, x: 100, y: 100 });
let pen = engine.scene(board).find((item) => item.id === penId);
check(
  "moving a stroke by x and y alone does not move the ink",
  pen.points[0][0] === 0,
  // Stated as the defect it is, so the next person does not "fix" the command
  // that replaced it back into the one that did not work.
  JSON.stringify(pen.points),
);

engine.exec(board, {
  cmd: "geometry",
  id: penId,
  x: 100,
  y: 100,
  w: 10,
  h: 10,
  points: [
    [100, 100],
    [110, 110],
  ],
});
pen = engine.scene(board).find((item) => item.id === penId);
check(
  "a geometry write moves the ink with the box",
  pen.points[0][0] === 100 && pen.x === 100,
  JSON.stringify(pen.points),
);

const rectId = engine.exec(board, {
  cmd: "add",
  kind: "rectangle",
  x: 0,
  y: 0,
  w: 10,
  h: 10,
  stroke: 0x1e1e1eff,
});
check("a fresh element reports an angle of zero", engine.scene(board).find((i) => i.id === rectId).angle === 0);

engine.exec(board, { cmd: "geometry", id: rectId, x: 0, y: 0, w: 10, h: 10, angle: 1.2 });
check("a rotation round-trips", engine.scene(board).find((i) => i.id === rectId).angle === 1.2);

engine.undo(board);
check(
  "undoing the first rotation leaves the shape flat",
  engine.scene(board).find((i) => i.id === rectId).angle === 0,
  // Nothing had ever written an angle, so there was no prior value to restore.
  `${engine.scene(board).find((i) => i.id === rectId).angle}`,
);

// -- the box a freehand stroke records ------------------------------------

const inked = engine.exec(board, {
  cmd: "stroke",
  points: [
    [10, 20],
    [50, 5],
    [30, 60],
  ],
  stroke: 0x1e1e1eff,
  stroke_width: 2,
});
const drawn = engine.scene(board).find((item) => item.id === inked);
check(
  "a stroke records the box its path occupies",
  drawn.x === 10 && drawn.y === 5 && drawn.w === 40 && drawn.h === 55,
  // It used to record only x and y, leaving every consumer a zero-sized box —
  // which is exactly the path scan the box exists to save them.
  `${drawn.x},${drawn.y} ${drawn.w}x${drawn.h}`,
);

console.log(failures === 0 ? "\nAll checks passed." : `\n${failures} check(s) failed.`);
process.exit(failures === 0 ? 0 : 1);
