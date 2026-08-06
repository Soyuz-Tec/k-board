/**
 * Proves the exported file is the drawing on the screen.
 *
 * `web/scene.js` describes each shape once and feeds two back-ends — canvas for
 * the screen, SVG for export. That is only worth doing if the two really do
 * stay in step, so this check drives both from the same scene and compares what
 * each one drew, command by command. A rectangle that exports as a diamond is
 * the failure this exists to catch, and it is exactly the failure nobody
 * notices until the file is already sent.
 *
 * The canvas side is driven through a recording stub rather than a real canvas,
 * so the check needs no DOM and no dependencies.
 *
 *   node scripts/export-check.mjs
 *
 * Exits non-zero on failure so CI can gate on it.
 */

import { bounds, drawShape, geometry, sceneBounds, toSvg } from "../web/scene.js";

let failures = 0;
function check(label, condition, detail = "") {
  if (condition) {
    console.log(`  PASS  ${label}`);
  } else {
    failures += 1;
    console.error(`  FAIL  ${label}${detail ? ` — ${detail}` : ""}`);
  }
}

const round = (value) => Math.round(value * 100) / 100;

/** A 2D context that draws nothing and remembers everything. */
function recorder() {
  const commands = [];
  return {
    commands,
    set strokeStyle(_) {},
    set fillStyle(_) {},
    set lineWidth(_) {},
    set lineJoin(_) {},
    set lineCap(_) {},
    beginPath() {},
    closePath() {
      commands.push(["Z"]);
    },
    moveTo(x, y) {
      commands.push(["M", round(x), round(y)]);
    },
    lineTo(x, y) {
      commands.push(["L", round(x), round(y)]);
    },
    quadraticCurveTo(cx, cy, x, y) {
      commands.push(["Q", round(cx), round(cy), round(x), round(y)]);
    },
    ellipse(cx, cy, rx, ry) {
      commands.push(["E", round(cx), round(cy), round(rx), round(ry)]);
    },
    set font(_) {},
    set textBaseline(_) {},
    fillText(content, x, y) {
      commands.push(["T", round(x), round(y), content]);
    },
    fill() {},
    stroke() {},
  };
}

function unescapeText(value) {
  return value
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&quot;/g, '"')
    .replace(/&#39;/g, "'")
    .replace(/&amp;/g, "&");
}

/** Read the same commands back out of SVG markup. */
function parseSvg(markup) {
  const commands = [];
  // One pass in document order. Collecting text separately would compare the
  // two back-ends in different orders and report drift that is not there.
  const figures = /<text\s[^>]*>(.*?)<\/text>|<(?:path|ellipse)\s([^>]*)\/>/g;
  for (const [, spans, attributes] of markup.matchAll(figures)) {
    if (spans !== undefined) {
      const tspans = /<tspan x="([-\d.]+)" y="([-\d.]+)">(.*?)<\/tspan>/g;
      for (const [, x, y, content] of spans.matchAll(tspans)) {
        commands.push(["T", Number(x), Number(y), unescapeText(content)]);
      }
      continue;
    }
    const ellipse = /cx="([-\d.]+)" cy="([-\d.]+)" rx="([-\d.]+)" ry="([-\d.]+)"/.exec(attributes);
    if (ellipse) {
      commands.push(["E", ...ellipse.slice(1, 5).map(Number)]);
      continue;
    }
    const data = /d="([^"]*)"/.exec(attributes)[1];
    for (const [, letter, args] of data.matchAll(/([MLQZ])([-\d.\s]*)/g)) {
      const numbers = args.trim() ? args.trim().split(/\s+/).map(Number) : [];
      commands.push([letter, ...numbers]);
    }
  }
  return commands;
}

const scene = [
  { kind: "rectangle", x: 10, y: 20, w: 100, h: 60, stroke: 0x1e1e1eff, fill: 0, stroke_width: 2 },
  { kind: "ellipse", x: 200, y: 40, w: 80, h: 80, stroke: 0xe03131ff, fill: 0xffff00ff, stroke_width: 3 },
  { kind: "diamond", x: 300, y: 10, w: 50, h: 90, stroke: 0x1971c2ff, fill: 0, stroke_width: 2 },
  { kind: "line", x: 360, y: 110, w: 90, h: 45, stroke: 0x1971c2ff, fill: 0, stroke_width: 3 },
  { kind: "arrow", x: 400, y: 200, w: -120, h: -60, stroke: 0x2f9e44ff, fill: 0, stroke_width: 2 },
  {
    kind: "freedraw",
    x: 0,
    y: 0,
    w: 0,
    h: 0,
    stroke: 0xf08c00ff,
    fill: 0,
    stroke_width: 4,
    points: [
      [500, 300],
      [520, 330],
      [545, 315],
      [560, 350],
    ],
  },
  {
    kind: "text",
    x: 600,
    y: 100,
    w: 180,
    h: 50,
    stroke: 0x1e1e1eff,
    fill: 0,
    stroke_width: 2,
    font_size: 20,
    // A second line, and characters that must not become markup.
    text: 'label <one> & "two"\nsecond line',
  },
  {
    kind: "rectangle",
    role: "sticky",
    x: 820,
    y: 80,
    w: 180,
    h: 120,
    stroke: 0xf08c00ff,
    fill: 0xffec99ff,
    stroke_width: 2,
    font_size: 20,
    text: "sticky note\nsecond line",
  },
];

console.log("k-board export check\n");

// 1. Nothing in the scene model is silently undrawable.
for (const item of scene) {
  const { figures } = geometry(item);
  check(`${item.kind} produces geometry`, figures.length > 0);
}

// 2. The claim the design rests on: one description, two back-ends, no drift.
const svg = toSvg(scene, { padding: 16, background: "#ffffff", title: "k-board demo" });
const fromSvg = parseSvg(svg);
const canvas = recorder();
for (const item of scene) drawShape(canvas, item);

check(
  "canvas and SVG issue the same number of commands",
  canvas.commands.length === fromSvg.length,
  `canvas=${canvas.commands.length} svg=${fromSvg.length}`,
);
check(
  "canvas and SVG draw identical geometry",
  JSON.stringify(canvas.commands) === JSON.stringify(fromSvg),
  `\n    canvas: ${JSON.stringify(canvas.commands.slice(0, 6))}\n    svg:    ${JSON.stringify(
    fromSvg.slice(0, 6),
  )}`,
);

// 3. The export is the board, not the viewport.
const box = sceneBounds(scene);
const viewBox = /viewBox="([^"]*)"/.exec(svg)[1].split(" ").map(Number);
check(
  "the viewBox is cropped to the content plus padding",
  viewBox[0] === round(box.x - 16) && viewBox[1] === round(box.y - 16),
  `viewBox=${viewBox} content=${JSON.stringify(box)}`,
);
check(
  "stroke width is inside the crop",
  // The widest stroke is 4, so its shape reaches 2 beyond its own geometry. An
  // export cropped to the geometry alone clips it.
  box.y <= Math.min(...scene.map((item) => bounds(item).y)) - 1,
  `top=${box.y}`,
);

// 4. Details that only bite once a file is open somewhere else.
check("the SVG declares its namespace", svg.includes('xmlns="http://www.w3.org/2000/svg"'));
check("the SVG carries an accessible name", svg.includes("<title>k-board demo</title>"));
check(
  "markup in a scope name cannot escape into the document",
  !toSvg(scene, { title: '</title><script>x</script>' }).includes("<script>"),
);
check(
  "an unfilled shape is transparent rather than black",
  svg.includes('fill="none"'),
  "an SVG path with no fill attribute fills black by default",
);
check("a filled shape keeps its fill", svg.includes('fill="rgba(255,255,0,1)"'));
check(
  "text is emitted as text rather than as an outline",
  svg.includes("<text ") && svg.includes("<tspan "),
);
check(
  "angle brackets and ampersands in a label cannot become markup",
  svg.includes("&lt;one&gt;") && svg.includes("&amp;") && !svg.includes("<one>"),
);
check(
  "typed whitespace survives the trip",
  svg.includes('xml:space="preserve"'),
  "SVG collapses whitespace by default and would silently reflow the label",
);
check(
  "a faded element carries its opacity",
  toSvg([{ ...scene[0], opacity: 0.4 }]).includes('opacity="0.4"'),
);
check(
  "a solid one does not",
  // Otherwise every element in the file carries an attribute saying "unchanged".
  !svg.includes('opacity="1"'),
);
check("an empty scene exports nothing rather than a blank file", toSvg([]) === null);
check("an empty scene has no bounds", sceneBounds([]) === null);

console.log(failures === 0 ? "\nAll checks passed." : `\n${failures} check(s) failed.`);
process.exit(failures === 0 ? 0 : 1);
