import { renderCommandSuggestions } from "./autocomplete-controller.js";
import { createDeviceToken, refreshDevices, revokeDevice } from "./devices.js";
import { logout } from "./shell.js";
import {
  handleInsertOnlyLocalCommand,
  insertSlashCommand,
  prepareParentSessionNew,
  prepareSessionSelection,
  prepareSessionSpawn,
  tryHandleLocalCommand,
} from "./commands.js";
import { renderMessages } from "./messages.js";
import { el, state } from "./state.js";
import { closeSheet, openSheet } from "./sheet-navigation.js";
import { renderSheet } from "./sheets-view.js";
import { refreshAll, sendRpc } from "./transport.js";
import { applyTheme } from "./theme.js";
import { autoResizeTextarea, showToast } from "./ui.js";

export function sheetButtonActionKey(button) {
  return [
    button.getAttribute("data-sheet-action") || "",
    button.getAttribute("data-active-session-id") || "",
    button.getAttribute("data-session-path") || "",
    button.getAttribute("data-open-branch-entry") || "",
    button.getAttribute("data-fork-entry") || "",
    button.getAttribute("data-run-command") || "",
    button.getAttribute("data-run-local-command") || "",
    button.getAttribute("data-device-id") || "",
    button.getAttribute("data-theme-choice") || "",
    button.getAttribute("data-show-thinking") || "",
  ].join("|");
}

export function handleSheetButtonAction(button) {
  const themeChoice = button.getAttribute("data-theme-choice");
  if (themeChoice) {
    applyTheme(themeChoice);
    renderSheet();
    return true;
  }
  if (button.hasAttribute("data-show-thinking")) {
    state.showThinking = !state.showThinking;
    try {
      localStorage.setItem("pi-phone-show-thinking", String(state.showThinking));
    } catch {
      // Private mode may reject storage; keep the in-memory choice.
    }
    renderMessages();
    renderSheet();
    return true;
  }
  const action = button.getAttribute("data-sheet-action");
  if (action === "refresh") return refreshSheet(), true;
  if (action === "new-session") return sendRpc({ type: "new_session" }), true;
  if (action === "compact") return sendRpc({ type: "compact" }), true;
  if (action === "stats") return sendRpc({ type: "get_session_stats" }), true;
  if (action === "models") return openSheet("models"), true;
  if (action === "thinking") return openSheet("thinking"), true;
  if (action === "commands") return openSheet("commands"), true;
  if (action === "sessions") return openSheet("sessions"), true;
  if (action === "devices") {
    void refreshDevices()
      .then(() => openSheet("devices"))
      .catch((error) => showToast(error instanceof Error ? error.message : "Failed to load devices", "error"));
    return true;
  }
  if (action === "create-device") {
    const input = document.querySelector("#device-name-input");
    const name = input instanceof HTMLInputElement ? input.value.trim() : "";
    if (!name) {
      showToast("Enter a device name first.", "error");
      return true;
    }
    void createDeviceToken(name)
      .then(() => {
        renderSheet();
        showToast("Device token created. Copy it now — it will not be shown again.", "success");
      })
      .catch((error) => showToast(error instanceof Error ? error.message : "Failed to create device", "error"));
    return true;
  }
  if (action === "revoke-device") {
    const deviceId = button.getAttribute("data-device-id");
    if (!deviceId) return true;
    if (state.pendingDeviceRevoke !== deviceId) {
      state.pendingDeviceRevoke = deviceId;
      renderSheet();
      showToast("Click Confirm revoke to finish.", "info");
      return true;
    }
    state.pendingDeviceRevoke = null;
    void revokeDevice(deviceId)
      .then(() => renderSheet())
      .catch((error) => showToast(error instanceof Error ? error.message : "Failed to revoke device", "error"));
    return true;
  }
  if (action === "logout") {
    logout();
    return true;
  }
  if (action === "copy-device-token") {
    const token = state.lastDeviceToken || "";
    if (token && navigator.clipboard) {
      void navigator.clipboard.writeText(token).then(
        () => showToast("Device token copied.", "success"),
        () => showToast("Copy failed. Select the token manually.", "error"),
      );
    }
    return true;
  }
  if (action === "new-parent-session") return handleNewParentSession(), true;
  if (action === "new-parallel-session") return handleSpawnActiveSession(), true;
  if (action === "tree") return openSheet("tree"), true;
  if (action === "cd") return handleInsertOnlyCommand("cd"), true;
  if (action === "catalog-more") {
    if (state.socket?.readyState === WebSocket.OPEN && state.catalogNextCursor) {
      state.catalogAppending = true;
      state.socket.send(JSON.stringify({
        kind: "sessions/list",
        cursor: state.catalogNextCursor,
        limit: 50,
      }));
    }
    return true;
  }
  if (action === "more-sessions") {
    state.sessionListLimit = Math.min((state.sessionListLimit || 12) + 25, 500);
    renderSheet();
    return true;
  }
  if (action === "show-subagents") {
    state.showSubagents = true;
    sendRpc({ type: "phone_list_sessions", includeSubagents: true });
    return true;
  }
  if (action === "refresh-sessions") {
    sendRpc({ type: "phone_list_sessions", includeSubagents: Boolean(state.showSubagents) });
    return true;
  }

  const thinkingLevel = button.getAttribute("data-thinking-level");
  if (thinkingLevel) return sendRpc({ type: "set_thinking_level", level: thinkingLevel }), true;

  const modelProvider = button.getAttribute("data-model-provider");
  const modelId = button.getAttribute("data-model-id");
  if (modelProvider && modelId) return sendRpc({ type: "set_model", provider: modelProvider, modelId }), true;

  const runLocalCommand = button.getAttribute("data-run-local-command");
  if (runLocalCommand) return handleRunLocalCommand(runLocalCommand), true;

  const runCommand = button.getAttribute("data-run-command");
  if (runCommand) return handleInsertRunCommand(runCommand), true;

  const activeSessionId = button.getAttribute("data-active-session-id");
  if (activeSessionId) return handleSelectActiveSession(activeSessionId), true;

  const sessionPath = button.getAttribute("data-session-path");
  if (sessionPath) return sendRpc({ type: "switch_session", sessionPath }), true;

  const openBranchEntry = button.getAttribute("data-open-branch-entry");
  if (openBranchEntry) return sendRpc({ type: "phone_open_branch_path", entryId: openBranchEntry }), true;

  const forkEntry = button.getAttribute("data-fork-entry");
  if (forkEntry) return sendRpc({ type: "fork", entryId: forkEntry }), true;

  return false;
}

function refreshSheet() {
  refreshAll();
}

function handleNewParentSession() {
  if (!prepareParentSessionNew()) return;
  closeSheet();
}

function handleSpawnActiveSession() {
  if (!prepareSessionSpawn()) return;
  closeSheet();
}

function handleRunLocalCommand(runLocalCommand) {
  if (handleInsertOnlyLocalCommand(runLocalCommand)) {
    closeSheet();
    return;
  }

  const result = tryHandleLocalCommand(`/${runLocalCommand}`, { hasAttachments: state.attachments.length > 0 });
  if (result === "handled") {
    el.promptInput.value = "";
    autoResizeTextarea();
    renderCommandSuggestions();
  }
}

/** Pre-fill an insert-only command (e.g. /cd) into the composer. */
function handleInsertOnlyCommand(commandName) {
  handleInsertOnlyLocalCommand(commandName);
  closeSheet();
  el.promptInput.focus();
}

function handleInsertRunCommand(runCommand) {
  const commandName = runCommand.replace(/^\//, "").trim();
  if (commandName) {
    insertSlashCommand(commandName);
  } else {
    el.promptInput.value = `${runCommand} `;
    autoResizeTextarea();
    renderCommandSuggestions();
    el.promptInput.focus();
  }
  closeSheet();
}

function handleSelectActiveSession(activeSessionId) {
  if (!prepareSessionSelection(activeSessionId)) return showToast("Not connected to Pi.", "error");
  closeSheet();
}
