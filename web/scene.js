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
 */
export function sceneBounds(scene) {
  if (scene.length === 0) return null;
  let minX = Infinity;
  let minY = Infinity;
  let maxX = -Infinity;
  let maxY = -Infinity;
  for (const item of scene) {
    const box = bounds(item);
    const bleed = (item.stroke_width || 2) / 2;
    minX = Math.min(minX, box.x - bleed);
    minY = Math.min(minY, box.y - bleed);
    maxX = Math.max(maxX, box.x + box.w + bleed);
    maxY = Math.max(maxY, box.y + box.h + bleed);
  }
  return { x: minX, y: minY, w: maxX - minX, h: maxY - minY };
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
}

// -- canvas back-end -------------------------------------------------------

/** Paint one element into any 2D context — the screen's, or an export's. */
export function drawShape(context, item) {
  const { fillable, figures } = geometry(item);
  const filled = fillable && (item.fill & 255) !== 0;

  context.strokeStyle = unpack(item.stroke);
  context.lineWidth = item.stroke_width || 2;
  context.lineJoin = "round";
  context.lineCap = "round";
  if (filled) context.fillStyle = unpack(item.fill);

  for (const figure of figures) {
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
  const paint =
    `fill="${filled ? unpack(item.fill) : "none"}" stroke="${unpack(item.stroke)}" ` +
    `stroke-width="${n(item.stroke_width || 2)}" stroke-linejoin="round" stroke-linecap="round"`;

  return figures.map((figure) => {
    if (figure.ellipse) {
      const { cx, cy, rx, ry } = figure.ellipse;
      return `<ellipse cx="${n(cx)}" cy="${n(cy)}" rx="${n(rx)}" ry="${n(ry)}" ${paint}/>`;
    }
    return `<path d="${pathData(figure)}" ${paint}/>`;
  });
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
