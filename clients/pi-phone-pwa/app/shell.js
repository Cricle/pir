import { getServerBase } from "./config.js";
import { prepareSessionSelection } from "./commands.js";
import { refreshDevices } from "./devices.js";
import { escapeHtml, formatDateTime } from "./formatters.js";
import { closeAllPages, openSheet } from "./sheet-navigation.js";
import { state } from "./state.js";
import { sendRpc } from "./transport.js";
import { openTokenModal, storeToken } from "./ui.js";

let deviceRefreshTimer = 0;

export function scheduleDeviceRefresh(delayMs = 400) {
  clearTimeout(deviceRefreshTimer);
  deviceRefreshTimer = setTimeout(() => {
    deviceRefreshTimer = 0;
    void refreshDevices()
      .then(() => renderSidebar())
      .catch(() => {});
  }, delayMs);
}

export function initializeShell() {
  document.querySelector("#sidebar-sessions")?.addEventListener("click", (event) => {
    const target = event.target;
    if (!(target instanceof Element) || !target.closest("[data-sidebar-new-session]")) return;
    closeAllPages();
    if (!sendRpc({ type: "new_session" })) return;
    toggleSidebar(false);
  });
  document.querySelector("#sidebar-devices")?.addEventListener("click", (event) => {
    const target = event.target;
    if (!(target instanceof Element) || !target.closest("[data-sidebar-device]")) return;
    toggleSidebar(false);
    openSheet("devices");
  });
  document.querySelector("#sidebar-user")?.addEventListener("click", () => {
    toggleSidebar(false);
    openSheet("settings");
  });
  document.querySelector("#session-menu-button")?.addEventListener("click", () => {
    if (!document.querySelector("#page-view")?.classList.contains("hidden")) return;
    openSheet("actions");
  });
  document.querySelector("#sidebar-close-button")?.addEventListener("click", () => toggleSidebar(false));
  document.querySelector("#sidebar-scrim")?.addEventListener("click", () => toggleSidebar(false));
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && state.sidebarOpen) toggleSidebar(false);
  });
  window.addEventListener("resize", () => toggleSidebar(state.sidebarOpen), { passive: true });
  window.addEventListener("pi-phone-connected", () => {
    scheduleDeviceRefresh(800);
  });
  if (state.token) {
    void refreshDevices()
      .then(() => renderSidebar())
      .catch(() => {});
  }
  toggleSidebar(false);
  renderSidebar();
}

export function toggleSidebar(force) {
  const sidebar = document.querySelector("#app-sidebar");
  const scrim = document.querySelector("#sidebar-scrim");
  const trigger = document.querySelector("#session-sidebar-button");
  if (!sidebar) return;
  const mobile = window.matchMedia("(max-width: 900px)").matches;
  const open = typeof force === "boolean" ? force : !sidebar.classList.contains("open");
  sidebar.classList.toggle("open", open);
  scrim?.classList.toggle("hidden", !open);
  trigger?.setAttribute("aria-expanded", String(open));
  if (mobile) {
    sidebar.inert = !open;
    sidebar.setAttribute("aria-hidden", String(!open));
    document.body.classList.toggle("sidebar-open", open);
  } else {
    sidebar.inert = false;
    sidebar.removeAttribute("aria-hidden");
    document.body.classList.remove("sidebar-open");
  }
  state.sidebarOpen = open;
}

export function logout() {
  closeAllPages();
  state.manuallyClosed = true;
  try {
    state.socket?.close();
  } catch {
    // ignore
  }
  state.socket = null;
  state.token = "";
  storeToken("");
  toggleSidebar(false);
  openTokenModal();
}

export function renderSidebar() {
  const user = state.serverStatus?.user || state.status?.user || state.health?.user || null;
  const userNode = document.querySelector("#sidebar-user");
  if (userNode) {
    const username = user?.displayName || user?.username || "Not signed in";
    const server = (() => {
      try {
        return new URL(getServerBase()).host;
      } catch {
        return "";
      }
    })();
    const initial = escapeHtml(String(username).trim().charAt(0).toUpperCase() || "?");
    userNode.innerHTML = `
      <span class="sidebar-user-avatar ${state.socket?.readyState === WebSocket.OPEN ? "online" : ""}" aria-hidden="true">${initial}</span>
      <span class="sidebar-user-copy">
        <strong>${escapeHtml(username)}</strong>
        <small>${escapeHtml(server)}</small>
      </span>
    `;
  }

  const sessionsNode = document.querySelector("#sidebar-sessions");
  if (sessionsNode) {
    const sessions = [...(state.activeSessions || [])].sort((left, right) => {
      const leftCurrent = left.id === state.activeSessionId ? 1 : 0;
      const rightCurrent = right.id === state.activeSessionId ? 1 : 0;
      if (leftCurrent !== rightCurrent) return rightCurrent - leftCurrent;
      return String(left.label || "").localeCompare(String(right.label || ""));
    });
    sessionsNode.innerHTML = `
      <div class="sidebar-section-head">
        <span>Sessions</span>
        <button type="button" class="sidebar-plus" data-sidebar-new-session="1" aria-label="New session" title="New session">＋</button>
      </div>
      <div class="sidebar-list">
        ${
          sessions.length
            ? sessions
                .map(
                  (session) => `
              <button type="button" class="sidebar-session${session.id === state.activeSessionId ? " current" : ""}" data-sidebar-session="${escapeHtml(session.id)}" title="${escapeHtml(session.label || session.localId || "Session")}">
                <span class="sidebar-session-dot ${session.online === false ? "offline" : "online"}" aria-hidden="true"></span>
                <span class="sidebar-session-title">${escapeHtml(session.label || session.localId || "Session")}</span>
                <span class="sidebar-session-meta">${[
                  session.isStreaming ? "working" : "",
                  session.model?.name || "",
                ]
                  .filter(Boolean)
                  .map(escapeHtml)
                  .join(" · ")}</span>
              </button>`,
                )
                .join("")
            : '<div class="sidebar-empty">No live sessions.<br />Run <code>/phone-start</code> on your computer.</div>'
        }
      </div>
      ${
        state.catalogHasMore
          ? '<button type="button" class="sidebar-more" data-sidebar-more="1">Load more sessions</button>'
          : ""
      }
    `;
    for (const button of sessionsNode.querySelectorAll("[data-sidebar-session]")) {
      button.addEventListener("click", () => {
        closeAllPages();
        prepareSessionSelection(button.dataset.sidebarSession);
        if (window.matchMedia("(max-width: 900px)").matches) toggleSidebar(false);
      });
    }
    sessionsNode.querySelector("[data-sidebar-more]")?.addEventListener("click", () => {
      if (state.socket?.readyState === WebSocket.OPEN && state.catalogNextCursor) {
        state.catalogAppending = true;
        state.socket.send(
          JSON.stringify({
            kind: "sessions/list",
            cursor: state.catalogNextCursor,
            limit: 50,
          }),
        );
      }
    });
  }

  const devicesNode = document.querySelector("#sidebar-devices");
  if (devicesNode) {
    const devices = state.devices || [];
    devicesNode.innerHTML = `
      <div class="sidebar-section-head">
        <span>Devices</span>
        <span class="sidebar-count">${devices.length}</span>
      </div>
      <div class="sidebar-list">
        ${
          devices.length
            ? devices
                .map(
                  (device) => `
              <button type="button" class="sidebar-device" data-sidebar-device="${escapeHtml(device.id)}">
                <span class="sidebar-device-main">
                  <strong>${escapeHtml(device.name || device.id)}</strong>
                  <small>${escapeHtml(device.platform || "unknown")}${device.lastSeenAt ? ` · ${escapeHtml(formatDateTime(device.lastSeenAt))}` : ""}</small>
                </span>
                <span class="sidebar-device-state ${device.online ? "online" : "offline"}">${device.online ? "online" : "offline"}</span>
              </button>`,
                )
                .join("")
            : '<div class="sidebar-empty">No devices yet.</div>'
        }
      </div>
    `;
  }
}
