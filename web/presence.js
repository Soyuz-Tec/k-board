/**
 * Who else is here, and where they are pointing.
 *
 * Presence is deliberately **not** part of the document. It never reaches the
 * engine, never enters the operation log, and is never persisted. A cursor is
 * only interesting while its owner is connected — routing it through the CRDT
 * would make every mouse movement a durable write and leave the pointer of
 * someone who closed their laptop in the board forever.
 *
 * That separation is the whole design. Everything here is throwaway state, and
 * losing all of it costs nothing but a repaint.
 */

/** Cursor reports per second. Shares a rate budget with drawing — see limits.rs. */
export const REPORT_INTERVAL_MS = 60;

/**
 * How long a silent peer is kept.
 *
 * The server announces departures, so this is the backstop for the case it
 * cannot cover: a peer whose connection died without a close frame, or whose
 * relay was dropped for lagging.
 */
export const STALE_AFTER_MS = 10_000;

/**
 * Distinct, readable, and stable for a given actor.
 *
 * Derived rather than assigned, so every replica independently agrees on what
 * colour a peer is without the server having to allocate or remember anything.
 * The hues are spaced by an irrational turn so that nearby actor ids — which is
 * what sequential connections produce — land far apart.
 */
export function peerColour(actor) {
  const hue = Math.round(((Number(actor) * 137.508) % 360 + 360) % 360);
  return `hsl(${hue} 70% 45%)`;
}

/** A short human-readable handle. Actor ids are allocated, not chosen. */
export function peerName(actor) {
  return `#${actor}`;
}

export class Presence {
  constructor() {
    /** @type {Map<number, {x: number, y: number, seen: number}>} */
    this.peers = new Map();
    this.lastReport = 0;
    this.lastSent = null;
  }

  /**
   * Whether a cursor report should be sent now.
   *
   * Throttled by time *and* by movement: a stationary pointer produces no
   * traffic at all, which matters because the rate budget is shared with the
   * drawing that is probably happening at the same time.
   */
  shouldReport(x, y, now) {
    if (now - this.lastReport < REPORT_INTERVAL_MS) return false;
    if (this.lastSent && this.lastSent.x === x && this.lastSent.y === y) return false;
    this.lastReport = now;
    this.lastSent = { x, y };
    return true;
  }

  observe(actor, x, y, now) {
    this.peers.set(actor, { x, y, seen: now });
  }

  forget(actor) {
    return this.peers.delete(actor);
  }

  /** Drop peers that have gone quiet. Returns true if anything was removed. */
  expire(now) {
    let removed = false;
    for (const [actor, peer] of this.peers) {
      if (now - peer.seen > STALE_AFTER_MS) {
        this.peers.delete(actor);
        removed = true;
      }
    }
    return removed;
  }

  /** Present peers, in a stable order so the readout does not jitter. */
  list() {
    return [...this.peers.entries()]
      .sort(([left], [right]) => Number(left) - Number(right))
      .map(([actor, peer]) => ({ actor, ...peer }));
  }

  clear() {
    this.peers.clear();
  }
}

/**
 * Draw peer cursors.
 *
 * In screen space, at a fixed size: a cursor is a pointer, not a drawn object,
 * so it should not shrink when someone zooms out. The caller supplies the view
 * transform rather than the context carrying it.
 */
export function drawCursors(context, peers, view) {
  context.save();
  // The device pixel ratio still applies — dropping it would halve every cursor
  // on a retina display. Only the scene's pan and zoom are discarded.
  context.setTransform(view.ratio, 0, 0, view.ratio, 0, 0);
  context.lineJoin = "round";
  context.font = "500 11px ui-sans-serif, system-ui, sans-serif";
  context.textBaseline = "middle";

  for (const peer of peers) {
    const x = peer.x * view.scale + view.x;
    const y = peer.y * view.scale + view.y;
    const colour = peerColour(peer.actor);

    // An arrowhead, outlined in white so it stays visible over dark strokes.
    context.beginPath();
    context.moveTo(x, y);
    context.lineTo(x, y + 15);
    context.lineTo(x + 4, y + 11.5);
    context.lineTo(x + 9.5, y + 11.5);
    context.closePath();
    context.fillStyle = colour;
    context.strokeStyle = "#ffffff";
    context.lineWidth = 1.5;
    context.fill();
    context.stroke();

    const label = peerName(peer.actor);
    const width = context.measureText(label).width;
    context.fillStyle = colour;
    context.beginPath();
    // A missing `roundRect` must not throw inside the render loop and take the
    // whole board down with it.
    if (context.roundRect) context.roundRect(x + 12, y + 10, width + 10, 16, 8);
    else context.rect(x + 12, y + 10, width + 10, 16);
    context.fill();
    context.fillStyle = "#ffffff";
    context.fillText(label, x + 17, y + 18.5);
  }

  context.restore();
}
