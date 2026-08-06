export const EMBED_CONTRACT_VERSION = 1;

export const EMBED_MESSAGES = Object.freeze({
  ready: "kboard:embed-ready",
  initialize: "kboard:initialize",
  event: "kboard:event",
  request: "kboard:request",
  response: "kboard:response",
});

const SCOPE_PATTERN = /^[A-Za-z0-9._:-]+$/;
const MAX_SCOPE_BYTES = 128;
const MAX_TOKEN_LENGTH = 8192;

export function isRecord(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

export function validateScope(value) {
  if (typeof value !== "string" || !SCOPE_PATTERN.test(value)) {
    throw new TypeError("scope must use only letters, numbers, dot, underscore, colon, or dash");
  }
  if (new TextEncoder().encode(value).length > MAX_SCOPE_BYTES) {
    throw new RangeError(`scope must be at most ${MAX_SCOPE_BYTES} bytes`);
  }
  return value;
}

export function validateAccessToken(value) {
  if (value === undefined || value === null) return null;
  if (typeof value !== "string" || value.length === 0 || value.length > MAX_TOKEN_LENGTH) {
    throw new TypeError("accessToken must be a non-empty bounded string");
  }
  return value;
}

export function normalizeBaseUrl(value, fallback) {
  const base = new URL(value ?? fallback);
  if (!['http:', 'https:'].includes(base.protocol) || base.username || base.password) {
    throw new TypeError("baseUrl must be an HTTP(S) URL without credentials");
  }
  base.search = "";
  base.hash = "";
  if (!base.pathname.endsWith("/")) base.pathname += "/";
  return base;
}

export function normalizeMountOptions(options, fallbackBaseUrl) {
  if (!isRecord(options)) throw new TypeError("mount options are required");
  return {
    scope: validateScope(options.scope),
    accessToken: validateAccessToken(options.accessToken),
    baseUrl: normalizeBaseUrl(options.baseUrl, fallbackBaseUrl),
    title:
      typeof options.title === "string" && options.title.trim()
        ? options.title.trim().slice(0, 200)
        : "Collaborative whiteboard",
    timeoutMs:
      Number.isInteger(options.timeoutMs) && options.timeoutMs >= 1_000 && options.timeoutMs <= 60_000
        ? options.timeoutMs
        : 15_000,
  };
}

export function isReadyAnnouncement(data) {
  return (
    isRecord(data) &&
    data.type === EMBED_MESSAGES.ready &&
    data.version === EMBED_CONTRACT_VERSION
  );
}

export function isPortMessage(data, type) {
  return (
    isRecord(data) &&
    data.type === type &&
    data.version === EMBED_CONTRACT_VERSION
  );
}
