import { encodeWsToken, getServerBase, storeStoredToken } from "./config.js";
import { renderLoadEarlier } from "./messages.js";
import { state } from "./state.js";
import { openTokenModal, renderHeader, renderQuota, showBanner, showToast } from "./ui.js";

/**
 * Absolute URL of the phone server. The PWA default is same-origin; a
 * `?server=` deep link or the saved server setting can point it at a central
 * relay instead.
 */
export function apiUrl(path) {
  return `${getServerBase()}${path}`;
}

export function wsUrl(path) {
  const url = new URL(apiUrl(path));
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  return url;
}

export function clearReconnectTimer() {
  if (!state.reconnectTimer) return;
  clearTimeout(state.reconnectTimer);
  state.reconnectTimer = null;
}

export function sendRpc(command) {
  if (state.socket?.readyState !== WebSocket.OPEN) {
    showToast("Not connected to Pi.", "error");
    return false;
  }
  state.socket.send(JSON.stringify({ kind: "rpc", command }));
  return true;
}

export function sendLocalCommand(command) {
  if (state.socket?.readyState !== WebSocket.OPEN) {
    showToast("Not connected to Pi.", "error");
    return false;
  }

  state.socket.send(JSON.stringify({ kind: "local-command", command }));
  return true;
}

export function requestReload() {
  if (state.status?.isStreaming || state.snapshotState?.isStreaming) {
    showToast("Wait for the current response to finish before reloading.", "error");
    return false;
  }

  if (state.snapshotState?.isCompacting) {
    showToast("Wait for compaction to finish before reloading.", "error");
    return false;
  }

  return sendLocalCommand("reload");
}

export async function refreshQuota({ force = false } = {}) {
  const model = state.snapshotState?.model;
  const currentModel = model && typeof model === "object"
    ? {
        provider: typeof model.provider === "string" ? model.provider : "",
        modelId: typeof model.id === "string" ? model.id : "",
      }
    : null;

  if (!currentModel || currentModel.provider !== "openai-codex" || !/^gpt-/i.test(currentModel.modelId || "")) {
    state.quota = null;
    renderQuota();
    return;
  }

  const requestId = ++state.quotaRequestId;

  try {
    const url = new URL(apiUrl("/api/quota"));
    url.searchParams.set("provider", currentModel.provider);
    url.searchParams.set("modelId", currentModel.modelId);
    if (force) url.searchParams.set("force", "1");

    const response = await fetch(url, {
      cache: "no-store",
      headers: state.token ? { Authorization: `Bearer ${state.token}` } : {},
    });
    if (!response.ok) throw new Error(`Quota request failed (${response.status})`);

    const quota = await response.json();
    if (requestId !== state.quotaRequestId) return;
    state.quota = quota;
  } catch {
    if (requestId !== state.quotaRequestId) return;
    if (!state.quota?.visible) {
      state.quota = null;
    }
  }

  renderQuota();
}

/** Request a page of older messages (windowing keeps snapshots small). */
export function requestEarlierMessages() {
  if (state.socket?.readyState !== WebSocket.OPEN) {
    showToast("Not connected to Pi.", "error");
    return false;
  }
  if (state.loadingEarlier) return false;

  state.loadingEarlier = true;
  renderLoadEarlier();

  const requestId = Date.now();
  state.socket.send(JSON.stringify({
    kind: "load-earlier-messages",
    // The server slices by absolute index in the full transcript.
    before: Math.max(0, state.totalMessageCount - state.loadedMessageCount),
    count: 60,
    requestId,
  }));
  return true;
}

export function refreshAll(options = {}) {
  const { forceQuota = false } = options;

  if (state.socket?.readyState === WebSocket.OPEN && state.activeSessionId) {
    state.socket.send(JSON.stringify({ kind: "refresh" }));
    sendRpc({ type: "get_commands" });
    sendRpc({ type: "get_available_models" });
  }

  void refreshQuota({ force: forceQuota });
}

/**
 * Reconnect delay for the periodic probe while the app is hidden.
 * Long: a backgrounded tab keeps burning battery and mobile data otherwise.
 */
const BACKGROUND_RETRY_MS = 10_000;
/** Reconnect delay for the foreground probe (visible + focused). */
const FOREGROUND_RETRY_MS = 1_200;
/** Reset the probe backoff after the connection has been stable this long. */
const STABLE_CONNECTION_MS = 20_000;

let retryDelayMs = 0;
let lastConnectedAt = 0;

export function isAppVisible() {
  return typeof document === "undefined" || document.visibilityState !== "hidden";
}

function baseRetryDelay() {
  return isAppVisible() ? FOREGROUND_RETRY_MS : BACKGROUND_RETRY_MS;
}

/** True while the socket is open or actively handshaking. */
function socketBusy() {
  return state.socket && (state.socket.readyState === WebSocket.OPEN || state.socket.readyState === WebSocket.CONNECTING);
}

/**
 * Kick a reconnect attempt right now, unless one is already in flight.
 * Used by the visibility handler: coming back from the background should not
 * wait out the remaining backoff timer before the user sees fresh data.
 */
export function pokeConnection({ handleEnvelope, handleAuthFailure }) {
  if (state.manuallyClosed) return;
  if (socketBusy()) return;
  clearReconnectTimer();
  connectSocket({ handleEnvelope, handleAuthFailure });
}

function scheduleReconnect({ handleEnvelope, handleAuthFailure }) {
  if (state.manuallyClosed) return;
  clearReconnectTimer();
  retryDelayMs = baseRetryDelay();
  state.reconnectTimer = setTimeout(() => {
    state.reconnectTimer = null;
    if (state.manuallyClosed || socketBusy()) return;
    connectSocket({ handleEnvelope, handleAuthFailure });
  }, retryDelayMs);
}

export function connectSocket({ handleEnvelope, handleAuthFailure }) {
  clearReconnectTimer();
  if (state.socket && (state.socket.readyState === WebSocket.OPEN || state.socket.readyState === WebSocket.CONNECTING)) {
    return;
  }

  const url = wsUrl("/ws");
  const protocols = state.token
    ? ["pi-phone.v1", `bearer.${encodeWsToken(state.token)}`]
    : ["pi-phone.v1"];
  const socket = new WebSocket(url, protocols);
  state.socket = socket;
  state.selectionSocket = null;
  renderHeader();

  socket.addEventListener("open", () => {
    clearReconnectTimer();
    showBanner("");
    renderHeader();
    window.dispatchEvent(new CustomEvent("pi-phone-connected"));
    refreshAll();
  });

  socket.addEventListener("message", (event) => {
    try {
      handleEnvelope(JSON.parse(event.data));
    } catch {
      showToast("Received malformed data from server.", "error");
    }
  });

  socket.addEventListener("close", (event) => {
    if (state.socket === socket) {
      state.socket = null;
      state.selectionSocket = null;
    }
    renderHeader();
    if (event.code === 4001) {
      handleAuthFailure();
      return;
    }
    if (event.code === 4002) {
      showBanner("The selected device is offline. Reconnect it with /phone-start.", "error");
      return;
    }
    if (event.code === 1008) {
      handleAuthFailure();
      return;
    }
    if (event.code === 1006) {
      showBanner("Connection lost. Retrying…", "error");
    }
    scheduleReconnect({ handleEnvelope, handleAuthFailure });
  });

  socket.addEventListener("error", () => {
    renderHeader();
  });
}

export async function loadHealth() {
  const response = await fetch(apiUrl("/api/health"), {
    cache: "no-store",
    headers: state.token ? { Authorization: `Bearer ${state.token}` } : {},
  });
  if (response.status === 401) {
    state.token = "";
    storeStoredToken("");
    openTokenModal();
    throw new Error("Access token rejected. Enter it again.");
  }
  if (!response.ok) throw new Error(`Health check failed (${response.status})`);
  state.health = await response.json();
  state.status = state.health;
  renderHeader();
}

export async function boot({ handleEnvelope, handleAuthFailure }) {
  try {
    await loadHealth();
  } catch (error) {
    showBanner(error instanceof Error ? error.message : "Failed to reach server.", "error");
    document.querySelector(".app-shell")?.classList.add("ready");
    return;
  }

  // First frame is ready (DOM parsed, real stylesheet applied): reveal the
  // shell. The inline critical CSS keeps the pre-removal paint dark.
  document.querySelector(".app-shell")?.classList.add("ready");

  if (!state.token) openTokenModal();
  else connectSocket({ handleEnvelope, handleAuthFailure });

  // Mobile browsers throttle timers and kill sockets while backgrounded, so
  // the app comes back to a dead socket. Handle the wake-up here: reconnect
  // immediately and pull a fresh snapshot instead of waiting out the probe.
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState !== "visible") return;
    lastConnectedAt = 0;
    pokeConnection({ handleEnvelope, handleAuthFailure });
    if (state.socket?.readyState === WebSocket.OPEN) {
      state.socket.send(JSON.stringify({ kind: "refresh" }));
      void refreshQuota({ force: true });
    }
  });
  window.addEventListener("focus", () => {
    pokeConnection({ handleEnvelope, handleAuthFailure });
  });
  window.addEventListener("online", () => {
    pokeConnection({ handleEnvelope, handleAuthFailure });
  });

  if ("serviceWorker" in navigator) {
    navigator.serviceWorker.register("./sw.js").catch(() => {});
  }
}
