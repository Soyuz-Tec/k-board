/**
 * End-to-end convergence check against a running server.
 *
 * Two independent replicas connect over WebSocket, edit concurrently, and must
 * end up identical — with *both* edits intact. The unit tests prove the merge
 * laws in isolation; this proves the whole path: wasm engine, wire format,
 * server fan-out, and back.
 *
 * It reuses `web/kboard.js` unchanged, so it also demonstrates that the engine
 * runs in a Node host as readily as in a browser — same wasm, same binding, no
 * DOM.
 *
 *   node scripts/two-replica-check.mjs
 *
 * Exits non-zero on failure so CI can gate on it.
 */

import { loadEngine } from "../web/kboard.js";

const BASE = process.env.KBOARD_URL ?? "http://127.0.0.1:8080";
const SCOPE = process.env.KBOARD_SCOPE ?? `node-check-${Date.now()}`;
const SETTLE_MS = 400;

const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

let failures = 0;
function check(label, condition, detail = "") {
  if (condition) {
    console.log(`  PASS  ${label}`);
  } else {
    failures += 1;
    console.error(`  FAIL  ${label}${detail ? ` — ${detail}` : ""}`);
  }
}

const engine = await loadEngine(`${BASE}/kboard.wasm`);

/** Connect one replica and resolve once the server has sent its join state. */
function connect(name) {
  return new Promise((resolve, reject) => {
    const socket = new WebSocket(`${BASE.replace(/^http/, "ws")}/ws/${SCOPE}`);
    let board = null;
    let actor = null;

    const replica = {
      name,
      get actor() {
        return actor;
      },
      exec(command) {
        const id = engine.exec(board, command);
        const ops = engine.pending(board);
        if (ops.length > 0) socket.send(JSON.stringify({ type: "ops", ops }));
        return id;
      },
      scene: () => engine.scene(board),
      undo() {
        const moved = engine.undo(board);
        const ops = engine.pending(board);
        if (ops.length > 0) socket.send(JSON.stringify({ type: "ops", ops }));
        return moved;
      },
      history: () => engine.history(board),
      close: () => socket.close(),
    };

    socket.onerror = () => reject(new Error(`${name}: socket error — is the server running?`));
    socket.onmessage = (event) => {
      const message = JSON.parse(event.data);
      if (message.type === "init") {
        actor = message.actor;
        board = engine.open(SCOPE, actor);
        engine.load(board, JSON.stringify(message.doc));
        resolve(replica);
      } else if (message.type === "ops") {
        engine.merge(board, JSON.stringify(message.ops));
      }
    };
  });
}

console.log(`k-board end-to-end check against ${BASE} (scope ${SCOPE})\n`);

const alice = await connect("alice");
const bob = await connect("bob");
check("two replicas connect", alice.actor !== null && bob.actor !== null);
check(
  "server assigns distinct actor ids",
  alice.actor !== bob.actor,
  `alice=${alice.actor} bob=${bob.actor}`,
);

// 1. A shape drawn on one replica reaches the other.
const id = alice.exec({
  cmd: "add",
  kind: "rectangle",
  x: 10,
  y: 20,
  w: 100,
  h: 60,
  stroke: 0x1e1e1eff,
});
await wait(SETTLE_MS);
check("shape propagates to the peer", bob.scene().length === 1, `bob saw ${bob.scene().length}`);

// 2. The headline property: concurrent edits to *different* attributes of the
//    same shape. An editor that merges whole elements loses one of these.
alice.exec({ cmd: "move", id, x: 250, y: 20 });
bob.exec({ cmd: "style", id, stroke: 0xe03131ff });
await wait(SETTLE_MS);

const [fromAlice] = alice.scene();
const [fromBob] = bob.scene();

check(
  "replicas converge after concurrent edits",
  JSON.stringify(fromAlice) === JSON.stringify(fromBob),
  `\n    alice: ${JSON.stringify(fromAlice)}\n    bob:   ${JSON.stringify(fromBob)}`,
);
check("the move survived", fromAlice.x === 250, `x=${fromAlice.x}`);
check(
  "the concurrent restyle also survived",
  fromAlice.stroke === 0xe03131ff,
  `stroke=0x${(fromAlice.stroke >>> 0).toString(16)}`,
);

// 3. A late joiner receives the current state, not the history.
const carol = await connect("carol");
const carolCanUndo = () => carol.history().canUndo;
await wait(SETTLE_MS);
check("late joiner receives current state", carol.scene().length === 1);
check(
  "late joiner agrees with everyone else",
  JSON.stringify(carol.scene()[0]) === JSON.stringify(fromAlice),
);

// 4. An undo is an ordinary edit: it reaches peers and converges.
alice.exec({ cmd: "move", id, x: 400, y: 400 });
await wait(SETTLE_MS);
check("a move reaches the peer", bob.scene()[0]?.x === 400, `x=${bob.scene()[0]?.x}`);

check("the mover can undo", alice.history().canUndo);
check("the peer cannot undo work it did not do", !carolCanUndo(), "history is per actor");

alice.undo();
await wait(SETTLE_MS);
check(
  "the undo reached the peer",
  bob.scene()[0]?.x === 250,
  `x=${bob.scene()[0]?.x}`,
);
check(
  "replicas still agree after an undo",
  JSON.stringify(alice.scene()) === JSON.stringify(bob.scene()),
);

// 5. Text is an element like any other: it replicates, and an edit to its
//    content converges the same way a move does.
const labelId = alice.exec({
  cmd: "text",
  x: 40,
  y: 500,
  w: 120,
  h: 25,
  text: "hello",
  font_size: 20,
  stroke: 0x1e1e1eff,
});
await wait(SETTLE_MS);

const labelOnBob = bob.scene().find((item) => item.id === labelId);
check("a label reaches the peer", labelOnBob?.text === "hello", JSON.stringify(labelOnBob));
check("and arrives as a text element", labelOnBob?.kind === "text", labelOnBob?.kind);

// The box travels with the words: the peer never measured this font and cannot
// derive the box for itself.
check(
  "the box the author measured travels with it",
  labelOnBob?.w === 120 && labelOnBob?.h === 25,
  `${labelOnBob?.w} by ${labelOnBob?.h}`,
);

bob.exec({ cmd: "set_text", id: labelId, text: "edited by the peer", w: 220, h: 25 });
await wait(SETTLE_MS);
const edited = alice.scene().find((item) => item.id === labelId);
check("an edit converges back", edited?.text === "edited by the peer", JSON.stringify(edited));
check("and brings the new box with it", edited?.w === 220, `${edited?.w}`);

alice.exec({ cmd: "delete", id: labelId });
await wait(SETTLE_MS);
check("a label deletes like anything else", !bob.scene().some((i) => i.id === labelId));

// 6. Deletion converges too.
bob.exec({ cmd: "delete", id });
await wait(SETTLE_MS);
check(
  "delete propagates to all replicas",
  alice.scene().length === 0 && carol.scene().length === 0,
  `alice=${alice.scene().length} carol=${carol.scene().length}`,
);

for (const replica of [alice, bob, carol]) replica.close();

const stats = await (await fetch(`${BASE}/api/rooms/${SCOPE}/stats`)).json();
console.log(`\nserver room: ${JSON.stringify(stats)}`);
console.log(failures === 0 ? "\nAll checks passed." : `\n${failures} check(s) failed.`);
process.exit(failures === 0 ? 0 : 1);
