/**
 * Moving, scaling and turning a selection.
 *
 * Pure arithmetic over scene items: nothing here touches the engine, the DOM,
 * or the network. It returns the geometry each element should end up with, and
 * the caller decides when to write it.
 *
 * ## What this model can and cannot express
 *
 * An element carries `x, y, w, h, angle` — a box and a turn, not a matrix. That
 * is enough for any uniform scale and any rotation, and it is *not* enough for a
 * non-uniform scale of an already-rotated shape: stretching a tilted rectangle
 * along the screen's x axis shears it, and a shear has no representation here.
 *
 * So resizing scales each element about its own centre and leaves its angle
 * alone. That is exact for uniform scaling and a deliberate approximation
 * otherwise — chosen over silently storing a transform the model cannot
 * round-trip, and over refusing to resize anything that has been turned.
 */

import { bounds, corners } from "./scene.js";

/** How near the pointer must be to grab a handle, in board units. */
export const HANDLE_GRAB = 8;

/** How far above the selection the rotation handle sits, in board units. */
export const ROTATE_REACH = 24;

/** The axis-aligned box containing every given element, rotation included. */
export function unionBounds(items) {
  if (items.length === 0) return null;
  let minX = Infinity;
  let minY = Infinity;
  let maxX = -Infinity;
  let maxY = -Infinity;
  for (const item of items) {
    for (const [x, y] of corners(item)) {
      minX = Math.min(minX, x);
      minY = Math.min(minY, y);
      maxX = Math.max(maxX, x);
      maxY = Math.max(maxY, y);
    }
  }
  return { x: minX, y: minY, w: maxX - minX, h: maxY - minY };
}

/**
 * The eight resize handles and the rotation handle, in board coordinates.
 *
 * Named by which corner or edge they anchor, because that is what the drag
 * arithmetic needs to know: dragging `nw` moves the top-left and holds the
 * bottom-right still.
 */
export function handles(box) {
  const { x, y, w, h } = box;
  return [
    { name: "nw", x, y },
    { name: "n", x: x + w / 2, y },
    { name: "ne", x: x + w, y },
    { name: "e", x: x + w, y: y + h / 2 },
    { name: "se", x: x + w, y: y + h },
    { name: "s", x: x + w / 2, y: y + h },
    { name: "sw", x, y: y + h },
    { name: "w", x, y: y + h / 2 },
    { name: "rotate", x: x + w / 2, y: y - ROTATE_REACH },
  ];
}

/** Which handle the pointer is on, or null. Tolerance scales with the zoom. */
export function handleAt(box, x, y, tolerance = HANDLE_GRAB) {
  for (const handle of handles(box)) {
    if (Math.abs(handle.x - x) <= tolerance && Math.abs(handle.y - y) <= tolerance) {
      return handle.name;
    }
  }
  return null;
}

/**
 * The box a drag of `handle` produces.
 *
 * The opposite corner or edge stays put, which is what makes a resize feel like
 * it is anchored rather than sliding. Edge handles move one axis only.
 */
export function boxFromDrag(start, handle, x, y) {
  let { x: left, y: top } = start;
  let right = start.x + start.w;
  let bottom = start.y + start.h;

  if (handle.includes("w")) left = x;
  if (handle.includes("e")) right = x;
  if (handle.includes("n")) top = y;
  if (handle.includes("s")) bottom = y;

  // Normalised, so dragging a handle past its opposite flips the box rather
  // than producing a negative size that every consumer then has to defend
  // against.
  return {
    x: Math.min(left, right),
    y: Math.min(top, bottom),
    w: Math.abs(right - left),
    h: Math.abs(bottom - top),
  };
}

/** Smallest box a resize may produce, so a selection can never be lost. */
export const MIN_EXTENT = 2;

/**
 * Scale elements from `from` to `to`.
 *
 * Each element is scaled about its own centre, and its centre is moved with the
 * box. Text scales uniformly by the smaller factor: the stored box and the
 * glyphs have to agree, and a font size stretched on one axis only would leave
 * words that no longer fit the box recorded for them.
 */
export function resize(items, from, to) {
  const sx = from.w > MIN_EXTENT ? to.w / from.w : 1;
  const sy = from.h > MIN_EXTENT ? to.h / from.h : 1;

  return items.map((item) => {
    const box = bounds(item);
    const centre = { x: box.x + box.w / 2, y: box.y + box.h / 2 };
    const movedX = to.x + (centre.x - from.x) * sx;
    const movedY = to.y + (centre.y - from.y) * sy;

    const uniform = Math.min(Math.abs(sx), Math.abs(sy));
    const scaleX = item.kind === "text" ? uniform : sx;
    const scaleY = item.kind === "text" ? uniform : sy;
    const w = Math.max(MIN_EXTENT, Math.abs(box.w * scaleX));
    const h = Math.max(MIN_EXTENT, Math.abs(box.h * scaleY));

    const next = {
      id: item.id,
      x: movedX - w / 2,
      y: movedY - h / 2,
      w,
      h,
    };
    if (item.kind === "text") next.font_size = Math.max(1, (item.font_size || 20) * uniform);
    if (item.points) {
      // The path *is* the shape for freehand, so scaling the box without it
      // would leave the stroke its original size inside a box claiming
      // otherwise.
      next.points = item.points.map(([px, py]) => [
        next.x + (px - box.x) * scaleX,
        next.y + (py - box.y) * scaleY,
      ]);
    }
    return next;
  });
}

/**
 * Turn elements by `delta` radians about `pivot`.
 *
 * Each element gains the same turn *and* orbits the pivot, which is what makes
 * rotating a group behave like rotating one object rather than spinning each
 * piece where it stands.
 */
export function rotate(items, pivot, delta) {
  const cos = Math.cos(delta);
  const sin = Math.sin(delta);

  return items.map((item) => {
    const box = bounds(item);
    const cx = box.x + box.w / 2;
    const cy = box.y + box.h / 2;
    const dx = cx - pivot.x;
    const dy = cy - pivot.y;
    const movedX = pivot.x + dx * cos - dy * sin;
    const movedY = pivot.y + dx * sin + dy * cos;

    return {
      id: item.id,
      x: movedX - box.w / 2,
      y: movedY - box.h / 2,
      w: box.w,
      h: box.h,
      angle: (item.angle || 0) + delta,
    };
  });
}

/** Offset elements by a delta, for a plain drag of the selection. */
export function translate(items, dx, dy) {
  return items.map((item) => {
    const box = bounds(item);
    const next = { id: item.id, x: box.x + dx, y: box.y + dy, w: box.w, h: box.h };
    if (item.points) next.points = item.points.map(([px, py]) => [px + dx, py + dy]);
    return next;
  });
}

/** Whether an element's box overlaps a marquee rectangle. */
export function intersects(item, marquee) {
  const box = unionBounds([item]);
  return (
    box.x < marquee.x + marquee.w &&
    box.x + box.w > marquee.x &&
    box.y < marquee.y + marquee.h &&
    box.y + box.h > marquee.y
  );
}
