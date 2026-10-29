import { contentToText } from "./formatters.js";
import {
  carryThinkingExpansionTo,
  clearSnapshotView,
  clearTransientState,
  handleAssistantEvent,
  prependEarlierMessages,
  renderMessages,
  renderWidgets,
  setHistoryWindow,
  transformMessage,
  upsertLiveTool,
} from "./messages.js";
import { closeSheet } from "./sheet-navigation.js";
import { renderSheet } from "./sheets-view.js";
import { state } from "./state.js";
import { refreshAll, refreshQuota, sendRpc } from "./transport.js";
import {
  autoResizeTextarea,
  clearUiModal,
  openTokenModal,
  openUiModalForRequest,
  renderHeader,
  resetToken,
  showBanner,
  showToast,
} from "./ui.js";
import { renderCommandSuggestions } from "./autocomplete-controller.js";
import { renderAutocompleteItems } from "./autocomplete.js";

export function handleAuthFailure() {
  resetToken({ clearInput: true });
  state.socket = null;
  renderHeader();
  openTokenModal();
  showBanner("Access token required. Enter the current /phone-start token.", "error");
}

function sendUiResponse(payload) {
  sendRpc({ type: "extension_ui_response", ...payload });
  clearUiModal();
}

function handleExtensionUiRequest(request) {
  if (request.method === "notify") {
    showToast(request.message || "Notification");
    return;
  }

  if (request.method === "setStatus") {
    state.footerStatus = request.statusText || "";
    renderWidgets();
    return;
  }

  if (request.method === "setWidget") {
    if (request.widgetLines?.length) state.widgets.set(request.widgetKey || "widget", request.widgetLines);
    else state.widgets.delete(request.widgetKey || "widget");
    renderWidgets();
    return;
  }

  if (request.method === "setTitle") {
    document.title = request.title || "Pi Phone";
    return;
  }

  if (request.method === "set_editor_text") {
    const promptInput = document.querySelector("#prompt-input");
    if (promptInput) promptInput.value = request.text || "";
    autoResizeTextarea();
    renderCommandSuggestions();
    return;
  }

  if (!["select", "confirm", "input", "editor"].includes(request.method)) {
    showToast(`Unsupported UI request: ${request.method || "unknown"}`);
    return;
  }

  openUiModalForRequest(request, sendUiResponse);
}

function handleRpcPayload(payload) {
  if (!payload) return;

  if (payload.type === "response") {
    if (!payload.success) {
      if (payload.command === "path_suggestions") {
        renderAutocompleteItems([]);
        return;
      }
      showToast(payload.error || `Command failed: ${payload.command}`, "error");
      return;
    }

    if (payload.command === "get_state") {
      state.snapshotState = payload.data || state.snapshotState;
      renderHeader();
      void refreshQuota();
      return;
    }

    if (payload.command === "get_messages") {
      state.messages = (payload.data?.messages || []).flatMap(transformMessage);
      clearTransientState();
      renderMessages();
      return;
    }

    if (payload.command === "get_commands") {
      state.commands = payload.data?.commands || [];
      renderCommandSuggestions();
      renderSheet();
      return;
    }

    if (payload.command === "path_suggestions") {
      const context = state.autocompleteContext;
      if (!context || context.type !== "path") return;
      if (Number(payload.data?.requestId) !== state.autocompleteRemoteRequestId) return;
      if (payload.data?.mode !== context.mode) return;
      if ((payload.data?.query || "") !== context.query) return;

      const suggestions = Array.isArray(payload.data?.suggestions) ? payload.data.suggestions : [];
      renderAutocompleteItems(suggestions.map((suggestion) => ({
        kind: "path",
        label: context.mode === "mention" ? `@${suggestion.value}` : suggestion.value,
        badge: suggestion.kind === "previous" ? "recent" : suggestion.isDirectory ? "dir" : "file",
        description: suggestion.description || suggestion.value,
        value: suggestion.value,
        isDirectory: Boolean(suggestion.isDirectory),
        title: suggestion.description || suggestion.value,
      })));
      return;
    }

    if (payload.command === "cd") {
      showToast(`Changed directory to ${payload.data?.cwd || "the selected path"}.`);
      refreshAll();
      return;
    }

    if (payload.command === "get_available_models") {
      state.models = payload.data?.models || [];
      renderSheet();
      return;
    }

    if (payload.command === "get_session_stats") {
      state.stats = payload.data || null;
      renderSheet();
      return;
    }

    if (payload.command === "phone_list_sessions") {
      state.sessions = payload.data?.sessions || [];
      state.subagentCount = Number(payload.data?.subagentCount || 0);
      state.sessionsLoading = false;
      renderSheet();
      return;
    }

    if (payload.command === "phone_get_tree") {
      state.tree = payload.data || null;
      renderSheet();
      return;
    }

    if (payload.command === "new_parent_session") {
      clearTransientState();
      refreshAll();
      showToast("Started a new parent session.");
      return;
    }

    if (payload.command === "new_session") {
      clearTransientState();
      refreshAll();
      showToast("Started a new Pi session.");
      return;
    }

    if (payload.command === "compact") {
      showToast("Compaction triggered.");
      refreshAll();
      return;
    }

    if (payload.command === "slash_command") {
      if (payload.data?.source === "extension") {
        refreshAll({ forceQuota: true });
      }
      return;
    }

    if (payload.command === "reload") {
      clearTransientState();
      showToast("Reloaded extensions, skills, prompts, and themes.");
      refreshAll({ forceQuota: true });
      return;
    }

    if (payload.command === "set_model") {
      showToast("Model updated.");
      refreshAll();
      return;
    }

    if (payload.command === "set_thinking_level") {
      showToast("Thinking level updated.");
      refreshAll();
      return;
    }

    if (payload.command === "switch_session") {
      showToast("Session switched.");
      refreshAll();
      closeSheet();
      return;
    }

    if (payload.command === "fork") {
      showToast("Fork created.");
      refreshAll();
      closeSheet();
      return;
    }

    if (payload.command === "phone_open_branch_path") {
      showToast("Opened selected branch path as a new session.");
      refreshAll();
      closeSheet();
      return;
    }

    return;
  }

  if (payload.type === "agent_start") {
    state.status = { ...(state.status || {}), isStreaming: true };
    // Latch the working indicator for the WHOLE turn: snapshots that land
    // between tool calls carry no live items and used to blink it off while
    // the next call was still coming. It stays on until agent_end clears it.
    state.workingUntilSettled = true;
    renderHeader();
    renderMessages();
    return;
  }

  if (payload.type === "agent_end") {
    state.status = { ...(state.status || {}), isStreaming: false };
    // Turn fully settled (pi will not auto-continue): only now may the
    // spinner go away.
    state.workingUntilSettled = false;
    renderHeader();
    renderMessages();
    refreshAll({ forceQuota: true });
    return;
  }

  if (payload.type === "message_update") {
    // The server slims this envelope to role + timestamp: the deltas carry the
    // content, and the timestamp is all that is needed to key the live item
    // identically to the message it will become once committed.
    handleAssistantEvent(payload.assistantMessageEvent, payload.message);
    return;
  }

  if (payload.type === "message_end" && payload.message?.role === "assistant") {
    const transformed = transformMessage(payload.message, Date.now())[0];
    if (transformed) {
      // Keep the live item's id: `transformMessage` keys on the same timestamp,
      // and reusing it means the thinking section keeps its state here rather
      // than being treated as a brand new section.
      const previousId = state.liveAssistant?.id;
      carryThinkingExpansionTo(previousId, transformed.id);
      state.liveAssistant = { ...transformed, id: previousId || transformed.id, live: false };
      renderMessages();
    }
    return;
  }

  if (payload.type === "tool_execution_start") {
    upsertLiveTool(payload.toolCallId, {
      id: `tool-live-${payload.toolCallId}`,
      key: `tool-${payload.toolCallId}`,
      kind: "tool",
      toolCallId: payload.toolCallId,
      toolName: payload.toolName || "tool",
      args: payload.args || {},
      command: payload.args?.command || "",
      live: true,
      title: payload.toolName || "tool",
      text: JSON.stringify(payload.args || {}, null, 2),
      meta: "Running…",
      status: "running",
      rawContent: null,
    });
    return;
  }

  if (payload.type === "tool_execution_update") {
    upsertLiveTool(payload.toolCallId, {
      ...(state.liveTools.get(payload.toolCallId) || {}),
      id: `tool-live-${payload.toolCallId}`,
      key: `tool-${payload.toolCallId}`,
      kind: "tool",
      toolCallId: payload.toolCallId,
      toolName: payload.toolName || "tool",
      args: payload.args || state.liveTools.get(payload.toolCallId)?.args || {},
      command: payload.args?.command || state.liveTools.get(payload.toolCallId)?.command || "",
      live: true,
      title: payload.toolName || "tool",
      text: contentToText(payload.partialResult?.content) || JSON.stringify(payload.args || {}, null, 2),
      meta: "Running…",
      status: "running",
      details: payload.partialResult?.details,
      rawContent: payload.partialResult?.content || state.liveTools.get(payload.toolCallId)?.rawContent || null,
    });
    return;
  }

  if (payload.type === "tool_execution_end") {
    upsertLiveTool(payload.toolCallId, {
      ...(state.liveTools.get(payload.toolCallId) || {}),
      id: `tool-live-${payload.toolCallId}`,
      key: `tool-${payload.toolCallId}`,
      kind: "tool",
      toolCallId: payload.toolCallId,
      toolName: payload.toolName || "tool",
      args: payload.args || state.liveTools.get(payload.toolCallId)?.args || {},
      command: payload.args?.command || state.liveTools.get(payload.toolCallId)?.command || "",
      live: false,
      title: payload.toolName || "tool",
      text: contentToText(payload.result?.content),
      meta: payload.isError ? "Failed" : "Done",
      status: payload.isError ? "error" : "done",
      // Measured server-side between tool_execution_start/end.
      durationMs: Number.isFinite(Number(payload.durationMs)) ? Number(payload.durationMs) : null,
      details: payload.result?.details,
      rawContent: payload.result?.content || state.liveTools.get(payload.toolCallId)?.rawContent || null,
    });
    return;
  }

  if (payload.type === "extension_ui_request") {
    handleExtensionUiRequest(payload);
    return;
  }

  if (payload.type === "compaction_start") {
    // Parallel sessions reach the phone through the rpc channel and already
    // delivered these; ignoring them left the compacting indicator stale.
    state.snapshotState = { ...(state.snapshotState || {}), isCompacting: true };
    renderHeader();
    showBanner("Compacting conversation…");
    return;
  }

  if (payload.type === "compaction_end") {
    state.snapshotState = { ...(state.snapshotState || {}), isCompacting: false };
    renderHeader();
    if (payload.errorMessage) {
      showBanner(`Compaction failed: ${payload.errorMessage}`, "error");
    } else if (payload.aborted) {
      showToast("Compaction cancelled.");
    } else {
      showBanner("");
      // The transcript was rewritten, so pull the compacted view.
      refreshAll();
    }
    return;
  }

  if (payload.type === "queue_update") {
    // Two senders, two semantics:
    //  - Parallel sessions: the child rpc forwards pi's OWN queue — fully
    //    authoritative, replace the chip list wholesale.
    //  - Parent sessions: the mirror only sees phone-queued messages (pi's
    //    internal queue is invisible to extensions). Two modes therefore:
    //    "replace" on send/cancel (the mirror is exactly right at that
    //    moment), and "delivery" on turn end/delivery — REMOVE chips whose
    //    mirror entry disappeared (delivered), keep everything else,
    //    including CLI-queued chips the mirror cannot see.
    const steering = Array.isArray(payload.steering) ? payload.steering : [];
    const followUp = Array.isArray(payload.followUp) ? payload.followUp : [];
    const toChip = (text, kind, i) => ({
      id: `piq-${kind}-${i}-${hashText(String(text || ""))}`,
      text: String(text || ""),
      imageCount: 0,
      kind,
      timestamp: Date.now(),
    });
    const nextPending = [
      ...steering.map((text, i) => toChip(text, "steer", i)),
      ...followUp.map((text, i) => toChip(text, "followUp", i)),
    ];

    let merged;
    if (payload.mode !== "delivery") {
      merged = nextPending;
    } else {
      const norm = (s) => String(s || "").replace(/\s+/g, " ").trim();
      const keepKeys = new Set(nextPending.map((c) => `${c.kind}:${norm(c.text)}`));
      merged = (state.pendingMessages || []).filter((chip) => keepKeys.has(`${chip.kind}:${norm(chip.text)}`));
    }

    const changed = JSON.stringify(merged.map((p) => p.id)) !== JSON.stringify((state.pendingMessages || []).map((p) => p.id));
    state.pendingMessages = merged;
    if (changed) renderMessages();
    return;
  }

  if (payload.type === "auto_retry_start") {
    showBanner(`Retrying after error: ${payload.errorMessage || "temporary failure"}`);
    return;
  }

  if (payload.type === "auto_retry_end") {
    showBanner(payload.success ? "" : `Retry failed: ${payload.finalError || "Unknown error"}`, payload.success ? "info" : "error");
  }
}

/**
 * Short content hash for queue chips: same text always gets the same id, so a
 * chip survives re-renders and the cancel request can find its entry in pi's
 * queue even after reconnects.
 */
function hashText(text) {
  let h = 5381;
  for (let i = 0; i < text.length; i += 1) {
    h = ((h << 5) + h + text.charCodeAt(i)) >>> 0;
  }
  return h.toString(36);
}

export async function handleEnvelope(event) {
  if (event.channel === "sessions" && event.event === "catalog") {
    const nextActiveSessionId = event.data?.activeSessionId || state.activeSessionId;
    const activeSessionChanged = nextActiveSessionId !== state.activeSessionId;

    state.activeSessions = event.data?.sessions || [];
    state.activeSessionId = nextActiveSessionId;

    if (activeSessionChanged && state.snapshotWorkerId && state.snapshotWorkerId !== state.activeSessionId) {
      clearSnapshotView();
      renderMessages();
    }

    renderHeader();
    renderSheet();
    return;
  }

  if (event.channel === "snapshot") {
    if (event.sessionWorkerId && state.activeSessionId && event.sessionWorkerId !== state.activeSessionId) {
      return;
    }

    // Paged-in older history: prepend rather than replace.
    if (event.event === "earlier-messages") {
      state.loadingEarlier = false;
      const olderItems = (event.messages || []).flatMap((message, index) =>
        transformMessage(message, `earlier-${event.startIndex ?? 0}-${index}`),
      );
      setHistoryWindow({
        truncated: event.hasMore !== false,
        total: event.totalMessageCount,
        loaded: state.messages.length + olderItems.length,
      });
      prependEarlierMessages(olderItems);
      return;
    }

    state.snapshotState = event.state || null;
    state.snapshotWorkerId = event.sessionWorkerId || state.activeSessionId || null;
    state.status = { ...(state.status || {}), isStreaming: Boolean(event.state?.isStreaming) };
    // Re-latch the spinner from server truth: after a reconnect or refresh the
    // latch may be stale, and a mid-turn snapshot must keep it spinning.
    state.workingUntilSettled = Boolean(event.state?.isStreaming) || state.workingUntilSettled && Boolean(state.status?.isStreaming);
    state.messages = (event.messages || []).flatMap(transformMessage);
    setHistoryWindow({
      truncated: Boolean(event.historyTruncated),
      total: Number.isFinite(event.totalMessageCount) ? event.totalMessageCount : state.messages.length,
      loaded: state.messages.length,
    });
    // Bandwidth: the server omits `commands` when the catalogue is unchanged
    // since the last snapshot; an explicit empty array still clears it.
    if (Array.isArray(event.commands)) state.commands = event.commands;
    clearTransientState();

    if (event.liveAssistantMessage?.role === "assistant") {
      const assistant = transformMessage(event.liveAssistantMessage, Date.now())[0];
      if (assistant) {
        state.liveAssistant = { ...assistant, id: "assistant-live", live: true };
      }
    }

    for (const tool of event.liveTools || []) {
      const text =
        contentToText(tool.result?.content)
        || contentToText(tool.partialResult?.content)
        || JSON.stringify(tool.args || {}, null, 2);

      state.liveTools.set(tool.toolCallId, {
        id: `tool-live-${tool.toolCallId}`,
        // Matches the committed toolResult's key so the disclosure state (and
        // the grouping) stays stable across a snapshot.
        key: `tool-${tool.toolCallId}`,
        kind: "tool",
        toolCallId: tool.toolCallId,
        toolName: tool.toolName || "tool",
        args: tool.args || {},
        command: tool.args?.command || "",
        live: !tool.result,
        title: tool.toolName || "tool",
        text,
        meta: tool.result ? (tool.isError ? "Failed" : "Done") : "Running…",
        // The snapshot carries the server-measured duration for finished tools.
        durationMs: Number.isFinite(Number(tool.durationMs)) ? Number(tool.durationMs) : null,
        details: tool.result?.details || tool.partialResult?.details,
        status: tool.isError ? "error" : (tool.result ? "done" : "running"),
        rawContent: tool.result?.content || tool.partialResult?.content || null,
      });
    }

    renderHeader();
    renderMessages();
    renderSheet();
    renderCommandSuggestions();
    void refreshQuota();
    return;
  }

  if (event.channel === "server") {
    if (event.event === "status") {
      state.status = event.data;
      // Queued steer / follow-up messages ride along with status so the
      // composer can show and cancel them.
      const nextPending = Array.isArray(event.data?.pendingMessages) ? event.data.pendingMessages : [];
      const changed = JSON.stringify(nextPending.map((p) => p.id)) !== JSON.stringify((state.pendingMessages || []).map((p) => p.id));
      state.pendingMessages = nextPending;
      renderHeader();
      if (changed) renderMessages();
      return;
    }
    if (event.event === "queue-update") {
      // PARENT-session queue push. The parent's mirror only sees phone-queued
      // messages; pi's real queue (invisible to extensions) may also hold
      // CLI-queued entries. Mode field decides semantics:
      //  - "replace" (send/cancel from the phone): mirror is exactly right →
      //    replace wholesale.
      //  - "delivery" (turn end / a message landed): REMOVE chips whose
      //    mirror entry is gone, keep everything else — CLI-queued chips
      //    survive, delivered ones drop in the same frame as the transcript.
      const steering = Array.isArray(event.data?.steering) ? event.data.steering : [];
      const followUp = Array.isArray(event.data?.followUp) ? event.data.followUp : [];
      const toChip = (text, kind, i) => ({
        id: `piq-${kind}-${i}-${hashText(text)}`,
        text: String(text || ""),
        imageCount: 0,
        kind,
        timestamp: Date.now(),
      });
      const incoming = [
        ...steering.map((text, i) => toChip(text, "steer", i)),
        ...followUp.map((text, i) => toChip(text, "followUp", i)),
      ];
      const norm = (s) => String(s || "").replace(/\s+/g, " ").trim();

      let merged;
      if (event.data?.mode === "delivery") {
        const keepKeys = new Set(incoming.map((c) => `${c.kind}:${norm(c.text)}`));
        merged = (state.pendingMessages || []).filter((chip) => keepKeys.has(`${chip.kind}:${norm(chip.text)}`));
      } else {
        merged = incoming;
      }

      const changed = JSON.stringify(merged.map((p) => p.id)) !== JSON.stringify((state.pendingMessages || []).map((p) => p.id));
      state.pendingMessages = merged;
      if (changed) renderMessages();
      return;
    }
    if (event.event === "stderr") {
      showBanner(event.data?.text?.trim() || "", "error");
      return;
    }
    if (event.event === "reloading") {
      showBanner(event.data?.message || "");
      return;
    }
    if (event.event === "session-spawned") {
      showToast(event.data?.message || "Opened new parallel session.");
      return;
    }
    if (event.event === "single-client-replaced") {
      showBanner(event.data?.message || "This phone session was replaced by another client.", "error");
      return;
    }
    if (event.event === "idle-timeout") {
      showBanner(event.data?.message || "Pi Phone stopped because it was idle.", "error");
      return;
    }
    if (["startup-error", "snapshot-error", "client-error"].includes(event.event)) {
      showToast(event.data?.message || "Server error", "error");
      return;
    }
    if (event.event === "agent-exit") {
      showBanner(event.data?.message || "Pi rpc exited.", "error");
      return;
    }
  }

  if (event.channel === "rpc") {
    handleRpcPayload(event.payload);
  }
}
