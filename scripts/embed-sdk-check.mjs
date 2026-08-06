import assert from "node:assert/strict";
import {
  EMBED_CONTRACT_VERSION,
  EMBED_MESSAGES,
  isPortMessage,
  isReadyAnnouncement,
  normalizeBaseUrl,
  normalizeMountOptions,
  validateAccessToken,
  validateScope,
} from "../web/embed-contract.mjs";

assert.equal(validateScope("tenant-42:meeting_9.v2"), "tenant-42:meeting_9.v2");
for (const invalid of ["", "has space", "../board", "line\nbreak", "slash/board"]) {
  assert.throws(() => validateScope(invalid), `scope ${JSON.stringify(invalid)} must fail`);
}
assert.throws(() => validateScope("x".repeat(129)), /128 bytes/);

assert.equal(validateAccessToken(undefined), null);
assert.equal(validateAccessToken("scope-bound-grant"), "scope-bound-grant");
assert.throws(() => validateAccessToken(""), /non-empty/);

assert.equal(
  normalizeBaseUrl("https://board.example/app?secret=no#fragment", "https://ignored.example/").href,
  "https://board.example/app/",
);
assert.throws(() => normalizeBaseUrl("javascript:alert(1)", "https://board.example/"), /HTTP/);
assert.throws(() => normalizeBaseUrl("https://user:pass@board.example/", null), /credentials/);

const options = normalizeMountOptions(
  { scope: "tenant:board", baseUrl: "https://board.example", title: "  Meeting board  " },
  "https://fallback.example/",
);
assert.equal(options.baseUrl.href, "https://board.example/");
assert.equal(options.title, "Meeting board");
assert.equal(options.timeoutMs, 15_000);

assert.equal(
  isReadyAnnouncement({ type: EMBED_MESSAGES.ready, version: EMBED_CONTRACT_VERSION }),
  true,
);
assert.equal(isReadyAnnouncement({ type: EMBED_MESSAGES.ready, version: 2 }), false);
assert.equal(
  isPortMessage(
    { type: EMBED_MESSAGES.event, version: EMBED_CONTRACT_VERSION },
    EMBED_MESSAGES.event,
  ),
  true,
);

console.log("PASS  Embedded SDK contract validates scopes, grants, URLs, options, and messages");
