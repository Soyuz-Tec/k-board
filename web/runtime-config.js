import {
  EMBED_CONTRACT_VERSION,
  EMBED_MESSAGES,
  isPortMessage,
  isRecord,
  validateAccessToken,
  validateScope,
} from "./embed-contract.mjs";

class HostBridge {
  constructor(port, hostOrigin) {
    this.port = port;
    this.hostOrigin = hostOrigin;
    this.handlers = new Map();
    this.port.onmessage = (event) => this.#receive(event.data);
    this.port.start();
  }

  emit(name, detail = {}) {
    this.port.postMessage({
      type: EMBED_MESSAGES.event,
      version: EMBED_CONTRACT_VERSION,
      name,
      detail,
    });
  }

  handle(action, handler) {
    this.handlers.set(action, handler);
  }

  async #receive(data) {
    if (!isPortMessage(data, EMBED_MESSAGES.request)) return;
    if (typeof data.requestId !== "string" || typeof data.action !== "string") return;
    const handler = this.handlers.get(data.action);
    if (!handler) return this.#respond(data.requestId, false, null, "unsupported request");
    try {
      const result = await handler();
      this.#respond(data.requestId, true, result ?? null, null);
    } catch (error) {
      this.#respond(data.requestId, false, null, error instanceof Error ? error.message : "failed");
    }
  }

  #respond(requestId, ok, result, error) {
    this.port.postMessage({
      type: EMBED_MESSAGES.response,
      version: EMBED_CONTRACT_VERSION,
      requestId,
      ok,
      result,
      error,
    });
  }
}

function standaloneConfiguration() {
  const bootstrapUrl = new URL(location.href);
  const accessToken = bootstrapUrl.searchParams.get("token");
  if (accessToken !== null) {
    bootstrapUrl.searchParams.delete("token");
    history.replaceState(
      null,
      "",
      `${bootstrapUrl.pathname}${bootstrapUrl.search}${bootstrapUrl.hash}`,
    );
  }
  return {
    mode: "standalone",
    scope: location.hash.slice(1) || "demo",
    accessToken,
    baseUrl: new URL("./", location.href),
    bridge: null,
  };
}

function embeddedConfiguration() {
  if (window.parent === window) {
    return Promise.reject(new Error("the embedded shell must be mounted by the K-board SDK"));
  }
  return new Promise((resolve, reject) => {
    const timeout = setTimeout(() => {
      window.removeEventListener("message", initialize);
      reject(new Error("host initialization timed out"));
    }, 15_000);

    function initialize(event) {
      if (event.source !== window.parent || !isRecord(event.data)) return;
      if (
        event.data.type !== EMBED_MESSAGES.initialize ||
        event.data.version !== EMBED_CONTRACT_VERSION ||
        event.ports.length !== 1
      ) {
        return;
      }
      try {
        const scope = validateScope(event.data.scope);
        const accessToken = validateAccessToken(event.data.accessToken);
        clearTimeout(timeout);
        window.removeEventListener("message", initialize);
        resolve({
          mode: "embedded",
          scope,
          accessToken,
          baseUrl: new URL("./", location.href),
          bridge: new HostBridge(event.ports[0], event.origin),
        });
      } catch (error) {
        clearTimeout(timeout);
        window.removeEventListener("message", initialize);
        reject(error);
      }
    }

    window.addEventListener("message", initialize);
    window.parent.postMessage(
      { type: EMBED_MESSAGES.ready, version: EMBED_CONTRACT_VERSION },
      "*",
    );
  });
}

export function isEmbeddedShell(pathname = location.pathname) {
  return pathname.replace(/\/+$/, "").endsWith("/embed");
}

export async function resolveRuntimeConfiguration() {
  return isEmbeddedShell() ? embeddedConfiguration() : standaloneConfiguration();
}
