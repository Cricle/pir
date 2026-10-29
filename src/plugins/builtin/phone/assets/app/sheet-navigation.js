import { renderSheet } from "./sheets-view.js";
import { el, state } from "./state.js";
import { sendRpc } from "./transport.js";

export function openSheet(mode = "actions") {
  state.sheetMode = mode;
  el.sheetModal.classList.remove("hidden");
  renderSheet();

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

export function closeSheet() {
  el.sheetModal.classList.add("hidden");
}
