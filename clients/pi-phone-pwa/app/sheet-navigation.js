import { pageSubtitleFor, pageTitleFor, renderSheet } from "./sheets-view.js";
import { el, state } from "./state.js";
import { sendRpc } from "./transport.js";
import { renderHeader } from "./ui.js";

/**
 * Pages replace the old modal sheets: the sidebar stays put and the center
 * column swaps between transcript and settings/devices/session views.
 */
export function openSheet(mode = "settings") {
  const wasHidden = el.pageView.classList.contains("hidden");
  enter(mode, true);
  if (wasHidden) {
    history.pushState({ piPhonePage: mode }, "");
  }
}

export function backPage() {
  if (state.pageStack.length > 0) {
    closeSheet();
    return;
  }
  history.back();
}

export function closeAllPages() {
  state.pageStack = [];
  closeSheet();
}

export function closeSheet() {
  const previous = state.pageStack.pop();
  if (previous) {
    enter(previous, false);
    return;
  }
  state.sheetMode = null;
  state.pageTitle = "";
  el.pageView.classList.add("hidden");
  el.pageBackButton.classList.add("hidden");
  el.chatView.classList.remove("hidden");
  document.body.classList.remove("page-open");
  renderHeader();
}

window.addEventListener("popstate", () => {
  if (!el.pageView.classList.contains("hidden")) {
    state.pageStack = [];
    closeSheet();
  }
});

function enter(mode, push) {
  const pageVisible = !el.pageView.classList.contains("hidden");
  if (push && pageVisible && state.sheetMode && state.sheetMode !== mode) {
    state.pageStack.push(state.sheetMode);
  }
  if (!pageVisible && push) {
    state.pageStack = [];
  }
  state.sheetMode = mode;
  el.chatView.classList.add("hidden");
  el.pageView.classList.remove("hidden");
  el.pageBackButton.classList.remove("hidden");
  document.body.classList.add("page-open");
  renderSheet();
  el.pageSubtitle.textContent = pageSubtitleFor(mode);
  state.pageTitle = pageTitleFor(mode);
  renderHeader();
  window.scrollTo({ top: 0, behavior: "auto" });

  if (mode === "actions") sendRpc({ type: "get_session_stats" });
  if (mode === "models") sendRpc({ type: "get_available_models" });
  if (mode === "commands") sendRpc({ type: "get_commands" });
  if (mode === "sessions" || mode === "active-sessions") {
    // Reset the unified panel's transient state on each open so a previous
    // search does not silently hide sessions.
    state.sessionsLoading = true;
    state.sessionQuery = "";
    state.sessionListLimit = 12;
    renderSheet();
    sendRpc({ type: "phone_list_sessions", includeSubagents: Boolean(state.showSubagents) });
  }
  if (mode === "tree") sendRpc({ type: "phone_get_tree" });
}
