/**
 * k-board web client.
 *
 * Every tab is an independent replica. It holds its own copy of the document in
 * wasm, applies local edits immediately, and reconciles with peers through the
 * server. Nothing here resolves conflicts — the engine does, identically on
 * every replica and on the server, because it is literally the same build.
 */

import { loadEngine } from "./kboard.js";
import {
  LINE_HEIGHT,
  STICKY_PADDING,
  bounds,
  drawShape,
  fontFor,
  intoLocal,
  pack,
  sceneBounds,
  toSvg,
  unpack,
} from "./scene.js?v=1";
import * as transform from "./transform.js";
import { Presence, drawCursors, peerName } from "./presence.js";
import * as clipboard from "./clipboard.js?v=1";
import { OutboxLimitError, createClientOutbox } from "./outbox.js";
import { resolveRuntimeConfiguration } from "./runtime-config.js";

const runtime = await resolveRuntimeConfiguration();

const canvas = document.getElementById("canvas");
const context = canvas.getContext("2d");
const statusDot = document.getElementById("dot");
const statusText = document.getElementById("statusText");
const hint = document.getElementById("hint");
const a11yList = document.getElementById("a11yList");
const peerList = document.getElementById("peerList");
const selectionText = document.getElementById("selectionText");
const editor = document.getElementById("editor");
const undoButton = document.getElementById("undo");
const redoButton = document.getElementById("redo");
const pngButton = document.getElementById("exportPng");
const svgButton = document.getElementById("exportSvg");
const recoveryButton = document.getElementById("exportRecovery");
const openStandaloneButton = document.getElementById("openStandalone");
const zoomOutButton = document.getElementById("zoomOut");
const zoomResetButton = document.getElementById("zoomReset");
const zoomInButton = document.getElementById("zoomIn");
const zoomFitButton = document.getElementById("zoomFit");

const COMMIT_INTERVAL_MS = 50; // live-drag update rate sent to peers
const MIN_ZOOM = 0.15;
const MAX_ZOOM = 6;

const state = {
  engine: null,
  board: null,
  actor: null,
  scope: runtime.scope,
  tool: "select",
  colour: "#1e1e1e",
  fill: "none",
  strokeWidth: 2,
  opacity: 1,
  view: { x: 0, y: 0, scale: 1 },
  scene: [],
  draft: null,
  pan: null,
  socket: null,
  bootstrapToken: runtime.accessToken,
  outbox: null,
  replica: null,
  negotiated: false,
  inFlight: null,
  ackTimer: null,
  retryTimer: null,
  reconnectAttempt: 0,
  captureChain: Promise.resolve(),
  localOnlyOps: [],
  everConnected: false,
  dirty: true,
  // Throwaway. Never reaches the engine, never reaches the log.
  presence: new Presence(),
  /// Ids, not elements: the scene is replaced wholesale on every change, so
  /// holding elements would mean holding stale copies of them.
  selection: new Set(),
  gesture: null,
  marquee: null,
  pointer: null,
  // Only reached when the system clipboard is unavailable or refused.
  localClipboard: {},
  // The element being typed into, or null. Never in `scene`: an in-progress
  // edit belongs to one person until they finish it.
  editing: null,
};

// -- geometry --------------------------------------------------------------

function hitTest(sceneX, sceneY) {
  const slack = 6 / state.view.scale;
  // Reverse: topmost element in paint order wins.
  for (let index = state.scene.length - 1; index >= 0; index -= 1) {
    const item = state.scene[index];
    const box = bounds(item);
    // Tested in the element's own frame. A turned element's box is not
    // axis-aligned on the board, and comparing against the board's axes would
    // make the clickable region drift away from the drawn one.
    const [x, y] = intoLocal(item, sceneX, sceneY);
    if (
      x >= box.x - slack &&
      x <= box.x + box.w + slack &&
      y >= box.y - slack &&
      y <= box.y + box.h + slack
    ) {
      return item;
    }
  }
  return null;
}

/** The selected elements, read from the scene so they cannot go stale. */
function selected() {
  return state.scene.filter((item) => state.selection.has(item.id));
}

/** The one selected element, or null. Some actions only make sense on one. */
function onlySelected() {
  const items = selected();
  return items.length === 1 ? items[0] : null;
}

/**
 * Replace the selection.
 *
 * Announced as well as drawn: an outline says nothing to a screen reader, and
 * every shortcut in this file acts on whatever is selected.
 */
function select(ids) {
  const next = new Set(ids === null ? [] : [ids].flat().filter(Boolean));
  if (next.size === state.selection.size && [...next].every((id) => state.selection.has(id))) {
    return;
  }
  state.selection = next;
  adoptStyleOfSelection();
  describeSelection();
  invalidate();
}

/**
 * Show the selection's own style in the controls.
 *
 * Without this the toolbar keeps describing the last thing drawn, and someone
 * nudging the opacity slider to see the current value would instead set it.
 */
function adoptStyleOfSelection() {
  const only = onlySelected();
  if (!only) return;
  state.strokeWidth = only.stroke_width || 2;
  state.opacity = only.opacity ?? 1;
  opacityInput.value = String(Math.round(state.opacity * 100));
  markChecked(widthButtons, widthButtons.find((b) => Number(b.dataset.width) === state.strokeWidth));
}

/** Add or remove one element, for shift-clicking a selection together. */
function toggleSelected(id) {
  const next = new Set(state.selection);
  if (!next.delete(id)) next.add(id);
  select([...next]);
}

/** The box the handles hang off, or null when nothing is selected. */
function selectionBox() {
  return transform.unionBounds(selected());
}

function toScene(clientX, clientY) {
  const rect = canvas.getBoundingClientRect();
  return [
    (clientX - rect.left - state.view.x) / state.view.scale,
    (clientY - rect.top - state.view.y) / state.view.scale,
  ];
}

// -- text ------------------------------------------------------------------

const DEFAULT_FONT_SIZE = 20;
const STICKY_MIN_WIDTH = 180;
const STICKY_MIN_HEIGHT = 120;
const STICKY_DEFAULT_FILL = "#ffec99";

/**
 * Measure a label with the canvas that will draw it.
 *
 * The engine stores the box rather than deriving it, because measuring text
 * needs a font engine and the engine deliberately has none. Measuring here —
 * with the same context, at the same size, in the same font — is the only way
 * the stored box is the box the words actually occupy.
 */
function measureText(text, size) {
  context.save();
  context.font = fontFor(size);
  const rows = text.split("\n");
  const width = Math.max(...rows.map((row) => context.measureText(row).width), 0);
  context.restore();
  return { w: Math.ceil(width), h: Math.ceil(rows.length * size * LINE_HEIGHT) };
}

/**
 * Open the overlay editor over a point on the board.
 *
 * A real `<textarea>` rather than a canvas-drawn caret: it brings selection,
 * an IME, spellcheck, screen-reader support, and the platform's own text
 * conventions with it. Reimplementing any of those on a canvas would be worse
 * in every case and wrong in most of them.
 */
function beginTextEdit({
  id = null,
  x,
  y,
  w = 0,
  h = 0,
  text = "",
  size = DEFAULT_FONT_SIZE,
  role = null,
  stroke = null,
  fill = null,
  stroke_width = 2,
  opacity = 1,
}) {
  const sticky = role === "sticky";
  const editingStroke = stroke ?? pack(state.colour);
  const editingFill = fill ?? pack(state.fill === "none" ? STICKY_DEFAULT_FILL : state.fill);
  state.editing = {
    id,
    x,
    y,
    w,
    h,
    size,
    role,
    stroke: editingStroke,
    fill: editingFill,
    stroke_width,
    opacity,
  };
  editor.value = text;
  editor.style.font = fontFor(size * state.view.scale);
  editor.style.lineHeight = String(LINE_HEIGHT);
  editor.style.left = `${x * state.view.scale + state.view.x}px`;
  editor.style.top = `${y * state.view.scale + state.view.y}px`;
  editor.style.color = unpack(editingStroke);
  editor.classList.toggle("sticky-editor", sticky);
  if (sticky) {
    editor.style.setProperty("--sticky-fill", unpack(editingFill));
    editor.style.setProperty("--sticky-stroke", unpack(editingStroke));
  } else {
    editor.style.removeProperty("--sticky-fill");
    editor.style.removeProperty("--sticky-stroke");
  }
  editor.hidden = false;
  fitEditor();
  editor.focus();
  editor.setSelectionRange(text.length, text.length);
  // The element being edited is hidden underneath, so the words are not drawn
  // twice at slightly different positions.
  invalidate();
}

/** Grow the textarea with its content, so nothing is typed out of sight. */
function fitEditor() {
  const size = state.editing.size * state.view.scale;
  const { w, h } = measureText(editor.value || " ", size);
  if (state.editing.role === "sticky") {
    const padding = STICKY_PADDING * state.view.scale;
    editor.style.width = `${Math.max(state.editing.w * state.view.scale, w + padding * 2, STICKY_MIN_WIDTH * state.view.scale)}px`;
    editor.style.height = `${Math.max(state.editing.h * state.view.scale, h + padding * 2, STICKY_MIN_HEIGHT * state.view.scale)}px`;
  } else {
    editor.style.width = `${w + size}px`;
    editor.style.height = `${h}px`;
  }
}

function editTextItem(item) {
  beginTextEdit({
    id: item.id,
    ...bounds(item),
    text: item.text,
    size: item.font_size,
    role: item.role,
    stroke: item.stroke,
    fill: item.fill,
    stroke_width: item.stroke_width,
    opacity: item.opacity,
  });
}

/**
 * Commit what was typed, or discard it.
 *
 * Emptying an existing label deletes it. The alternative is an element with
 * nothing to draw, which cannot be clicked to be fixed and cannot be seen to
 * be deleted.
 */
function endTextEdit() {
  const editing = state.editing;
  if (!editing) return;
  state.editing = null;
  editor.hidden = true;
  const text = editor.value.replace(/\s+$/, "");
  editor.value = "";
  canvas.focus();

  if (state.board !== null) {
    const measured = measureText(text, editing.size);
    const sticky = editing.role === "sticky";
    const w = sticky
      ? Math.max(editing.w, measured.w + STICKY_PADDING * 2, STICKY_MIN_WIDTH)
      : measured.w;
    const h = sticky
      ? Math.max(editing.h, measured.h + STICKY_PADDING * 2, STICKY_MIN_HEIGHT)
      : measured.h;
    if (text === "") {
      if (editing.id) state.engine.exec(state.board, { cmd: "delete", id: editing.id });
    } else if (editing.id) {
      state.engine.exec(state.board, { cmd: "set_text", id: editing.id, text, w, h });
    } else if (sticky) {
      const id = state.engine.exec(state.board, {
        cmd: "sticky",
        x: editing.x,
        y: editing.y,
        w,
        h,
        text,
        font_size: editing.size,
        stroke: editing.stroke,
        fill: editing.fill,
        stroke_width: editing.stroke_width,
        opacity: editing.opacity,
      });
      select(id);
    } else {
      const id = state.engine.exec(state.board, {
        cmd: "text",
        x: editing.x,
        y: editing.y,
        w,
        h,
        text,
        font_size: editing.size,
        stroke: pack(state.colour),
      });
      select(id);
    }
    flush();
  }
  sceneChanged();
}

// -- rendering -------------------------------------------------------------

/** The view changed (draft, pan, zoom). Repaint; the document is unchanged. */
function invalidate() {
  state.dirty = true;
}

function refreshZoomControl() {
  zoomResetButton.textContent = `${Math.round(state.view.scale * 100)}%`;
  zoomOutButton.disabled = state.view.scale <= MIN_ZOOM;
  zoomInButton.disabled = state.view.scale >= MAX_ZOOM;
}

/** Change magnification while keeping one viewport point anchored. */
function setZoom(scale, anchor = null) {
  const next = Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, scale));
  const point = anchor ?? { x: canvas.clientWidth / 2, y: canvas.clientHeight / 2 };
  state.view.x = point.x - ((point.x - state.view.x) * next) / state.view.scale;
  state.view.y = point.y - ((point.y - state.view.y) * next) / state.view.scale;
  state.view.scale = next;
  refreshZoomControl();
  invalidate();
}

function fitContent() {
  const box = sceneBounds(state.scene);
  if (box === null) {
    state.view = { x: 0, y: 0, scale: 1 };
    refreshZoomControl();
    invalidate();
    return;
  }

  const padding = 56;
  const width = Math.max(1, canvas.clientWidth - padding * 2);
  const height = Math.max(1, canvas.clientHeight - padding * 2);
  const scale = Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, Math.min(width / Math.max(box.w, 1), height / Math.max(box.h, 1))));
  state.view.scale = scale;
  state.view.x = (canvas.clientWidth - box.w * scale) / 2 - box.x * scale;
  state.view.y = (canvas.clientHeight - box.h * scale) / 2 - box.y * scale;
  refreshZoomControl();
  invalidate();
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
  // A peer may have deleted something that was selected. Holding the id would
  // leave Delete and Copy pointed at what is no longer there.
  for (const id of state.selection) {
    if (!state.scene.some((item) => item.id === id)) state.selection.delete(id);
  }
  describeSelection();
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
  zoomFitButton.disabled = empty;
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
  context.fillStyle = style.getPropertyValue("--canvas-grid").trim() || "#eee";
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

  for (const item of state.scene) {
    // Drawn by the textarea instead, so the words do not appear twice a pixel
    // apart while someone is typing them.
    if (item.id === state.editing?.id) continue;
    drawShape(context, item);
  }
  if (state.draft) drawShape(context, state.draft);

  drawSelection();
  if (state.marquee) drawMarquee(state.marquee);

  // Above the drawing and outside the scene transform: a cursor is a pointer,
  // not an object on the board.
  const peers = state.presence.list();
  if (peers.length > 0) drawCursors(context, peers, { ...state.view, ratio });
}

/**
 * Outline the selection and hang the handles off it.
 *
 * Drawn in scene space so it tracks the shapes, but with widths divided by the
 * zoom so they stay one size at any magnification — an outline that thickens as
 * you zoom in stops reading as an annotation and starts looking drawn on.
 */
function drawSelection() {
  const items = selected();
  if (items.length === 0) return;
  const scale = state.view.scale;
  const box = transform.unionBounds(items);

  context.save();
  context.strokeStyle = "#1971c2";
  context.lineWidth = 1.5 / scale;

  // Each member is outlined as well as the group, so a selection of three
  // scattered shapes does not read as one large empty rectangle.
  if (items.length > 1) {
    context.setLineDash([3 / scale, 3 / scale]);
    context.globalAlpha = 0.5;
    for (const item of items) {
      const own = transform.unionBounds([item]);
      context.strokeRect(own.x, own.y, own.w, own.h);
    }
    context.globalAlpha = 1;
  }

  const pad = 4 / scale;
  context.setLineDash([5 / scale, 4 / scale]);
  context.strokeRect(box.x - pad, box.y - pad, box.w + pad * 2, box.h + pad * 2);

  context.setLineDash([]);
  const size = 7 / scale;
  for (const handle of transform.handles(box)) {
    context.beginPath();
    if (handle.name === "rotate") {
      // Round, and joined to the box by a stem, so it reads as a different
      // kind of control rather than a ninth way to resize.
      context.moveTo(box.x + box.w / 2, box.y);
      context.lineTo(handle.x, handle.y);
      context.stroke();
      context.beginPath();
      context.arc(handle.x, handle.y, size / 2, 0, Math.PI * 2);
    } else {
      context.rect(handle.x - size / 2, handle.y - size / 2, size, size);
    }
    context.fillStyle = "#ffffff";
    context.fill();
    context.stroke();
  }
  context.restore();
}

function drawMarquee(box) {
  const scale = state.view.scale;
  context.save();
  context.strokeStyle = "#1971c2";
  context.fillStyle = "rgba(25,113,194,0.08)";
  context.lineWidth = 1 / scale;
  context.setLineDash([4 / scale, 3 / scale]);
  context.fillRect(box.x, box.y, box.w, box.h);
  context.strokeRect(box.x, box.y, box.w, box.h);
  context.restore();
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
      entry.textContent = `${semanticKind(item)} ${index + 1}, ${describeItem(item)}`;
      return entry;
    }),
  );
}

function semanticKind(item) {
  return item.role === "sticky" ? "sticky note" : item.kind;
}

function describeSelection() {
  const items = selected();
  let readout = "Nothing selected.";
  if (items.length === 1) {
    readout = `Selected: ${semanticKind(items[0])}, ${describeItem(items[0])}.`;
  } else if (items.length > 1) {
    // Counted by kind rather than listed: "seven rectangles" is what someone
    // needs to hear, and seven near-identical sentences is not.
    const tally = new Map();
    for (const item of items) {
      const kind = semanticKind(item);
      tally.set(kind, (tally.get(kind) ?? 0) + 1);
    }
    const parts = [...tally].map(([kind, count]) => `${count} ${kind}${count === 1 ? "" : "s"}`);
    readout = `Selected ${items.length} elements: ${parts.join(", ")}.`;
  }
  if (selectionText.textContent !== readout) selectionText.textContent = readout;
}

function describeItem(item) {
  const box = bounds(item);
  let where = `at ${Math.round(box.x)}, ${Math.round(box.y)}, ${Math.round(
    box.w,
  )} by ${Math.round(box.h)}`;

  // In degrees, because nobody reads radians aloud. Omitted when upright rather
  // than announcing "rotated 0 degrees" on every element on the board. Without
  // this a turned shape reads exactly like one that was never touched — the box
  // does not change when a shape rotates about its own centre.
  const turn = Math.round((((item.angle || 0) * 180) / Math.PI) % 360);
  if (turn !== 0) where += `, rotated ${turn} degrees`;

  const alpha = item.opacity ?? 1;
  if (alpha < 1) where += `, ${Math.round(alpha * 100)}% opacity`;

  // The words are the content. Reading out a label's dimensions and not what it
  // says would describe the box and omit the point of it.
  return item.text ? `"${item.text.replace(/\n/g, " ")}", ${where}` : where;
}

// -- sync ------------------------------------------------------------------

function setStatus(kind, text) {
  statusDot.className = `dot ${kind}`;
  statusText.textContent = text;
  runtime.bridge?.emit("status", { kind, text });
}

function ensureBoard(actor) {
  if (state.board !== null) return;
  state.actor = actor;
  state.board = state.engine.open(state.scope, actor);
}

function outboxSummary() {
  return state.outbox?.snapshot() ?? {
    pending: 0,
    sending: 0,
    refused: 0,
    batches: 0,
    operations: state.localOnlyOps.length,
    durableStorage: false,
    limitation: "outbox is starting",
  };
}

function durabilityStatus(prefix) {
  const summary = outboxSummary();
  const local = summary.operations + state.localOnlyOps.length;
  const parts = [prefix];
  if (summary.sending > 0) parts.push(`${summary.sending} sending`);
  if (local > 0) parts.push(`${local} awaiting durable ack`);
  if (summary.refused > 0) parts.push(`${summary.refused} recovery required`);
  if (!summary.durableStorage) parts.push("this tab only");
  return parts.join(" · ");
}

function armOutboxRetry() {
  clearTimeout(state.retryTimer);
  const retryAt = state.outbox?.nextRetryAt();
  if (retryAt === null || retryAt === undefined) return;
  state.retryTimer = setTimeout(() => void sendNextBatch(), Math.max(0, retryAt - Date.now()));
}

async function sendNextBatch() {
  if (
    state.inFlight !== null ||
    !state.negotiated ||
    state.socket?.readyState !== WebSocket.OPEN ||
    state.outbox === null
  ) {
    return;
  }
  const batch = state.outbox.nextReady();
  if (!batch) {
    armOutboxRetry();
    return;
  }
  if (!(await state.outbox.markSending(batch.id))) return;
  state.inFlight = batch.id;
  setStatus("live", durabilityStatus(`live · ${state.scope}`));
  try {
    state.socket.send(JSON.stringify({ type: "ops", batch: batch.id, ops: batch.operations }));
  } catch {
    state.inFlight = null;
    await state.outbox.resetSending("socket send failed before acknowledgement");
    state.socket.close();
    return;
  }
  clearTimeout(state.ackTimer);
  state.ackTimer = setTimeout(() => {
    if (state.inFlight === batch.id) state.socket?.close();
  }, 10_000);
}

function flush() {
  if (state.board === null || state.outbox === null) return;
  const ops = state.engine.pending(state.board);
  if (ops.length > 0) state.localOnlyOps.push(...ops);
  state.captureChain = state.captureChain.then(async () => {
    if (state.localOnlyOps.length > 0) {
      const count = state.localOnlyOps.length;
      const capture = state.localOnlyOps.slice(0, count);
      try {
        await state.outbox.capture(capture);
        state.localOnlyOps.splice(0, count);
      } catch (error) {
        recoveryButton.disabled = false;
        const reason =
          error instanceof OutboxLimitError
            ? "offline queue full · export recovery copy"
            : "browser storage failed · edits remain local-only";
        setStatus("offline", durabilityStatus(reason));
        return;
      }
    }
    if (!state.negotiated || state.socket?.readyState !== WebSocket.OPEN) {
      setStatus("offline", durabilityStatus("offline · edits retained"));
    }
    await sendNextBatch();
  });
}

function connect() {
  const url = new URL(`ws/${encodeURIComponent(state.scope)}`, runtime.baseUrl);
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  // Offered as a subprotocol rather than a query parameter: a URL ends up in
  // server logs, browser history, and referrers. An open server ignores it.
  const token = state.bootstrapToken;
  const socket = token
    ? new WebSocket(url.href, ["kboard.v2", `kboard.token.${token}`])
    : new WebSocket(url.href, ["kboard.v2"]);
  state.socket = socket;
  state.negotiated = false;
  setStatus("", "connecting");

  socket.onopen = () => {
    state.everConnected = true;
    socket.send(JSON.stringify({ type: "hello", version: 2, replica: state.replica }));
    setStatus("", durabilityStatus("negotiating"));
  };

  socket.onmessage = (event) => {
    let message;
    try {
      message = JSON.parse(event.data);
    } catch {
      return;
    }

    if (message.type === "init") {
      if (message.version !== 2 || message.replica !== state.replica || message.actor !== state.actor) {
        setStatus("offline", "protocol identity mismatch · edits retained");
        socket.close();
        return;
      }
      ensureBoard(message.actor);
      state.engine.load(state.board, JSON.stringify(message.doc));
      state.negotiated = true;
      state.reconnectAttempt = 0;
      setStatus("live", durabilityStatus(`live · ${state.scope} · you are #${message.actor}`));
      flush();
      sceneChanged();
    } else if (message.type === "ack") {
      const acknowledged = message.batch;
      state.captureChain = state.captureChain.then(async () => {
        const outcome = await state.outbox.acknowledge(acknowledged, message.sequence);
        if (state.inFlight === acknowledged) {
          state.inFlight = null;
          clearTimeout(state.ackTimer);
        }
        if (outcome !== "unknown") {
          setStatus("live", durabilityStatus(`durable · sequence ${message.sequence}`));
        }
        await sendNextBatch();
      });
    } else if (message.type === "refused") {
      state.captureChain = state.captureChain.then(async () => {
        const outcome = await state.outbox.refuse(
          message.batch,
          message.code,
          message.retryable,
        );
        if (state.inFlight === message.batch) {
          state.inFlight = null;
          clearTimeout(state.ackTimer);
        }
        if (outcome === "permanent") {
          recoveryButton.disabled = false;
          setStatus("offline", durabilityStatus(`refused ${message.code} · export recovery copy`));
        } else {
          setStatus("offline", durabilityStatus(`retrying after ${message.code}`));
          armOutboxRetry();
        }
      });
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
    state.negotiated = false;
    state.inFlight = null;
    clearTimeout(state.ackTimer);
    state.captureChain = state.captureChain.then(() =>
      state.outbox.resetSending("connection closed before durable acknowledgement"),
    );
    // 1008/1006 after an immediate close usually means the handshake was
    // refused. Retrying forever against a rejected token looks like a network
    // problem to the user, so name it.
    if (!state.everConnected) {
      setStatus("offline", durabilityStatus("offline · not authorised or unreachable"));
      return;
    }
    // Nobody is reachable, so nobody's cursor is current. Leaving them on
    // screen would show a room full of people who cannot see you.
    state.presence.clear();
    peersChanged();
    setStatus(
      "offline",
      durabilityStatus(
        event.code === 1008 ? "authorization expired · refresh access" : "offline · edits retained",
      ),
    );
    const ceiling = Math.min(30_000, 600 * 2 ** Math.min(state.reconnectAttempt, 6));
    const delay = Math.floor(ceiling / 2 + Math.random() * (ceiling / 2));
    state.reconnectAttempt += 1;
    clearTimeout(state.retryTimer);
    state.retryTimer = setTimeout(connect, delay);
  };

  socket.onerror = () => socket.close();
}

// -- clipboard -------------------------------------------------------------

/**
 * Add elements to the board and select the last one.
 *
 * Selecting the result is what makes a paste followed by a drag feel like one
 * gesture instead of two.
 */
function insert(elements) {
  if (state.board === null || elements.length === 0) return;
  const added = [];
  for (const element of elements) {
    added.push(state.engine.exec(state.board, clipboard.toCommand(element)));
  }
  flush();
  sceneChanged();
  // Everything that was added, so a paste of five shapes can immediately be
  // dragged as the five shapes that were pasted.
  select(added);
}

async function copySelection({ cut = false } = {}) {
  const chosen = selected();
  if (chosen.length === 0) return;

  const where = await clipboard.writeText(clipboard.serialise(chosen), state.localClipboard);
  if (cut) {
    for (const item of chosen) state.engine.exec(state.board, { cmd: "delete", id: item.id });
    flush();
    sceneChanged();
  }
  setStatus(
    "live",
    where === "system"
      ? `${cut ? "cut" : "copied"} · ${state.scope}`
      : `${cut ? "cut" : "copied"} · this tab only`,
  );
}

async function paste() {
  if (state.board === null) return;
  const elements = clipboard.parse(await clipboard.readText(state.localClipboard));
  // Not an error: the clipboard is shared with every other application, and
  // most of what is on it was never meant for this board.
  if (!elements) return;
  insert(clipboard.place(elements, state.pointer));
}

/** A copy without involving the clipboard, so it cannot clobber what is on it. */
function duplicateSelection() {
  const chosen = selected();
  if (chosen.length === 0) return;
  const copied = clipboard.parse(clipboard.serialise(chosen));
  if (copied) insert(clipboard.place(copied, null));
}

function deleteSelection() {
  const chosen = selected();
  if (chosen.length === 0) return;
  for (const item of chosen) state.engine.exec(state.board, { cmd: "delete", id: item.id });
  flush();
  sceneChanged();
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

// -- style -----------------------------------------------------------------

/** Fully opaque black with the alpha byte set, or 0 for "no fill". */
function packOrNone(colour) {
  return colour === "none" ? 0 : pack(colour);
}

/** The look a newly drawn shape takes. */
function currentStyle() {
  return {
    stroke: pack(state.colour),
    fill: packOrNone(state.fill),
    stroke_width: state.strokeWidth,
    opacity: state.opacity,
  };
}

/**
 * Change a style property.
 *
 * Applied to the selection *and* remembered for the next shape. Separating
 * those is the arrangement where someone restyles a shape and then wonders why
 * the next one came out with the old colour.
 *
 * Only the changed field is sent. Resending the others would overwrite a peer's
 * concurrent restyle with values this client merely happens to be holding.
 */
function applyStyle(change) {
  const chosen = selected();
  for (const item of chosen) {
    state.engine.exec(state.board, { cmd: "style", id: item.id, ...change });
  }
  if (chosen.length > 0) {
    flush();
    sceneChanged();
  }
}

/** Reflect a radio group's selection, since these are buttons rather than inputs. */
function markChecked(group, chosen) {
  for (const button of group) button.setAttribute("aria-checked", String(button === chosen));
}

// -- gestures --------------------------------------------------------------

/**
 * Write a transform to the engine.
 *
 * One `geometry` command per element rather than a move and a resize and a
 * rotate: they only make sense together, and sending them apart would put a
 * half-transformed shape in the log and three entries in the history where the
 * user made one gesture.
 *
 * It also fixes freehand, whose visible geometry is its path — a `move` that
 * wrote only x and y left the stroke exactly where it was while its box walked
 * off without it.
 */
function applyGeometry(changes) {
  for (const change of changes) {
    state.engine.exec(state.board, { cmd: "geometry", ...change });
  }
  flush();
  sceneChanged();
}

/** Begin a drag on the selection: move, resize, or rotate. */
function beginGesture(kind, x, y, handle = null) {
  const items = selected();
  const box = transform.unionBounds(items);
  state.gesture = {
    kind,
    handle,
    box,
    // Snapshotted at the start and transformed from there every frame. Applying
    // each frame's delta to the previous result would accumulate the rounding
    // of every intermediate step across a long drag.
    items,
    origin: { x, y },
    pivot: { x: box.x + box.w / 2, y: box.y + box.h / 2 },
    startAngle: Math.atan2(y - (box.y + box.h / 2), x - (box.x + box.w / 2)),
    lastCommit: 0,
  };
}

/** What the current gesture would produce at this pointer position. */
function gestureResult(x, y) {
  const gesture = state.gesture;
  if (gesture.kind === "move") {
    return transform.translate(gesture.items, x - gesture.origin.x, y - gesture.origin.y);
  }
  if (gesture.kind === "resize") {
    return transform.resize(
      gesture.items,
      gesture.box,
      transform.boxFromDrag(gesture.box, gesture.handle, x, y),
    );
  }
  const angle = Math.atan2(y - gesture.pivot.y, x - gesture.pivot.x);
  return transform.rotate(gesture.items, gesture.pivot, angle - gesture.startAngle);
}

// -- input -----------------------------------------------------------------

function commitDraft() {
  if (!state.draft) return;
  const draft = state.draft;
  state.draft = null;

  if (draft.kind === "freedraw") {
    if ((draft.points?.length ?? 0) < 2) return;
    const id = state.engine.exec(state.board, {
      cmd: "stroke",
      points: draft.points,
      stroke: draft.stroke,
      stroke_width: draft.stroke_width,
    });
    // `stroke` has no opacity of its own — a path carries no fill, so the
    // command that creates one takes no look beyond its colour and width. The
    // id comes back from exec rather than being guessed at from the scene.
    if (draft.opacity < 1) {
      state.engine.exec(state.board, { cmd: "style", id, opacity: draft.opacity });
    }
  } else {
    // Ignore accidental click-sized shapes.
    if (Math.abs(draft.w) < 3 && Math.abs(draft.h) < 3) return;
    const id = state.engine.exec(state.board, {
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
    if (draft.opacity < 1) {
      state.engine.exec(state.board, { cmd: "style", id, opacity: draft.opacity });
    }
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

  // Middle button or Alt pans regardless of the active tool. Shift used to,
  // and now extends the selection instead — a modifier cannot do both.
  if (event.button === 1 || event.altKey) {
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

  if (state.tool === "text") {
    // Keep the canvas' pointer default from stealing focus back after the
    // handler opens the real textarea overlay.
    event.preventDefault();
    const target = hitTest(x, y);
    // Clicking an existing label edits it rather than starting a new one on
    // top of it, which is what a second click on words obviously means.
    if (target?.text !== undefined && target?.text !== null) {
      select(target.id);
      editTextItem(target);
    } else {
      select(null);
      beginTextEdit({ x, y });
    }
    return;
  }

  if (state.tool === "sticky") {
    event.preventDefault();
    const target = hitTest(x, y);
    if (target?.role === "sticky") {
      select(target.id);
      editTextItem(target);
    } else {
      select(null);
      beginTextEdit({
        x,
        y,
        w: STICKY_MIN_WIDTH,
        h: STICKY_MIN_HEIGHT,
        role: "sticky",
      });
    }
    return;
  }

  if (state.tool === "select") {
    // Handles are tested first: they sit on top of the shapes they belong to,
    // and a corner handle overlapping another element must resize rather than
    // select whatever is underneath it.
    const box = selectionBox();
    if (box) {
      const handle = transform.handleAt(box, x, y, transform.HANDLE_GRAB / state.view.scale);
      if (handle) {
        beginGesture(handle === "rotate" ? "rotate" : "resize", x, y, handle);
        return;
      }
    }

    const target = hitTest(x, y);
    if (event.shiftKey) {
      if (target) toggleSelected(target.id);
      return;
    }
    if (target) {
      // Clicking a member of a multiple selection drags the whole thing, which
      // is what grabbing one of several selected shapes means.
      if (!state.selection.has(target.id)) select(target.id);
      beginGesture("move", x, y);
    } else {
      select(null);
      state.marquee = { x, y, w: 0, h: 0, originX: x, originY: y };
      invalidate();
    }
    return;
  }

  const style = currentStyle();
  state.draft =
    state.tool === "freedraw"
      ? { kind: "freedraw", x, y, w: 0, h: 0, points: [[x, y]], ...style }
      : { kind: state.tool, x, y, w: 0, h: 0, ...style };
  invalidate();
});

canvas.addEventListener("wheel", () => endTextEdit(), { passive: true });

canvas.addEventListener("dblclick", (event) => {
  if (state.board === null) return;
  const [x, y] = toScene(event.clientX, event.clientY);
  const target = hitTest(x, y);
  if (target?.text === undefined || target?.text === null) return;
  select(target.id);
  editTextItem(target);
});

canvas.addEventListener("pointermove", (event) => {
  if (state.pan) {
    state.view.x = state.pan.x + (event.clientX - state.pan.startX);
    state.view.y = state.pan.y + (event.clientY - state.pan.startY);
    invalidate();
    return;
  }

  const [x, y] = toScene(event.clientX, event.clientY);
  // Remembered so a paste lands where the user is looking rather than where
  // the copy happened to be, which means nothing on a board that has since
  // been panned — or on a different board entirely.
  state.pointer = { x, y };
  reportCursor(x, y);

  if (state.marquee) {
    state.marquee.x = Math.min(state.marquee.originX, x);
    state.marquee.y = Math.min(state.marquee.originY, y);
    state.marquee.w = Math.abs(x - state.marquee.originX);
    state.marquee.h = Math.abs(y - state.marquee.originY);
    invalidate();
    return;
  }

  if (state.gesture) {
    const now = performance.now();
    // Throttled rather than per-frame: peers should see the drag happening,
    // without one drag becoming a thousand operations in the durable log.
    if (now - state.gesture.lastCommit > COMMIT_INTERVAL_MS) {
      state.gesture.lastCommit = now;
      applyGeometry(gestureResult(x, y));
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
  if (state.marquee) {
    const marquee = state.marquee;
    state.marquee = null;
    // A click, not a drag. Clearing the selection already happened on the way
    // down; selecting nothing again here would be the same answer twice.
    if (marquee.w >= 1 || marquee.h >= 1) {
      select(state.scene.filter((item) => transform.intersects(item, marquee)).map((i) => i.id));
    }
    invalidate();
    return;
  }
  if (state.gesture) {
    const [x, y] = toScene(event.clientX, event.clientY);
    // Applied once more unthrottled, so where the pointer finished is where the
    // shapes finish rather than wherever the last throttled frame landed.
    applyGeometry(gestureResult(x, y));
    state.gesture = null;
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
    // Keep the point under the cursor fixed while zooming.
    setZoom(state.view.scale * factor, { x: px, y: py });
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

function exportRecovery() {
  if (state.outbox === null) return;
  const recovery = {
    format: "kboard-recovery-v1",
    exportedAt: new Date().toISOString(),
    scope: state.scope,
    replica: state.replica,
    actor: state.actor === null ? null : String(state.actor),
    outbox: state.outbox.exportData(),
    // A quota failure leaves these operations deliberately in memory rather
    // than pretending they are durable. Include them while this tab is alive.
    localOnlyOperations: state.localOnlyOps,
  };
  download(
    new Blob([`${JSON.stringify(recovery, null, 2)}\n`], { type: "application/json" }),
    exportName("recovery.json"),
  );
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

const strokeSwatches = [...document.querySelectorAll(".swatch[data-colour]")];
for (const swatch of strokeSwatches) {
  swatch.addEventListener("click", () => {
    state.colour = swatch.dataset.colour;
    markChecked(strokeSwatches, swatch);
    applyStyle({ stroke: pack(state.colour) });
    // A label is drawn in the stroke colour, so an open editor has to follow.
    editor.style.color = state.colour;
    if (state.editing) {
      state.editing.stroke = pack(state.colour);
      editor.style.setProperty("--sticky-stroke", state.colour);
    }
  });
}

const fillSwatches = [...document.querySelectorAll(".swatch[data-fill]")];
for (const swatch of fillSwatches) {
  swatch.addEventListener("click", () => {
    state.fill = swatch.dataset.fill;
    markChecked(fillSwatches, swatch);
    applyStyle({ fill: packOrNone(state.fill) });
    if (state.editing?.role === "sticky") {
      state.editing.fill = packOrNone(state.fill);
      editor.style.setProperty(
        "--sticky-fill",
        state.fill === "none" ? "transparent" : state.fill,
      );
    }
  });
}

const widthButtons = [...document.querySelectorAll(".width")];
for (const button of widthButtons) {
  button.addEventListener("click", () => {
    state.strokeWidth = Number(button.dataset.width);
    markChecked(widthButtons, button);
    applyStyle({ stroke_width: state.strokeWidth });
  });
}

const opacityInput = document.getElementById("opacity");
opacityInput.addEventListener("input", () => {
  state.opacity = Number(opacityInput.value) / 100;
  applyStyle({ opacity: state.opacity });
});

editor.addEventListener("input", fitEditor);
// Clicking away commits. That is what a click away from a text box means
// everywhere else, and the alternative is losing the words to a stray click.
editor.addEventListener("blur", () => endTextEdit());
editor.addEventListener("keydown", (event) => {
  // Enter adds a line, as it does in every other multi-line field. Escape and
  // Ctrl+Enter finish. Everything else — selection, undo inside the field,
  // IME composition — is the platform's, and is left alone deliberately.
  if (event.key === "Escape" || (event.key === "Enter" && (event.metaKey || event.ctrlKey))) {
    event.preventDefault();
    endTextEdit();
  }
  event.stopPropagation();
});

undoButton.addEventListener("click", () => stepHistory(true));
redoButton.addEventListener("click", () => stepHistory(false));
pngButton.addEventListener("click", exportPng);
svgButton.addEventListener("click", exportSvg);
recoveryButton.addEventListener("click", exportRecovery);
zoomOutButton.addEventListener("click", () => setZoom(state.view.scale / 1.2));
zoomResetButton.addEventListener("click", () => setZoom(1));
zoomInButton.addEventListener("click", () => setZoom(state.view.scale * 1.2));
zoomFitButton.addEventListener("click", fitContent);
openStandaloneButton.addEventListener("click", () => {
  runtime.bridge?.emit("openStandalone", { scope: state.scope });
});

if (runtime.mode === "embedded") {
  document.body.classList.add("embed-mode");
  openStandaloneButton.hidden = false;
  hint.classList.add("gone");
  runtime.bridge.handle("focus", () => {
    canvas.focus();
    return { focused: true };
  });
  runtime.bridge.handle("flush", async () => {
    flush();
    await state.captureChain;
    const summary = outboxSummary();
    return { pending: summary.pending + summary.sending };
  });
}

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
  l: "line",
  p: "freedraw",
  s: "sticky",
  t: "text",
  e: "eraser",
};

window.addEventListener("keydown", (event) => {
  // Every shortcut below acts on the board. While someone is typing, the keys
  // belong to the field — otherwise "d" duplicates a shape instead of being
  // written, and Delete erases the selection instead of a character.
  if (state.editing) return;
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
    if (key === "c" || key === "x") {
      // Only claimed when something is selected, so the browser's own copy of
      // selected page text still works when the board is not the subject.
      if (selected().length === 0) return;
      event.preventDefault();
      copySelection({ cut: key === "x" });
      return;
    }
    if (key === "v") {
      event.preventDefault();
      paste();
      return;
    }
    if (key === "d") {
      event.preventDefault();
      duplicateSelection();
      return;
    }
    if (key === "a") {
      event.preventDefault();
      select(state.scene.map((item) => item.id));
      return;
    }
    if (key === "0") {
      event.preventDefault();
      setZoom(1);
      return;
    }
  }
  if (event.metaKey || event.ctrlKey || event.altKey) return;

  if (event.key === "Delete" || event.key === "Backspace") {
    if (selected().length === 0) return;
    event.preventDefault();
    deleteSelection();
    return;
  }
  if (event.key === "Escape") {
    select(null);
    return;
  }
  if (event.key === "+" || event.key === "=") {
    event.preventDefault();
    setZoom(state.view.scale * 1.2);
    return;
  }
  if (event.key === "-") {
    event.preventDefault();
    setZoom(state.view.scale / 1.2);
    return;
  }
  if (event.key === "0") {
    event.preventDefault();
    setZoom(1);
    return;
  }
  // Cycles what is selected, so every shortcut above is reachable without a
  // pointer. Only when the canvas has focus — Tab must still move through the
  // toolbar for everyone else.
  if (event.key === "Tab" && document.activeElement === canvas && state.scene.length > 0) {
    event.preventDefault();
    const only = onlySelected();
    const at = only ? state.scene.findIndex((item) => item.id === only.id) : -1;
    const count = state.scene.length;
    const step = event.shiftKey ? -1 : 1;
    // With nothing selected, forwards starts at the first element and
    // backwards at the last, rather than at whatever the modulus happens to
    // produce for an index of -1.
    const next = at === -1 ? (step === 1 ? 0 : count - 1) : (at + step + count) % count;
    select(state.scene[next].id);
    return;
  }

  const tool = SHORTCUTS[event.key.toLowerCase()];
  if (tool) selectTool(tool);
});

if (runtime.mode === "standalone") {
  window.addEventListener("hashchange", () => location.reload());
}
window.addEventListener("resize", invalidate);
refreshZoomControl();

// -- start -----------------------------------------------------------------

(async function start() {
  if (runtime.mode === "standalone" && !location.hash) {
    history.replaceState(null, "", `#${state.scope}`);
  }
  try {
    state.engine = await loadEngine("./kboard.wasm");
  } catch (error) {
    setStatus("offline", "engine failed to load");
    runtime.bridge?.emit("error", { message: "engine failed to load" });
    hint.textContent = "Build the engine: cargo build -p kboard-ffi --target wasm32-unknown-unknown --release";
    hint.classList.remove("gone");
    console.error(error);
    return;
  }

  const client = await createClientOutbox(state.scope);
  state.outbox = client.outbox;
  state.replica = client.replica;
  ensureBoard(client.actor);
  const restoredOperations = state.outbox.allOperations();
  if (restoredOperations.length > 0) {
    state.engine.merge(state.board, JSON.stringify(restoredOperations));
    setStatus("offline", durabilityStatus("restored pending work"));
  } else if (!state.outbox.snapshot().durableStorage) {
    setStatus("offline", durabilityStatus("local board"));
  }
  recoveryButton.disabled = state.outbox.snapshot().refused === 0;
  sceneChanged();
  connect();

  requestAnimationFrame(frame);
  runtime.bridge?.emit("ready", {
    scope: state.scope,
    mode: runtime.mode,
    status: statusText.textContent,
  });
})();
