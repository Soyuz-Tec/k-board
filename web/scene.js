/**
 * What a scene *is*, independent of how it is shown.
 *
 * Colour, geometry, and the two back-ends that turn geometry into output:
 * canvas for the screen, SVG for export.
 *
 * The reason they live together is that an export which does not match the
 * screen is worse than no export at all — you only discover the difference
 * after sending the file to someone. So each shape is described **once**, as a
 * sequence of path commands, and both back-ends consume that description.
 * A canvas that draws one thing and an SVG that draws another is not a bug
 * that can occur here; there is only one drawing.
 *
 * Nothing in this file touches the DOM, which is also what makes it testable
 * in Node.
 */

// -- colour ----------------------------------------------------------------

/** `#rrggbb` and an alpha byte into the engine's packed `0xRRGGBBAA`. */
export function pack(hex, alpha = 255) {
  const value = Number.parseInt(hex.slice(1), 16);
  return (
    ((((value >>> 16) & 255) << 24) |
      (((value >>> 8) & 255) << 16) |
      ((value & 255) << 8) |
      alpha) >>>
    0
  );
}

/** Packed colour into a CSS colour — understood by canvas and SVG alike. */
export function unpack(packed) {
  const r = (packed >>> 24) & 255;
  const g = (packed >>> 16) & 255;
  const b = (packed >>> 8) & 255;
  const a = (packed & 255) / 255;
  return `rgba(${r},${g},${b},${a})`;
}

// -- typography ------------------------------------------------------------

/**
 * The one font both back-ends name.
 *
 * A generic family rather than a specific face: an SVG opened on a machine
 * without the font falls back to something, and the width the author measured
 * would no longer be the width it draws at. Sticking to what every system has
 * keeps the measured box honest in more places than naming a favourite would.
 */
export const FONT_FAMILY = "ui-sans-serif, system-ui, -apple-system, Segoe UI, sans-serif";

/** Multiplied by the font size. Matches what the editor's textarea uses. */
export const LINE_HEIGHT = 1.25;
export const STICKY_PADDING = 16;

export function fontFor(size) {
  return `${size}px ${FONT_FAMILY}`;
}

/** Text is stored with its newlines; every consumer needs the same split. */
export function lines(item) {
  return String(item.text ?? "").split("\n");
}

// -- geometry --------------------------------------------------------------

/** The axis-aligned box an element occupies, with negative extents normalised. */
export function bounds(item) {
  if (item.points?.length) {
    let minX = Infinity;
    let minY = Infinity;
    let maxX = -Infinity;
    let maxY = -Infinity;
    for (const [x, y] of item.points) {
      minX = Math.min(minX, x);
      minY = Math.min(minY, y);
      maxX = Math.max(maxX, x);
      maxY = Math.max(maxY, y);
    }
    return { x: minX, y: minY, w: maxX - minX, h: maxY - minY };
  }
  return {
    x: Math.min(item.x, item.x + item.w),
    y: Math.min(item.y, item.y + item.h),
    w: Math.abs(item.w),
    h: Math.abs(item.h),
  };
}

/**
 * The box containing every element, or `null` for an empty scene.
 *
 * Stroke width is included: a line drawn exactly on the boundary is half
 * outside the geometric box, and an export cropped to the geometry clips it.
 * So is rotation, for the same reason.
 */
export function sceneBounds(scene) {
  if (scene.length === 0) return null;
  let minX = Infinity;
  let minY = Infinity;
  let maxX = -Infinity;
  let maxY = -Infinity;
  for (const item of scene) {
    const bleed = (item.stroke_width || 2) / 2;
    // Corners rather than the box: a rotated shape reaches past its own
    // axis-aligned extents, and an export cropped to those clips its edges off.
    for (const [x, y] of corners(item)) {
      minX = Math.min(minX, x - bleed);
      minY = Math.min(minY, y - bleed);
      maxX = Math.max(maxX, x + bleed);
      maxY = Math.max(maxY, y + bleed);
    }
  }
  return { x: minX, y: minY, w: maxX - minX, h: maxY - minY };
}

/**
 * The rotation an element carries, or null when it is upright.
 *
 * Kept as a transform rather than baked into the figures: rotated glyphs cannot
 * be expressed as rotated coordinates, so a description that pre-rotated its
 * points would have nothing to say about text. One transform covers every kind.
 */
export function rotation(item) {
  const angle = item.angle || 0;
  if (angle === 0) return null;
  const box = bounds(item);
  return { angle, cx: box.x + box.w / 2, cy: box.y + box.h / 2 };
}

/**
 * Move a point into an element's own frame.
 *
 * Hit-testing happens in board coordinates, but a rotated element's box is not
 * axis-aligned there. Rotating the *pointer* backwards is cheaper and simpler
 * than rotating the box forwards, and gives the same answer.
 */
export function intoLocal(item, x, y) {
  const spin = rotation(item);
  if (!spin) return [x, y];
  const cos = Math.cos(-spin.angle);
  const sin = Math.sin(-spin.angle);
  const dx = x - spin.cx;
  const dy = y - spin.cy;
  return [spin.cx + dx * cos - dy * sin, spin.cy + dx * sin + dy * cos];
}

/** The four corners of an element's box, in board coordinates. */
export function corners(item) {
  const box = bounds(item);
  const spin = rotation(item);
  const points = [
    [box.x, box.y],
    [box.x + box.w, box.y],
    [box.x + box.w, box.y + box.h],
    [box.x, box.y + box.h],
  ];
  if (!spin) return points;
  const cos = Math.cos(spin.angle);
  const sin = Math.sin(spin.angle);
  return points.map(([x, y]) => {
    const dx = x - spin.cx;
    const dy = y - spin.cy;
    return [spin.cx + dx * cos - dy * sin, spin.cy + dx * sin + dy * cos];
  });
}

/**
 * One element as figures both back-ends understand.
 *
 * A figure is either a path — `M`/`L`/`Q` commands, optionally closed — or an
 * ellipse, which neither back-end expresses well as a path. `fillable` is false
 * for shapes that are strokes rather than outlines: an arrow with a fill would
 * paint the region between its head and its tail.
 */
export function geometry(item) {
  const box = bounds(item);

  switch (item.kind) {
    case "text":
      // Not a path. Both back-ends draw glyphs, and neither can express them
      // as one — so the description says so rather than pretending otherwise.
      {
        const size = item.font_size || 20;
        return {
          fillable: false,
          figures: [
            {
              text: {
                size,
                // Baselines are computed here rather than in each back-end.
                // The same arithmetic written twice is two formulas that
                // merely happen to agree today.
                rows: lines(item).map((content, index) => ({
                  content,
                  x: box.x,
                  y: box.y + size * LINE_HEIGHT * (index + 1) - size * 0.25,
                })),
              },
            },
          ],
        };
      }

    case "rectangle":
      if (item.role === "sticky") {
        const size = item.font_size || 20;
        return {
          fillable: true,
          figures: [
            {
              close: true,
              path: [
                ["M", box.x, box.y],
                ["L", box.x + box.w, box.y],
                ["L", box.x + box.w, box.y + box.h],
                ["L", box.x, box.y + box.h],
              ],
            },
            {
              text: {
                size,
                rows: lines(item).map((content, index) => ({
                  content,
                  x: box.x + STICKY_PADDING,
                  y:
                    box.y +
                    STICKY_PADDING +
                    size * LINE_HEIGHT * (index + 1) -
                    size * 0.25,
                })),
              },
            },
          ],
        };
      }
      return rectangleGeometry(box);

    case "ellipse":
      return {
        fillable: true,
        figures: [
          {
            ellipse: {
              cx: box.x + box.w / 2,
              cy: box.y + box.h / 2,
              rx: box.w / 2,
              ry: box.h / 2,
            },
          },
        ],
      };

    case "diamond":
      return {
        fillable: true,
        figures: [
          {
            close: true,
            path: [
              ["M", box.x + box.w / 2, box.y],
              ["L", box.x + box.w, box.y + box.h / 2],
              ["L", box.x + box.w / 2, box.y + box.h],
              ["L", box.x, box.y + box.h / 2],
            ],
          },
        ],
      };

    case "line":
      return {
        fillable: false,
        figures: [{ path: [["M", item.x, item.y], ["L", item.x + item.w, item.y + item.h]] }],
      };

    case "arrow": {
      // Drawn from the raw extents rather than the normalised box, because an
      // arrow pointing up-left is not the same as one pointing down-right.
      const x2 = item.x + item.w;
      const y2 = item.y + item.h;
      const angle = Math.atan2(item.h, item.w);
      const head = Math.min(16, Math.hypot(item.w, item.h) / 3);
      return {
        fillable: false,
        figures: [
          { path: [["M", item.x, item.y], ["L", x2, y2]] },
          {
            path: [
              ["M", x2, y2],
              ["L", x2 - head * Math.cos(angle - 0.4), y2 - head * Math.sin(angle - 0.4)],
            ],
          },
          {
            path: [
              ["M", x2, y2],
              ["L", x2 - head * Math.cos(angle + 0.4), y2 - head * Math.sin(angle + 0.4)],
            ],
          },
        ],
      };
    }

    case "freedraw": {
      const points = item.points ?? [];
      if (points.length === 0) return { fillable: false, figures: [] };
      const path = [["M", points[0][0], points[0][1]]];
      // Quadratic smoothing through midpoints: cheap, and much better looking
      // than straight segments between raw pointer samples.
      for (let index = 1; index < points.length - 1; index += 1) {
        const [cx, cy] = points[index];
        const [nx, ny] = points[index + 1];
        path.push(["Q", cx, cy, (cx + nx) / 2, (cy + ny) / 2]);
      }
      const last = points[points.length - 1];
      path.push(["L", last[0], last[1]]);
      return { fillable: false, figures: [{ path }] };
    }

    default:
      return rectangleGeometry(box);
  }
}

function rectangleGeometry(box) {
  return {
    fillable: true,
    figures: [
      {
        close: true,
        path: [
          ["M", box.x, box.y],
          ["L", box.x + box.w, box.y],
          ["L", box.x + box.w, box.y + box.h],
          ["L", box.x, box.y + box.h],
        ],
      },
    ],
  };
}

// -- canvas back-end -------------------------------------------------------

/** Paint one element into any 2D context — the screen's, or an export's. */
export function drawShape(context, item) {
  const { fillable, figures } = geometry(item);
  const filled = fillable && (item.fill & 255) !== 0;
  const spin = rotation(item);
  const alpha = item.opacity ?? 1;
  const faded = alpha < 1;

  if (spin || faded) {
    context.save();
  }
  if (faded) context.globalAlpha = alpha;
  if (spin) {
    context.translate(spin.cx, spin.cy);
    context.rotate(spin.angle);
    context.translate(-spin.cx, -spin.cy);
  }

  context.strokeStyle = unpack(item.stroke);
  context.lineWidth = item.stroke_width || 2;
  context.lineJoin = "round";
  context.lineCap = "round";
  if (filled) context.fillStyle = unpack(item.fill);

  for (const figure of figures) {
    if (figure.text) {
      // Filled, not stroked: outlined glyphs are illegible at label sizes.
      context.fillStyle = unpack(item.stroke);
      context.font = fontFor(figure.text.size);
      context.textBaseline = "alphabetic";
      for (const row of figure.text.rows) context.fillText(row.content, row.x, row.y);
      continue;
    }

    context.beginPath();
    if (figure.ellipse) {
      const { cx, cy, rx, ry } = figure.ellipse;
      context.ellipse(cx, cy, rx, ry, 0, 0, Math.PI * 2);
    } else {
      for (const [command, ...args] of figure.path) {
        if (command === "M") context.moveTo(args[0], args[1]);
        else if (command === "L") context.lineTo(args[0], args[1]);
        else context.quadraticCurveTo(args[0], args[1], args[2], args[3]);
      }
      if (figure.close) context.closePath();
    }
    if (filled) context.fill();
    context.stroke();
  }

  if (spin || faded) context.restore();
}

// -- SVG back-end ----------------------------------------------------------

/** Two decimals is below a pixel at any sane export scale, and keeps files small. */
function n(value) {
  return Number.isFinite(value) ? String(Math.round(value * 100) / 100) : "0";
}

function escapeText(value) {
  return String(value)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

function pathData(figure) {
  return figure.path
    .map(([command, ...args]) => `${command}${args.map(n).join(" ")}`)
    .join(" ")
    .concat(figure.close ? " Z" : "");
}

function elementMarkup(item) {
  const { fillable, figures } = geometry(item);
  const filled = fillable && (item.fill & 255) !== 0;
  const alpha = item.opacity ?? 1;
  const paint =
    `fill="${filled ? unpack(item.fill) : "none"}" stroke="${unpack(item.stroke)}" ` +
    `stroke-width="${n(item.stroke_width || 2)}" stroke-linejoin="round" stroke-linecap="round"` +
    // Omitted when solid, so the common case does not carry an attribute
    // saying "unchanged" on every element in the file.
    (alpha < 1 ? ` opacity="${n(alpha)}"` : "");

  const markup = figures.map((figure) => {
    if (figure.text) {
      const { rows, size } = figure.text;
      const spans = rows
        .map(
          (row) =>
            `<tspan x="${n(row.x)}" y="${n(row.y)}">${escapeText(row.content)}</tspan>`,
        )
        .join("");
      // `xml:space` preserved so leading indentation a user typed survives the
      // trip; SVG collapses whitespace by default and would silently reflow it.
      return (
        `<text xml:space="preserve" font-family="${escapeText(FONT_FAMILY)}" ` +
        `font-size="${n(size)}" fill="${unpack(item.stroke)}"` +
        (alpha < 1 ? ` opacity="${n(alpha)}"` : "") +
        `>${spans}</text>`
      );
    }
    if (figure.ellipse) {
      const { cx, cy, rx, ry } = figure.ellipse;
      return `<ellipse cx="${n(cx)}" cy="${n(cy)}" rx="${n(rx)}" ry="${n(ry)}" ${paint}/>`;
    }
    return `<path d="${pathData(figure)}" ${paint}/>`;
  });

  const spin = rotation(item);
  if (!spin) return markup;
  // Degrees, because that is what SVG takes — the only place in the codebase
  // where the angle is not in radians, and the conversion is here so it is the
  // only place that has to know.
  const degrees = n((spin.angle * 180) / Math.PI);
  return [
    `<g transform="rotate(${degrees} ${n(spin.cx)} ${n(spin.cy)})">${markup.join("")}</g>`,
  ];
}

/**
 * Serialise a whole scene as a standalone SVG document.
 *
 * Cropped to the content rather than to the viewport: an export is the board,
 * not a screenshot of where you happened to be looking. The grid is left out
 * for the same reason — it is an affordance for drawing, not content.
 *
 * Returns `null` for an empty scene, so callers say so rather than producing a
 * blank file that looks like a failure.
 */
export function toSvg(scene, { padding = 16, background = "#ffffff", title = "" } = {}) {
  const box = sceneBounds(scene);
  if (box === null) return null;

  const x = box.x - padding;
  const y = box.y - padding;
  const width = box.w + padding * 2;
  const height = box.h + padding * 2;

  const body = scene.flatMap(elementMarkup).join("\n  ");
  // A titled SVG is announced by assistive technology; an untitled one is an
  // unlabelled graphic.
  const label = title ? `\n  <title>${escapeText(title)}</title>` : "";

  return (
    `<svg xmlns="http://www.w3.org/2000/svg" width="${n(width)}" height="${n(height)}" ` +
    `viewBox="${n(x)} ${n(y)} ${n(width)} ${n(height)}" role="img">${label}\n` +
    `  <rect x="${n(x)}" y="${n(y)}" width="${n(width)}" height="${n(height)}" fill="${escapeText(
      background,
    )}"/>\n  ${body}\n</svg>\n`
  );
}
