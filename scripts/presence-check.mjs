/**
 * Proves presence is relayed and then forgotten.
 *
 * Cursors are the one thing on this board that must *not* be durable. The
 * design claim is that they reach connected peers without touching the
 * document, the operation log, or the disk — and the way that claim fails
 * quietly is that presence works perfectly while also being recorded, which
 * nobody notices until a board has a million rows of mouse movement in it.
 *
 * So the interesting assertions here are the negative ones: after a hundred
 * cursor reports, the room must have accepted nothing.
 *
 *   node scripts/presence-check.mjs
 *
 * Exits non-zero on failure so CI can gate on it.
 */

import { loadEngine } from "../web/kboard.js";
import { Presence, peerColour, peerName } from "../web/presence.js";

const BASE = process.env.KBOARD_URL ?? "http://127.0.0.1:8080";
const SCOPE = process.env.KBOARD_PRESENCE_SCOPE ?? "presence-check";
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

/** A raw connection: no engine, so what arrives on the wire is what is seen. */
function connect(name) {
  return new Promise((resolve, reject) => {
    const socket = new WebSocket(`${BASE.replace(/^http/, "ws")}/ws/${SCOPE}`);
    const received = [];
    let actor = null;

    socket.onerror = () => reject(new Error(`${name}: socket error — is the server running?`));
    socket.onmessage = (event) => {
      const message = JSON.parse(event.data);
      if (message.type === "init") {
        actor = message.actor;
        resolve({
          name,
          received,
          get actor() {
            return actor;
          },
          send: (payload) => socket.send(JSON.stringify(payload)),
          close: () => socket.close(),
        });
      } else {
        received.push(message);
      }
    };
  });
}

const stats = async () => (await fetch(`${BASE}/api/rooms/${SCOPE}/stats`)).json();

console.log(`k-board presence check against ${BASE} (scope ${SCOPE})\n`);

// -- the pure client-side logic, which needs no server ---------------------

const presence = new Presence();
check(
  "a stationary pointer produces no traffic",
  presence.shouldReport(10, 10, 1_000) && !presence.shouldReport(10, 10, 10_000),
  "an unmoved cursor should not be resent even after the interval",
);
check("a moved pointer reports again", presence.shouldReport(11, 10, 20_000));
check(
  "sequential actors get well-separated colours",
  peerColour(1) !== peerColour(2) && peerColour(2) !== peerColour(3),
  `${peerColour(1)} ${peerColour(2)} ${peerColour(3)}`,
);
check("the same actor always gets the same colour", peerColour(7) === peerColour(7));

presence.observe(42, 1, 2, 0);
check("a peer is remembered", presence.list().length === 1);
check("a silent peer is dropped", presence.expire(60_000) && presence.list().length === 0);

// -- and the part that needs one -------------------------------------------

const alice = await connect("alice");
const bob = await connect("bob");
check("two replicas connect", alice.actor !== null && bob.actor !== null);

const before = await stats();

alice.send({ type: "presence", x: 120.5, y: 240.25 });
await wait(SETTLE_MS);

const seen = bob.received.filter((message) => message.type === "presence");
check("a cursor reaches the peer", seen.length === 1, JSON.stringify(bob.received));
check(
  "the position is relayed intact",
  seen[0]?.x === 120.5 && seen[0]?.y === 240.25,
  JSON.stringify(seen[0]),
);
check(
  "the peer is told whose cursor it is",
  seen[0]?.actor === alice.actor,
  `got ${seen[0]?.actor}, expected ${alice.actor}`,
);
check(
  "a connection does not receive its own cursor back",
  alice.received.filter((message) => message.type === "presence").length === 0,
);

// A client that names someone else must not be able to move their pointer.
alice.send({ type: "presence", x: 1, y: 1, actor: 999_999 });
await wait(SETTLE_MS);
const spoofed = bob.received.filter((message) => message.type === "presence").at(-1);
check(
  "a client cannot move somebody else's cursor",
  spoofed?.actor === alice.actor,
  `server reported actor ${spoofed?.actor}`,
);

// The assertion this file exists for.
for (let index = 0; index < 100; index += 1) {
  alice.send({ type: "presence", x: index, y: index * 2 });
}
await wait(SETTLE_MS);

const after = await stats();
check(
  "a hundred cursor reports accept no operations",
  after.accepted === before.accepted,
  `accepted went ${before.accepted} → ${after.accepted}`,
);
check(
  "and add no elements to the board",
  after.elements === 0 && after.tombstones === 0,
  JSON.stringify(after),
);

// The control. Without it, the two assertions above would pass just as
// happily against a server that records nothing at all.
const engine = await loadEngine(`${BASE}/kboard.wasm`);
const board = engine.open(SCOPE, bob.actor);
engine.exec(board, { cmd: "add", kind: "rectangle", x: 1, y: 2, w: 30, h: 40, stroke: 0x1e1e1eff });
bob.send({ type: "ops", ops: engine.pending(board) });
await wait(SETTLE_MS);

const drawn = await stats();
check(
  "a real edit does move the counter presence left alone",
  drawn.accepted > after.accepted && drawn.elements === 1,
  JSON.stringify(drawn),
);

// A malformed frame is dropped, not fatal.
alice.send({ type: "presence", x: "over there", y: null });
await wait(SETTLE_MS);
alice.send({ type: "presence", x: 5, y: 5 });
await wait(SETTLE_MS);
check(
  "a malformed cursor does not end the session",
  bob.received.filter((message) => message.type === "presence").at(-1)?.x === 5,
  "the connection should survive a frame it cannot parse",
);

// Departure is announced rather than left to time out.
alice.close();
await wait(SETTLE_MS);
const left = bob.received.filter((message) => message.type === "left");
check("a departure is announced", left.length >= 1, JSON.stringify(bob.received.slice(-3)));
check("and names who left", left.at(-1)?.actor === alice.actor, JSON.stringify(left.at(-1)));

check("peer names are stable and readable", peerName(alice.actor) === `#${alice.actor}`);

bob.close();

console.log(failures === 0 ? "\nAll checks passed." : `\n${failures} check(s) failed.`);
process.exit(failures === 0 ? 0 : 1);
