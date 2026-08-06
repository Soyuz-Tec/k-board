/**
 * Browser binding for the k-board engine.
 *
 * The engine is a raw C ABI wasm module with no imports and no generated glue —
 * the same exports a native host calls through Rustler or P/Invoke. That is why
 * this file exists and why it is short: marshalling bytes is the entire job.
 *
 * The one non-obvious rule: never cache a view over `memory.buffer`. Any call
 * that allocates can grow the wasm memory, which detaches every existing
 * ArrayBuffer view. Each access below re-reads it.
 */

export const STATUS = Object.freeze({
  OK: 0,
  PANIC: 1,
  BAD_INPUT: 2,
  NO_BOARD: 3,
  REFUSED: 4,
});

const EXPECTED_ABI = 3;

export class EngineError extends Error {
  constructor(operation, status) {
    super(`k-board: ${operation} failed (${nameOf(status)})`);
    this.name = "EngineError";
    this.status = status;
  }
}

function nameOf(status) {
  return Object.keys(STATUS).find((key) => STATUS[key] === status) ?? `status ${status}`;
}

export async function loadEngine(url = "./kboard.wasm") {
  let source;
  try {
    source = await WebAssembly.instantiateStreaming(fetch(url), {});
  } catch {
    // Falls back when the server sends a MIME type other than application/wasm.
    const bytes = await (await fetch(url)).arrayBuffer();
    source = await WebAssembly.instantiate(bytes, {});
  }
  return new Engine(source.instance);
}

export class Engine {
  #exports;
  #encoder = new TextEncoder();
  #decoder = new TextDecoder();

  constructor(instance) {
    this.#exports = instance.exports;
    const abi = this.#exports.kb_abi_version();
    if (abi !== EXPECTED_ABI) {
      throw new Error(`k-board: ABI ${abi}, expected ${EXPECTED_ABI}`);
    }
  }

  /** Re-read every time: growing memory detaches prior views. */
  get #bytes() {
    return new Uint8Array(this.#exports.memory.buffer);
  }

  #withText(text, use) {
    const encoded = this.#encoder.encode(text);
    if (encoded.length === 0) return use(0, 0);

    const pointer = this.#exports.kb_alloc(encoded.length);
    if (pointer === 0) throw new Error("k-board: allocation failed");
    try {
      this.#bytes.set(encoded, pointer);
      return use(pointer, encoded.length);
    } finally {
      this.#exports.kb_free(pointer, encoded.length);
    }
  }

  /** Bytes produced by the most recent call. */
  #result() {
    const length = this.#exports.kb_last_len();
    if (length === 0) return "";
    const pointer = this.#exports.kb_last_ptr();
    return this.#decoder.decode(this.#bytes.subarray(pointer, pointer + length));
  }

  /** Open a board. The host decides who may do this; the engine does not. */
  open(scope, actor) {
    if (!Number.isSafeInteger(actor) || actor <= 0) {
      throw new Error("k-board: actor must be a positive safe integer");
    }
    const handle = this.#withText(scope, (pointer, length) =>
      this.#exports.kb_open(pointer, length, BigInt(actor)),
    );
    if (handle === 0) throw new Error("k-board: could not open board");
    return handle;
  }

  close(handle) {
    this.#exports.kb_close(handle);
  }

  /** Run a command. Returns the affected element id, or "" for board-wide ones. */
  exec(handle, command, nowMs = Date.now()) {
    const json = JSON.stringify(command);
    const status = this.#withText(json, (pointer, length) =>
      this.#exports.kb_exec(handle, pointer, length, nowMs),
    );
    if (status !== STATUS.OK) throw new EngineError(`exec ${command.cmd}`, status);
    return this.#result();
  }

  /** Merge peer operations. Accepts the raw JSON string off the wire. */
  merge(handle, opsJson) {
    const status = this.#withText(opsJson, (pointer, length) =>
      this.#exports.kb_merge(handle, pointer, length),
    );
    if (status !== STATUS.OK) throw new EngineError("merge", status);
    return Number(this.#result());
  }

  /** Merge a whole document — the join handshake. */
  load(handle, documentJson) {
    const status = this.#withText(documentJson, (pointer, length) =>
      this.#exports.kb_load(handle, pointer, length),
    );
    // REFUSED here means the server sent another tenant's board. That is a
    // routing bug and must be loud, not silently ignored.
    if (status !== STATUS.OK) throw new EngineError("load", status);
    return this.#result() === "true";
  }

  /** Drain operations produced locally but not yet broadcast. */
  pending(handle) {
    const status = this.#exports.kb_pending(handle);
    if (status !== STATUS.OK) throw new EngineError("pending", status);
    const json = this.#result();
    return json ? JSON.parse(json) : [];
  }

  /**
   * Reverse this actor's most recent change. Returns whether anything moved.
   *
   * The reversal is an ordinary edit and shows up in `pending()` like any
   * other, so the caller broadcasts it without special handling.
   */
  undo(handle, nowMs = Date.now()) {
    const status = this.#exports.kb_undo(handle, nowMs);
    if (status !== STATUS.OK) throw new EngineError("undo", status);
    return this.#result() === "true";
  }

  /** Reapply the most recently undone change. */
  redo(handle, nowMs = Date.now()) {
    const status = this.#exports.kb_redo(handle, nowMs);
    if (status !== STATUS.OK) throw new EngineError("redo", status);
    return this.#result() === "true";
  }

  /**
   * What the history currently allows.
   *
   * One call rather than two so a toolbar cannot render half-updated.
   */
  history(handle) {
    const status = this.#exports.kb_history(handle);
    if (status !== STATUS.OK) throw new EngineError("history", status);
    const [canUndo, canRedo] = this.#result().split(",");
    return { canUndo: canUndo === "true", canRedo: canRedo === "true" };
  }

  /** Render-ready scene, in paint order. */
  scene(handle) {
    const status = this.#exports.kb_scene(handle);
    if (status !== STATUS.OK) throw new EngineError("scene", status);
    const json = this.#result();
    return json ? JSON.parse(json) : [];
  }
}
