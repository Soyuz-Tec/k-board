/**
 * Copying elements, on and off the board.
 *
 * The payload goes on the *system* clipboard as JSON text, not into a variable
 * in this tab. That is what makes copy work between two boards, between two
 * tabs, and after a reload — which is the whole reason anyone copies rather
 * than redrawing. An in-tab clipboard would look identical in a demo and fail
 * the first time it mattered.
 *
 * Nothing here touches the engine. A copied element is turned back into the
 * `add`/`stroke` commands that would have created it, so a paste is an ordinary
 * edit that merges and undoes like any other — and so the copy gets a *new*
 * identity rather than re-addressing the original.
 */

/** Marks text on the clipboard as ours. Anything else is left alone. */
export const CLIPBOARD_KIND = "k-board/clipboard";

/** Where a paste lands when there is no pointer to land it on. */
export const PASTE_OFFSET = 12;

/**
 * The copyable form of an element.
 *
 * Its id is deliberately dropped. An id identifies a place in the document,
 * not a shape, and pasting one back would edit the original instead of adding
 * a copy — a "paste" that silently moves the thing you copied.
 */
export function serialise(items) {
  return JSON.stringify({
    kind: CLIPBOARD_KIND,
    version: 1,
    elements: items.map((item) => ({
      kind: item.kind,
      x: item.x,
      y: item.y,
      w: item.w,
      h: item.h,
      stroke: item.stroke,
      fill: item.fill,
      stroke_width: item.stroke_width,
      ...(item.points ? { points: item.points } : {}),
      ...(item.text != null ? { text: item.text, font_size: item.font_size } : {}),
    })),
  });
}

/**
 * Read a payload back, or `null` for anything that is not ours.
 *
 * The clipboard is shared with every other application on the machine, so this
 * is handed arbitrary text as a matter of course. Text that is not a k-board
 * payload is not an error and must not be reported as one.
 */
export function parse(text) {
  if (typeof text !== "string" || !text.includes(CLIPBOARD_KIND)) return null;
  let payload;
  try {
    payload = JSON.parse(text);
  } catch {
    return null;
  }
  if (payload?.kind !== CLIPBOARD_KIND || !Array.isArray(payload.elements)) return null;

  const elements = payload.elements.filter(
    (element) =>
      typeof element?.kind === "string" &&
      ["x", "y", "w", "h"].every((key) => Number.isFinite(element[key])),
  );
  return elements.length > 0 ? elements : null;
}

/** The box containing a set of copied elements, in their original coordinates. */
function extent(elements) {
  let minX = Infinity;
  let minY = Infinity;
  let maxX = -Infinity;
  let maxY = -Infinity;
  for (const element of elements) {
    const points = element.points ?? [
      [element.x, element.y],
      [element.x + element.w, element.y + element.h],
    ];
    for (const [x, y] of points) {
      minX = Math.min(minX, x);
      minY = Math.min(minY, y);
      maxX = Math.max(maxX, x);
      maxY = Math.max(maxY, y);
    }
  }
  return { minX, minY, maxX, maxY };
}

/**
 * Move copied elements so their centre lands on `at`, or nudge them when there
 * is nowhere in particular to put them.
 *
 * Centring on the pointer is what makes paste predictable across boards: the
 * original coordinates mean nothing on a canvas that has been panned somewhere
 * else, or on a different board entirely.
 */
export function place(elements, at) {
  const { minX, minY, maxX, maxY } = extent(elements);
  const [dx, dy] = at
    ? [at.x - (minX + maxX) / 2, at.y - (minY + maxY) / 2]
    : [PASTE_OFFSET, PASTE_OFFSET];

  return elements.map((element) => ({
    ...element,
    x: element.x + dx,
    y: element.y + dy,
    ...(element.points
      ? { points: element.points.map(([x, y]) => [x + dx, y + dy]) }
      : {}),
  }));
}

/**
 * The command that recreates one element.
 *
 * A paste is an ordinary edit — the same command the drawing tools issue — so
 * it converges, replicates, and undoes without any special case anywhere.
 */
export function toCommand(element) {
  if (element.kind === "text") {
    return {
      cmd: "text",
      x: element.x,
      y: element.y,
      w: element.w,
      h: element.h,
      text: element.text ?? "",
      font_size: element.font_size ?? 20,
      stroke: element.stroke ?? 0,
    };
  }
  if (element.kind === "freedraw") {
    return {
      cmd: "stroke",
      points: element.points ?? [],
      stroke: element.stroke ?? 0,
      stroke_width: element.stroke_width ?? 2,
    };
  }
  return {
    cmd: "add",
    kind: element.kind,
    x: element.x,
    y: element.y,
    w: element.w,
    h: element.h,
    stroke: element.stroke ?? 0,
    fill: element.fill ?? 0,
    stroke_width: element.stroke_width ?? 2,
  };
}

/**
 * Put text on the system clipboard, falling back to an in-tab copy.
 *
 * `navigator.clipboard` needs a secure context and can be refused by the user,
 * so the fallback is not hypothetical. It is worse — it does not cross tabs —
 * but silently doing nothing would be worse still.
 */
export async function writeText(text, fallback) {
  try {
    await navigator.clipboard.writeText(text);
    return "system";
  } catch {
    fallback.text = text;
    return "local";
  }
}

/** Read the system clipboard, falling back to whatever this tab last copied. */
export async function readText(fallback) {
  try {
    const text = await navigator.clipboard.readText();
    if (parse(text)) return text;
  } catch {
    /* denied, unavailable, or not a secure context */
  }
  return fallback.text ?? "";
}
