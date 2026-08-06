/**
 * Proves a paste adds a shape rather than moving the one that was copied.
 *
 * The dangerous mistake here is quiet: an element's id names a place in the
 * document, so a copy that keeps its id is not a copy at all — pasting it
 * rewrites the original, and what looks like "paste" silently moves the thing
 * you copied. The board still converges, still persists, still undoes. It is
 * simply wrong, and only wrong in a way you notice after the original has
 * moved.
 *
 * So the assertion that matters runs the round trip through the real engine:
 * copy a shape, paste it, and require **two** elements with different ids and
 * an untouched original.
 *
 *   node scripts/clipboard-check.mjs
 *
 * Exits non-zero on failure so CI can gate on it.
 */

import { loadEngine } from "../web/kboard.js";
import * as clipboard from "../web/clipboard.js";

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

console.log("k-board clipboard check\n");

// -- the payload -----------------------------------------------------------

const shape = {
  id: "element-id-that-must-not-travel",
  kind: "rectangle",
  x: 10,
  y: 20,
  w: 100,
  h: 60,
  stroke: 0x1e1e1eff,
  fill: 0,
  stroke_width: 2,
};

const text = clipboard.serialise([shape]);
check("a copied element does not carry its id", !text.includes(shape.id), text.slice(0, 80));
check("the payload is identifiable as ours", text.includes(clipboard.CLIPBOARD_KIND));

const parsed = clipboard.parse(text);
check("a payload round-trips", parsed?.length === 1 && parsed[0].kind === "rectangle");
check(
  "and keeps everything that describes the shape",
  parsed[0].x === 10 && parsed[0].w === 100 && parsed[0].stroke === 0x1e1e1eff,
  JSON.stringify(parsed[0]),
);

// The clipboard belongs to the whole machine. Most of what is on it was never
// meant for this board, and none of it may cause an error.
for (const foreign of [
  "",
  "just some text a user copied from a web page",
  '{"kind":"someone-else/clipboard","elements":[]}',
  `{"kind":"${clipboard.CLIPBOARD_KIND}"`,
  `{"kind":"${clipboard.CLIPBOARD_KIND}","elements":"not an array"}`,
  `{"kind":"${clipboard.CLIPBOARD_KIND}","elements":[]}`,
  null,
  undefined,
  12345,
]) {
  check(`foreign clipboard content is ignored: ${JSON.stringify(foreign)?.slice(0, 40)}`,
    clipboard.parse(foreign) === null);
}

check(
  "an element with unusable coordinates is dropped rather than pasted as NaN",
  clipboard.parse(
    `{"kind":"${clipboard.CLIPBOARD_KIND}","elements":[{"kind":"rectangle","x":null,"y":0,"w":1,"h":1}]}`,
  ) === null,
);

// -- placement -------------------------------------------------------------

const [centred] = clipboard.place(parsed, { x: 500, y: 500 });
check(
  "a paste centres on the pointer",
  centred.x + centred.w / 2 === 500 && centred.y + centred.h / 2 === 500,
  JSON.stringify(centred),
);

const [nudged] = clipboard.place(parsed, null);
check(
  "with no pointer it is nudged off the original",
  nudged.x === 10 + clipboard.PASTE_OFFSET && nudged.y === 20 + clipboard.PASTE_OFFSET,
  JSON.stringify(nudged),
);

const stroke = {
  kind: "freedraw",
  x: 0,
  y: 0,
  w: 0,
  h: 0,
  stroke: 0xf08c00ff,
  stroke_width: 4,
  points: [
    [0, 0],
    [10, 10],
  ],
};
const [movedStroke] = clipboard.place(clipboard.parse(clipboard.serialise([stroke])), {
  x: 100,
  y: 100,
});
check(
  "a freehand stroke's points move with it",
  movedStroke.points[0][0] === 95 && movedStroke.points[1][0] === 105,
  JSON.stringify(movedStroke.points),
);
check(
  "a freehand stroke is recreated as a stroke, not a box",
  clipboard.toCommand(movedStroke).cmd === "stroke",
);
check("a shape is recreated as an add", clipboard.toCommand(centred).cmd === "add");

const sticky = {
  kind: "rectangle",
  role: "sticky",
  x: 20,
  y: 30,
  w: 180,
  h: 120,
  stroke: 0x1e1e1eff,
  fill: 0xffec99ff,
  stroke_width: 2,
  opacity: 0.8,
  font_size: 20,
  text: "Launch plan",
};
const [copiedSticky] = clipboard.parse(clipboard.serialise([sticky]));
const stickyCommand = clipboard.toCommand(copiedSticky);
check("a sticky note keeps its semantic role", copiedSticky.role === "sticky");
check("a sticky note is recreated atomically", stickyCommand.cmd === "sticky");
check(
  "a sticky note keeps text, surface, and opacity",
  stickyCommand.text === "Launch plan" &&
    stickyCommand.fill === 0xffec99ff &&
    stickyCommand.opacity === 0.8,
  JSON.stringify(stickyCommand),
);

// -- the round trip through the real engine --------------------------------

const engine = await loadEngine(`${BASE}/kboard.wasm`);
const board = engine.open("clipboard-check", 1);

const original = engine.exec(board, {
  cmd: "add",
  kind: "rectangle",
  x: 10,
  y: 20,
  w: 100,
  h: 60,
  stroke: 0x1e1e1eff,
});

const copied = clipboard.parse(clipboard.serialise(engine.scene(board)));
const pasted = engine.exec(board, clipboard.toCommand(clipboard.place(copied, { x: 500, y: 500 })[0]));

const scene = engine.scene(board);
check("a paste adds an element rather than editing one", scene.length === 2, `${scene.length}`);
check("the copy has its own identity", original !== pasted, `${original} vs ${pasted}`);

const before = scene.find((item) => item.id === original);
check(
  "and the original has not moved",
  before?.x === 10 && before?.y === 20,
  JSON.stringify(before),
);

const after = scene.find((item) => item.id === pasted);
check(
  "the copy landed where it was placed",
  after?.x === 450 && after?.y === 470,
  JSON.stringify(after),
);
check(
  "and looks like what was copied",
  after?.w === 100 && after?.h === 60 && after?.stroke === 0x1e1e1eff,
  JSON.stringify(after),
);

// Deleting the original must not disturb the copy — the clearest statement
// that they are genuinely separate elements.
engine.exec(board, { cmd: "delete", id: original });
const remaining = engine.scene(board);
check(
  "deleting the original leaves the copy alone",
  remaining.length === 1 && remaining[0].id === pasted,
  JSON.stringify(remaining),
);

const noteBoard = engine.open("clipboard-sticky-check", 2);
const originalNote = engine.exec(noteBoard, clipboard.toCommand(sticky));
const notePayload = clipboard.parse(clipboard.serialise(engine.scene(noteBoard)));
const pastedNote = engine.exec(
  noteBoard,
  clipboard.toCommand(clipboard.place(notePayload, { x: 500, y: 500 })[0]),
);
const notes = engine.scene(noteBoard);
check("sticky paste creates a second element", notes.length === 2 && originalNote !== pastedNote);
check(
  "sticky paste preserves its role and words",
  notes.every((item) => item.role === "sticky" && item.text === "Launch plan"),
  JSON.stringify(notes),
);

console.log(failures === 0 ? "\nAll checks passed." : `\n${failures} check(s) failed.`);
process.exit(failures === 0 ? 0 : 1);
