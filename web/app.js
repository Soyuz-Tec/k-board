/**
 * k-board web client.
 *
 * Every tab is an independent replica. It holds its own copy of the document in
 * wasm, applies local edits immediately, and reconciles with peers through the
 * server. Nothing here resolves conflicts — the engine does, identically on
 * every replica and on the server, because it is literally the same build.
 */

import { loadEngine } from "./kboard.js";
import { bounds, drawShape, pack, sceneBounds, toSvg } from "./scene.js";
import { Presence, drawCursors, peerName } from "./presence.js";

const canvas = document.getElementById("canvas");
const context = canvas.getContext("2d");
const statusDot = document.getElementById("dot");
const statusText = document.getElementById("statusText");
const hint = document.getElementById("hint");
const a11yList = document.getElementById("a11yList");
const peerList = document.getElementById("peerList");
const undoButton = document.getElementById("undo");
const redoButton = document.getElementById("redo");
const pngButton = document.getElementById("exportPng");
const svgButton = document.getElementById("exportSvg");

const COMMIT_INTERVAL_MS = 50; // live-drag update rate sent to peers

const state = {
  engine: null,
  board: null,
  actor: null,
  scope: location.hash.slice(1) || "demo",
  tool: "select",
  colour: "#1e1e1e",
  view: { x: 0, y: 0, scale: 1 },
  scene: [],
  draft: null,
  drag: null,
  pan: null,
  socket: null,
  outbox: [],
  everConnected: false,
  dirty: true,
  // Throwaway. Never reaches the engine, never reaches the log.
  presence: new Presence(),
};

// -- geometry --------------------------------------------------------------

function hitTest(sceneX, sceneY) {
  const slack = 6 / state.view.scale;
  // Reverse: topmost element in paint order wins.
  for (let index = state.scene.length - 1; index >= 0; index -= 1) {
    const box = bounds(state.scene[index]);
    if (
      sceneX >= box.x - slack &&
      sceneX <= box.x + box.w + slack &&
      sceneY >= box.y - slack &&
      sceneY <= box.y + box.h + slack
    ) {
      return state.scene[index];
    }
  }
  return null;
}

function toScene(clientX, clientY) {
  const rect = canvas.getBoundingClientRect();
  return [
    (clientX - rect.left - state.view.x) / state.view.scale,
    (clientY - rect.top - state.view.y) / state.view.scale,
  ];
}

// -- rendering -------------------------------------------------------------

/** The view changed (draft, pan, zoom). Repaint; the document is unchanged. */
function invalidate() {
  state.dirty = true;
}

/**
 * The document changed. Refresh the scene and the accessibility tree, then
 * repaint.
 *
 * Deliberately not driven by `requestAnimationFrame`. rAF is paused entirely in
 * a background tab, and an assistive-technology user does not need pixels to be
 * painting for the content to be readable. Tying the DOM mirror to the render
 * loop would let it go silently stale.
 */
function sceneChanged() {
  if (state.board !== null) state.scene = state.engine.scene(state.board);
  describeForScreenReaders();
  refreshControls();
  state.dirty = true;
}

// Reads the engine rather than tracking it here, so the buttons can never
// disagree with what undo would actually do.
function refreshControls() {
  if (state.board === null) return;
  const { canUndo, canRedo } = state.engine.history(state.board);
  undoButton.disabled = !canUndo;
  redoButton.disabled = !canRedo;
  // An export of an empty board is a blank file that looks like a failure.
  const empty = state.scene.length === 0;
  pngButton.disabled = empty;
  svgButton.disabled = empty;
}

function stepHistory(backward) {
  if (state.board === null) return;
  const moved = backward
    ? state.engine.undo(state.board)
    : state.engine.redo(state.board);
  if (!moved) return;
  flush();
  sceneChanged();
}

function drawGrid(width, height) {
  const step = 24 * state.view.scale;
  if (step < 9) return;
  const style = getComputedStyle(document.documentElement);
  context.fillStyle = style.getPropertyValue("--grid").trim() || "#eee";
  const startX = state.view.x % step;
  const startY = state.view.y % step;
  for (let x = startX; x < width; x += step) {
    for (let y = startY; y < height; y += step) {
      context.fillRect(x, y, 1.5, 1.5);
    }
  }
}

function render() {
  const ratio = window.devicePixelRatio || 1;
  const width = canvas.clientWidth;
  const height = canvas.clientHeight;
  if (canvas.width !== width * ratio || canvas.height !== height * ratio) {
    canvas.width = width * ratio;
    canvas.height = height * ratio;
  }

  context.setTransform(ratio, 0, 0, ratio, 0, 0);
  context.clearRect(0, 0, width, height);
  drawGrid(width, height);

  const { x, y, scale } = state.view;
  context.setTransform(ratio * scale, 0, 0, ratio * scale, ratio * x, ratio * y);

  for (const item of state.scene) drawShape(context, item);
  if (state.draft) drawShape(context, state.draft);

  // Above the drawing and outside the scene transform: a cursor is a pointer,
  // not an object on the board.
  const peers = state.presence.list();
  if (peers.length > 0) drawCursors(context, peers, { ...state.view, ratio });
}

function frame() {
  // Cheap, and it runs whether or not anything else changed: a peer who stops
  // sending must stop being drawn even on an otherwise idle board.
  if (state.presence.expire(performance.now())) peersChanged();

  if (state.dirty) {
    state.dirty = false;
    render();
  }
  requestAnimationFrame(frame);
}

/**
 * Presence moved. Repaint, and update the readout.
 *
 * Separate from `sceneChanged` on purpose: presence is not a document change,
 * so it must not touch the engine, the accessibility mirror of the drawing, or
 * the undo controls.
 */
function peersChanged() {
  const peers = state.presence.list();
  const readout =
    peers.length === 0
      ? "You are the only one here."
      : `Also here: ${peers.map((peer) => peerName(peer.actor)).join(", ")}.`;
  // Only written when it actually differs. This is a live region, and a peer
  // moving their mouse changes their position seventeen times a second without
  // changing who is present — rewriting it each time would make a screen reader
  // announce the same sentence over and over.
  if (peerList.textContent !== readout) peerList.textContent = readout;
  state.dirty = true;
}

/**
 * Mirror the scene into the DOM.
 *
 * Canvas pixels carry no semantics, so assistive technology sees an empty
 * region. This is the difference between a canvas app that is keyboard-operable
 * and one that is actually readable.
 */
function describeForScreenReaders() {
  const items = state.scene;
  a11yList.replaceChildren(
    ...items.map((item, index) => {
      const box = bounds(item);
      const entry = document.createElement("li");
      entry.textContent = `${item.kind} ${index + 1}, at ${Math.round(box.x)}, ${Math.round(
        box.y,
      )}, ${Math.round(box.w)} by ${Math.round(box.h)}`;
      return entry;
    }),
  );
}

// -- sync ------------------------------------------------------------------

function setStatus(kind, text) {
  statusDot.className = `dot ${kind}`;
  statusText.textContent = text;
}

function ensureBoard(actor) {
  if (state.board !== null) return;
  state.actor = actor;
  state.board = state.engine.open(state.scope, actor);
}

function flush() {
  if (state.board === null) return;
  const ops = state.engine.pending(state.board);
  if (ops.length > 0) state.outbox.push(...ops);
  if (state.outbox.length === 0) return;

  if (state.socket?.readyState === WebSocket.OPEN) {
    state.socket.send(JSON.stringify({ type: "ops", ops: state.outbox }));
    // Only cleared once handed to an open socket. Anything drawn while offline
    // waits here and replays on reconnect — merge is idempotent, so a resend
    // that the server already saw costs nothing.
    state.outbox = [];
  }
}

function connect() {
  const protocol = location.protocol === "https:" ? "wss" : "ws";
  const url = `${protocol}://${location.host}/ws/${encodeURIComponent(state.scope)}`;
  // Offered as a subprotocol rather than a query parameter: a URL ends up in
  // server logs, browser history, and referrers. An open server ignores it.
  const token = new URLSearchParams(location.search).get("token");
  const socket = token
    ? new WebSocket(url, [`kboard.token.${token}`])
    : new WebSocket(url);
  state.socket = socket;
  setStatus("", "connecting");

  socket.onopen = () => {
    state.everConnected = true;
    setStatus("live", `live · ${state.scope}`);
    flush();
  };

  socket.onmessage = (event) => {
    let message;
    try {
      message = JSON.parse(event.data);
    } catch {
      return;
    }

    if (message.type === "init") {
      ensureBoard(message.actor);
      state.engine.load(state.board, JSON.stringify(message.doc));
      setStatus("live", `live · ${state.scope} · you are #${message.actor}`);
      flush();
      sceneChanged();
    } else if (message.type === "ops") {
      state.engine.merge(state.board, JSON.stringify(message.ops));
      sceneChanged();
    } else if (message.type === "presence") {
      state.presence.observe(message.actor, message.x, message.y, performance.now());
      peersChanged();
    } else if (message.type === "left") {
      if (state.presence.forget(message.actor)) peersChanged();
    }
  };

  socket.onclose = (event) => {
    // 1008/1006 after an immediate close usually means the handshake was
    // refused. Retrying forever against a rejected token looks like a network
    // problem to the user, so name it.
    if (!state.everConnected) {
      setStatus("offline", "offline · not authorised for this board");
      return;
    }
    // Nobody is reachable, so nobody's cursor is current. Leaving them on
    // screen would show a room full of people who cannot see you.
    state.presence.clear();
    peersChanged();
    setStatus("offline", "offline · edits are queued");
    // Reconnect, and keep drawing meanwhile. The queue drains on reopen.
    setTimeout(connect, 1200);
  };

  socket.onerror = () => socket.close();
}

// -- presence --------------------------------------------------------------

/**
 * Tell peers where the pointer is.
 *
 * Sent directly rather than through `flush`: the outbox exists so that work
 * drawn offline is replayed on reconnect, and a cursor position from thirty
 * seconds ago is not work — replaying it would move a pointer that has since
 * gone somewhere else.
 */
function reportCursor(x, y) {
  if (state.socket?.readyState !== WebSocket.OPEN) return;
  if (!state.presence.shouldReport(x, y, performance.now())) return;
  state.socket.send(JSON.stringify({ type: "presence", x, y }));
}

// -- input -----------------------------------------------------------------

function commitDraft() {
  if (!state.draft) return;
  const draft = state.draft;
  state.draft = null;

  if (draft.kind === "freedraw") {
    if ((draft.points?.length ?? 0) < 2) return;
    state.engine.exec(state.board, {
      cmd: "stroke",
      points: draft.points,
      stroke: draft.stroke,
      stroke_width: draft.stroke_width,
    });
  } else {
    // Ignore accidental click-sized shapes.
    if (Math.abs(draft.w) < 3 && Math.abs(draft.h) < 3) return;
    state.engine.exec(state.board, {
      cmd: "add",
      kind: draft.kind,
      x: draft.x,
      y: draft.y,
      w: draft.w,
      h: draft.h,
      stroke: draft.stroke,
      fill: draft.fill,
      stroke_width: draft.stroke_width,
    });
  }
  flush();
  sceneChanged();
}

canvas.addEventListener("pointerdown", (event) => {
  if (state.board === null) return;
  // Throws if the pointer was already released, or for a synthetic event.
  // Losing capture degrades the drag; it must not abort the whole handler.
  try {
    canvas.setPointerCapture(event.pointerId);
  } catch {
    /* capture is an optimisation, not a requirement */
  }
  hint.classList.add("gone");

  const [x, y] = toScene(event.clientX, event.clientY);

  // Middle button or space-drag pans regardless of the active tool.
  if (event.button === 1 || event.shiftKey) {
    state.pan = { startX: event.clientX, startY: event.clientY, ...state.view };
    return;
  }

  if (state.tool === "eraser") {
    const target = hitTest(x, y);
    if (target) {
      state.engine.exec(state.board, { cmd: "delete", id: target.id });
      flush();
      sceneChanged();
    }
    return;
  }

  if (state.tool === "select") {
    const target = hitTest(x, y);
    if (target) {
      const box = bounds(target);
      state.drag = {
        id: target.id,
        offsetX: x - target.x,
        offsetY: y - target.y,
        lastCommit: 0,
      };
    } else {
      state.pan = { startX: event.clientX, startY: event.clientY, ...state.view };
    }
    return;
  }

  const stroke = pack(state.colour);
  state.draft =
    state.tool === "freedraw"
      ? { kind: "freedraw", x, y, w: 0, h: 0, points: [[x, y]], stroke, fill: 0, stroke_width: 2 }
      : { kind: state.tool, x, y, w: 0, h: 0, stroke, fill: 0, stroke_width: 2 };
  invalidate();
});

canvas.addEventListener("pointermove", (event) => {
  if (state.pan) {
    state.view.x = state.pan.x + (event.clientX - state.pan.startX);
    state.view.y = state.pan.y + (event.clientY - state.pan.startY);
    invalidate();
    return;
  }

  const [x, y] = toScene(event.clientX, event.clientY);
  reportCursor(x, y);

  if (state.drag) {
    const now = performance.now();
    // Throttled rather than per-frame: peers should see the drag happening,
    // without one drag becoming a thousand operations in the durable log.
    if (now - state.drag.lastCommit > COMMIT_INTERVAL_MS) {
      state.drag.lastCommit = now;
      state.engine.exec(state.board, {
        cmd: "move",
        id: state.drag.id,
        x: x - state.drag.offsetX,
        y: y - state.drag.offsetY,
      });
      flush();
      sceneChanged();
    }
    return;
  }

  if (!state.draft) return;
  if (state.draft.kind === "freedraw") {
    state.draft.points.push([x, y]);
  } else {
    state.draft.w = x - state.draft.x;
    state.draft.h = y - state.draft.y;
  }
  invalidate();
});

function endPointer(event) {
  try {
    if (canvas.hasPointerCapture?.(event.pointerId)) {
      canvas.releasePointerCapture(event.pointerId);
    }
  } catch {
    /* already released */
  }
  if (state.pan) {
    state.pan = null;
    return;
  }
  if (state.drag) {
    const [x, y] = toScene(event.clientX, event.clientY);
    state.engine.exec(state.board, {
      cmd: "move",
      id: state.drag.id,
      x: x - state.drag.offsetX,
      y: y - state.drag.offsetY,
    });
    state.drag = null;
    flush();
    sceneChanged();
    return;
  }
  commitDraft();
}

canvas.addEventListener("pointerup", endPointer);
canvas.addEventListener("pointercancel", endPointer);

canvas.addEventListener(
  "wheel",
  (event) => {
    event.preventDefault();
    const rect = canvas.getBoundingClientRect();
    const px = event.clientX - rect.left;
    const py = event.clientY - rect.top;
    const factor = Math.exp(-event.deltaY * 0.0015);
    const next = Math.min(6, Math.max(0.15, state.view.scale * factor));
    // Keep the point under the cursor fixed while zooming.
    state.view.x = px - ((px - state.view.x) * next) / state.view.scale;
    state.view.y = py - ((py - state.view.y) * next) / state.view.scale;
    state.view.scale = next;
    invalidate();
  },
  { passive: false },
);

// -- export ----------------------------------------------------------------

/**
 * Whole board, not the viewport.
 *
 * An export is the drawing, not a screenshot of where the author happened to be
 * looking — so it is cropped to the content and rendered at its own scale,
 * ignoring pan and zoom entirely. The grid is left out for the same reason: it
 * is an affordance for drawing, not something anyone means to send.
 */
const EXPORT_PADDING = 16;
const EXPORT_PIXEL_SCALE = 2; // a PNG that still reads on a high-density screen

/**
 * Always light, never the page background.
 *
 * A file that comes out dark because the author's laptop was in dark mode is a
 * file that renders differently for whoever receives it. Every colour in the
 * palette is chosen to read on white, so white is what they are exported on.
 */
const EXPORT_BACKGROUND = "#ffffff";

/** `tenant-a:board` is a legal scope and an illegal Windows filename. */
function exportName(extension) {
  const stamp = new Date().toISOString().slice(0, 19).replace(/[:T]/g, "-");
  const scope = state.scope.replace(/[^a-zA-Z0-9._-]/g, "-");
  return `${scope}-${stamp}.${extension}`;
}

function download(blob, filename) {
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = filename;
  link.click();
  // Revoked on the next turn: revoking synchronously can beat the navigation
  // the click just started.
  setTimeout(() => URL.revokeObjectURL(url), 0);
}

function exportSvg() {
  const svg = toSvg(state.scene, {
    padding: EXPORT_PADDING,
    background: EXPORT_BACKGROUND,
    title: `k-board ${state.scope}`,
  });
  if (svg === null) return setStatus("offline", "nothing to export");
  download(new Blob([svg], { type: "image/svg+xml" }), exportName("svg"));
}

function exportPng() {
  const box = sceneBounds(state.scene);
  if (box === null) return setStatus("offline", "nothing to export");

  const width = box.w + EXPORT_PADDING * 2;
  const height = box.h + EXPORT_PADDING * 2;
  const sheet = document.createElement("canvas");
  sheet.width = Math.max(1, Math.round(width * EXPORT_PIXEL_SCALE));
  sheet.height = Math.max(1, Math.round(height * EXPORT_PIXEL_SCALE));

  const paper = sheet.getContext("2d");
  paper.fillStyle = EXPORT_BACKGROUND;
  paper.fillRect(0, 0, sheet.width, sheet.height);
  paper.setTransform(
    EXPORT_PIXEL_SCALE,
    0,
    0,
    EXPORT_PIXEL_SCALE,
    -(box.x - EXPORT_PADDING) * EXPORT_PIXEL_SCALE,
    -(box.y - EXPORT_PADDING) * EXPORT_PIXEL_SCALE,
  );

  // The same drawing routine the screen uses, so the file cannot disagree
  // with what the author was looking at.
  for (const item of state.scene) drawShape(paper, item);

  sheet.toBlob((blob) => {
    if (blob) download(blob, exportName("png"));
    else setStatus("offline", "export failed");
  }, "image/png");
}

// -- chrome ----------------------------------------------------------------

function selectTool(tool) {
  state.tool = tool;
  for (const button of document.querySelectorAll(".tool")) {
    button.setAttribute("aria-pressed", String(button.dataset.tool === tool));
  }
  canvas.classList.toggle("select-mode", tool === "select");
}

for (const button of document.querySelectorAll(".tool")) {
  button.addEventListener("click", () => selectTool(button.dataset.tool));
}

for (const swatch of document.querySelectorAll(".swatch")) {
  swatch.addEventListener("click", () => {
    state.colour = swatch.dataset.colour;
    for (const other of document.querySelectorAll(".swatch")) {
      other.setAttribute("aria-checked", String(other === swatch));
    }
  });
}

undoButton.addEventListener("click", () => stepHistory(true));
redoButton.addEventListener("click", () => stepHistory(false));
pngButton.addEventListener("click", exportPng);
svgButton.addEventListener("click", exportSvg);

document.getElementById("clear").addEventListener("click", () => {
  if (state.board === null) return;
  state.engine.exec(state.board, { cmd: "clear" });
  flush();
  sceneChanged();
});

const SHORTCUTS = {
  v: "select",
  r: "rectangle",
  o: "ellipse",
  d: "diamond",
  a: "arrow",
  p: "freedraw",
  e: "eraser",
};

window.addEventListener("keydown", (event) => {
  // Undo shortcuts are the one place a modifier is expected, so they are
  // handled before the plain tool shortcuts bail out on one.
  if ((event.metaKey || event.ctrlKey) && !event.altKey) {
    const key = event.key.toLowerCase();
    if (key === "z") {
      event.preventDefault();
      stepHistory(!event.shiftKey);
      return;
    }
    if (key === "y") {
      event.preventDefault();
      stepHistory(false);
      return;
    }
  }
  if (event.metaKey || event.ctrlKey || event.altKey) return;
  const tool = SHORTCUTS[event.key.toLowerCase()];
  if (tool) selectTool(tool);
});

window.addEventListener("hashchange", () => location.reload());
window.addEventListener("resize", invalidate);

// -- start -----------------------------------------------------------------

(async function start() {
  if (!location.hash) history.replaceState(null, "", `#${state.scope}`);
  try {
    state.engine = await loadEngine("./kboard.wasm");
  } catch (error) {
    setStatus("offline", "engine failed to load");
    hint.textContent = "Build the engine: cargo build -p kboard-ffi --target wasm32-unknown-unknown --release";
    hint.classList.remove("gone");
    console.error(error);
    return;
  }

  connect();

  // Local-first: if the server never answers, open a board anyway with a
  // locally chosen actor id and keep working. Everything queues for reconnect.
  setTimeout(() => {
    if (state.board === null) {
      ensureBoard(crypto.getRandomValues(new Uint32Array(1))[0]);
      setStatus("offline", "offline · local board");
      sceneChanged();
    }
  }, 2500);

  requestAnimationFrame(frame);
})();
