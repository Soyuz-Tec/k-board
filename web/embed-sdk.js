import {
  EMBED_CONTRACT_VERSION,
  EMBED_MESSAGES,
  isPortMessage,
  isReadyAnnouncement,
  normalizeMountOptions,
} from "./embed-contract.mjs";

const DEFAULT_BASE_URL = new URL("./", import.meta.url);
const ELEMENT_NAME = "k-board";

export class KBoardElement extends HTMLElement {
  #configured = false;
  #started = false;
  #options = null;
  #frame = null;
  #port = null;
  #pending = new Map();
  #handshakeTimer = null;
  #readySettled = false;
  #resolveReady;
  #rejectReady;

  constructor() {
    super();
    this.ready = new Promise((resolve, reject) => {
      this.#resolveReady = resolve;
      this.#rejectReady = reject;
    });
  }

  configure(options) {
    if (this.#configured) throw new Error("K-board can only be configured once");
    this.#configured = true;
    this.#options = normalizeMountOptions(options, DEFAULT_BASE_URL);
    if (this.isConnected) this.#start();
    return this;
  }

  connectedCallback() {
    if (this.#configured) this.#start();
  }

  disconnectedCallback() {
    this.#teardown(new Error("K-board was removed from its host"));
  }

  whenReady() {
    return this.ready;
  }

  focusBoard() {
    return this.#request("focus");
  }

  flush() {
    return this.#request("flush");
  }

  resize({ width, height } = {}) {
    if (width !== undefined) this.style.width = typeof width === "number" ? `${width}px` : width;
    if (height !== undefined) this.style.height = typeof height === "number" ? `${height}px` : height;
  }

  destroy() {
    this.#teardown(new Error("K-board was destroyed"));
    this.remove();
  }

  #start() {
    if (this.#started) return;
    this.#started = true;
    const root = this.attachShadow({ mode: "open" });
    const stylesheet = document.createElement("link");
    stylesheet.rel = "stylesheet";
    stylesheet.href = new URL("embed-element.css", this.#options.baseUrl).href;
    const frame = document.createElement("iframe");
    frame.title = this.#options.title;
    frame.src = new URL("embed", this.#options.baseUrl).href;
    frame.referrerPolicy = "no-referrer";
    frame.setAttribute("allow", "clipboard-read; clipboard-write");
    frame.setAttribute("sandbox", "allow-scripts allow-same-origin allow-downloads");
    root.append(stylesheet, frame);
    this.#frame = frame;
    window.addEventListener("message", this.#announce);
    this.#handshakeTimer = setTimeout(
      () => this.#fail(new Error("K-board initialization timed out")),
      this.#options.timeoutMs,
    );
  }

  #announce = (event) => {
    if (
      event.source !== this.#frame?.contentWindow ||
      event.origin !== this.#options.baseUrl.origin ||
      !isReadyAnnouncement(event.data)
    ) {
      return;
    }
    window.removeEventListener("message", this.#announce);
    const channel = new MessageChannel();
    this.#port = channel.port1;
    this.#port.onmessage = (message) => this.#receive(message.data);
    this.#port.start();
    this.#frame.contentWindow.postMessage(
      {
        type: EMBED_MESSAGES.initialize,
        version: EMBED_CONTRACT_VERSION,
        scope: this.#options.scope,
        accessToken: this.#options.accessToken,
      },
      this.#options.baseUrl.origin,
      [channel.port2],
    );
    this.#options.accessToken = null;
  };

  #receive(data) {
    if (isPortMessage(data, EMBED_MESSAGES.event)) {
      this.#event(data.name, data.detail);
      if (data.name === "ready") {
        clearTimeout(this.#handshakeTimer);
        this.#settleReady(null);
      }
      return;
    }
    if (!isPortMessage(data, EMBED_MESSAGES.response) || typeof data.requestId !== "string") return;
    const pending = this.#pending.get(data.requestId);
    if (!pending) return;
    clearTimeout(pending.timer);
    this.#pending.delete(data.requestId);
    if (data.ok) pending.resolve(data.result);
    else pending.reject(new Error(data.error || "K-board request failed"));
  }

  #event(name, detail) {
    const enriched =
      name === "openStandalone"
        ? { ...detail, url: new URL(`#${this.#options.scope}`, this.#options.baseUrl).href }
        : detail;
    this.dispatchEvent(new CustomEvent(name, { detail: enriched, cancelable: true }));
  }

  #request(action) {
    if (!this.#port) return Promise.reject(new Error("K-board is not ready"));
    const requestId = crypto.randomUUID();
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.#pending.delete(requestId);
        reject(new Error(`K-board ${action} timed out`));
      }, this.#options.timeoutMs);
      this.#pending.set(requestId, { resolve, reject, timer });
      this.#port.postMessage({
        type: EMBED_MESSAGES.request,
        version: EMBED_CONTRACT_VERSION,
        requestId,
        action,
      });
    });
  }

  #fail(error) {
    this.#event("error", { message: error.message });
    this.#teardown(error);
  }

  #settleReady(error) {
    if (this.#readySettled) return;
    this.#readySettled = true;
    if (error) this.#rejectReady(error);
    else this.#resolveReady(this);
  }

  #teardown(error) {
    this.#settleReady(error);
    clearTimeout(this.#handshakeTimer);
    window.removeEventListener("message", this.#announce);
    this.#port?.close();
    this.#port = null;
    for (const pending of this.#pending.values()) {
      clearTimeout(pending.timer);
      pending.reject(error);
    }
    this.#pending.clear();
  }
}

if (!customElements.get(ELEMENT_NAME)) customElements.define(ELEMENT_NAME, KBoardElement);

export const KBoard = Object.freeze({
  async mount(container, options) {
    if (!(container instanceof Element)) throw new TypeError("container must be a DOM element");
    const board = document.createElement(ELEMENT_NAME);
    board.configure(options);
    container.append(board);
    await board.whenReady();
    return board;
  },
});
