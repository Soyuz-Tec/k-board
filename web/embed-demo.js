const container = document.getElementById("whiteboard");
const status = document.getElementById("hostStatus");
const focusButton = document.getElementById("focus");
const flushButton = document.getElementById("flush");
const remountButton = document.getElementById("remount");

let board = null;
const requestedBase = new URL(location.href).searchParams.get("board");
const boardBaseUrl = new URL(requestedBase ?? "./", location.href);
const { KBoard } = await import(new URL("embed-sdk.js", boardBaseUrl));

function setControls(enabled) {
  focusButton.disabled = !enabled;
  flushButton.disabled = !enabled;
  remountButton.disabled = false;
  remountButton.textContent = enabled ? "Destroy" : "Remount";
}

async function mount() {
  status.textContent = "Mounting K-board…";
  remountButton.disabled = true;
  try {
    board = await KBoard.mount(container, {
      baseUrl: boardBaseUrl,
      scope: "sdk-demo",
      title: "K-board embedded whiteboard",
    });
    board.addEventListener("status", (event) => {
      status.textContent = `Embedded client: ${event.detail.text}`;
    });
    board.addEventListener("error", (event) => {
      status.textContent = `Embedded client error: ${event.detail.message}`;
    });
    board.addEventListener("openStandalone", (event) => window.open(event.detail.url, "_blank"));
    status.textContent = "Embedded client ready";
    setControls(true);
  } catch (error) {
    status.textContent = `Mount failed: ${error.message}`;
    setControls(false);
  }
}

focusButton.addEventListener("click", async () => {
  await board.focusBoard();
  status.textContent = "Board focused through the host API";
});

flushButton.addEventListener("click", async () => {
  const result = await board.flush();
  status.textContent = `Host flush completed · ${result.pending} pending batch(es)`;
});

remountButton.addEventListener("click", () => {
  if (board) {
    board.destroy();
    board = null;
    container.replaceChildren();
    status.textContent = "Embedded client destroyed cleanly";
    setControls(false);
  } else {
    mount();
  }
});

mount();
