import { addAttachments, clearAttachments, removeAttachment, renderAttachmentStrip, syncAttachmentsWithPrompt } from "./attachments.js";
import { renderCommandSuggestions } from "./autocomplete-controller.js";
import { applyAutocompleteItem, submitPrompt } from "./commands.js";
import { el, state } from "./state.js";
import { handleSheetButtonAction, sheetButtonActionKey } from "./sheet-actions.js";
import { closeSheet, openSheet } from "./sheet-navigation.js";
import { renderSheet } from "./sheets-view.js";
import { connectSocket, refreshAll, requestEarlierMessages, sendRpc } from "./transport.js";
import { detailStateKey } from "./messages.js";
import {
  autoResizeTextarea,
  closeTokenModal,
  isNearBottom,
  scheduleComposerLayoutSync,
  scrollMessagesToBottom,
  setFollowLatest,
  showToast,
  storeToken,
} from "./ui.js";

export function initializeBindings({ handleEnvelope, handleAuthFailure }) {
  el.promptInput.addEventListener("input", () => {
    syncAttachmentsWithPrompt();
    autoResizeTextarea();
    renderCommandSuggestions();
  });

  el.promptInput.addEventListener("click", () => {
    renderCommandSuggestions();
  });

  el.promptInput.addEventListener("keyup", (event) => {
    if (["ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown", "Home", "End"].includes(event.key)) {
      renderCommandSuggestions();
    }
  });

  // Enter submits; Shift+Enter (or the mobile keyboard's newline key) inserts a newline.
  // `isComposing` guards against IME candidate selection (Chinese/Japanese input).
  el.promptInput.addEventListener("compositionstart", () => {
    state.isComposing = true;
  });
  el.promptInput.addEventListener("compositionend", () => {
    state.isComposing = false;
  });
  el.promptInput.addEventListener("keydown", (event) => {
    if (event.key !== "Enter") return;
    if (state.isComposing || event.isComposing || event.keyCode === 229) return;
    if (event.shiftKey || event.ctrlKey || event.metaKey || event.altKey) return;

    event.preventDefault();

    const pending = state.autocompleteItems;
    if (pending.length && state.autocompleteContext) {
      applyAutocompleteItem(pending[0]);
      return;
    }

    submitPrompt();
  });

  autoResizeTextarea();
  renderCommandSuggestions();
  renderAttachmentStrip();
  scheduleComposerLayoutSync();

  if ("ResizeObserver" in window && el.composerWrap) {
    const composerResizeObserver = new ResizeObserver(() => scheduleComposerLayoutSync());
    composerResizeObserver.observe(el.composerWrap);
  }

  window.addEventListener("resize", scheduleComposerLayoutSync, { passive: true });
  window.visualViewport?.addEventListener("resize", scheduleComposerLayoutSync, { passive: true });

  const noteUserScrollIntent = () => {
    state.lastUserScrollIntentAt = Date.now();
  };

  const syncFollowLatestOnScroll = () => {
    const now = Date.now();
    const fromUser = now - state.lastUserScrollIntentAt < 400;
    if (!fromUser && now < state.ignoreScrollTrackingUntil) return;
    setFollowLatest(isNearBottom());
  };

  window.addEventListener("wheel", noteUserScrollIntent, { passive: true });
  window.addEventListener("touchmove", noteUserScrollIntent, { passive: true });
  window.addEventListener("scroll", syncFollowLatestOnScroll, { passive: true });

  el.refreshButton.addEventListener("click", refreshAll);
  el.abortButton.addEventListener("click", () => sendRpc({ type: "abort" }));
  el.stopButton?.addEventListener("click", () => sendRpc({ type: "abort" }));
  el.jumpToLatestButton?.addEventListener("click", () => {
    setFollowLatest(true);
    scrollMessagesToBottom({ force: true, behavior: "smooth" });
  });
  el.actionsButton.addEventListener("click", () => openSheet("actions"));
  el.loadEarlierButton?.addEventListener("click", requestEarlierMessages);
  // Commands moved into the ⋯ sheet; the composer button no longer exists.
  el.insertCommandButton?.addEventListener("click", () => openSheet("commands"));
  el.sessionSidebarButton.addEventListener("click", () => openSheet("active-sessions"));
  el.steerButton.addEventListener("click", () => submitPrompt({ steer: true }));
  el.sendButton.addEventListener("click", () => submitPrompt());
  el.sheetCloseButton.addEventListener("click", closeSheet);
  el.sheetSavedSessionsButton?.addEventListener("click", () => openSheet("sessions"));
  el.attachImageButton.addEventListener("click", () => el.imageInput.click());
  el.imageInput.addEventListener("change", (event) => {
    addAttachments(event.target.files);
    renderCommandSuggestions();
    el.imageInput.value = "";
  });

  el.attachmentStrip.addEventListener("click", (event) => {
    const button = event.target.closest("[data-remove-attachment]");
    if (!button) return;
    removeAttachment(button.getAttribute("data-remove-attachment"));
    renderCommandSuggestions();
  });

  // Cancelling a queued steer / follow-up message. The text rides along so
  // parallel sessions can find and drop the exact entry in pi's real queue.
  el.pendingStrip.addEventListener("click", (event) => {
    const button = event.target.closest("[data-cancel-pending]");
    if (!button) return;
    const id = button.getAttribute("data-cancel-pending");
    if (!id) return;
    const text = button.getAttribute("data-cancel-text") || "";
    button.disabled = true;
    sendRpc({ type: "phone_cancel_pending_message", id, text });
  });

  el.commandStrip.addEventListener("click", (event) => {
    const button = event.target.closest("[data-autocomplete-index]");
    if (!button) return;

    const index = Number(button.getAttribute("data-autocomplete-index"));
    if (!Number.isFinite(index) || index < 0) return;
    applyAutocompleteItem(state.autocompleteItems[index]);
  });

  el.sheetContent.addEventListener("change", (event) => {
    if (!(event.target instanceof HTMLSelectElement)) return;
    if (!event.target.hasAttribute("data-command-category-select")) return;
    state.commandSheetCategory = event.target.value;
    renderSheet();
  });

  // Session search. Delegated because the sheet content is re-rendered on every
  // update, which would drop a directly-bound listener.
  el.sheetContent.addEventListener("input", (event) => {
    const target = event.target;
    if (!(target instanceof HTMLInputElement) || target.id !== "session-search") return;
    state.sessionQuery = target.value;
    state.sessionListLimit = 12;
    renderSheet();
    // renderSheet replaces the input, so restore focus and caret position.
    const next = document.querySelector("#session-search");
    if (next instanceof HTMLInputElement) {
      next.focus();
      const end = next.value.length;
      try { next.setSelectionRange(end, end); } catch { /* type=search may not support it */ }
    }
  });

  el.sheetContent.addEventListener("pointerdown", (event) => {
    const button = event.target.closest("button");
    if (!button) return;

    const sheetAction = button.getAttribute("data-sheet-action") || "";
    const shouldHandleEarly = button.hasAttribute("data-active-session-id") || ["new-parent-session", "new-parallel-session"].includes(sheetAction);
    if (!shouldHandleEarly) return;

    event.preventDefault();
    const actionKey = sheetButtonActionKey(button);
    state.lastSheetPointerAction = actionKey;
    state.lastSheetPointerActionAt = Date.now();
    handleSheetButtonAction(button);
  });

  el.sheetContent.addEventListener("click", (event) => {
    const button = event.target.closest("button");
    if (!button) return;

    const actionKey = sheetButtonActionKey(button);
    if (state.lastSheetPointerAction === actionKey && Date.now() - state.lastSheetPointerActionAt < 800) {
      state.lastSheetPointerAction = "";
      state.lastSheetPointerActionAt = 0;
      return;
    }

    handleSheetButtonAction(button);
  });

  el.tokenSaveButton.addEventListener("click", () => {
    const nextToken = el.tokenInput.value.trim();
    if (!nextToken) {
      showToast("Enter the current /phone-start token.", "error");
      el.tokenInput.focus();
      return;
    }

    state.token = nextToken;
    storeToken(state.token);
    closeTokenModal();
    connectSocket({ handleEnvelope, handleAuthFailure });
  });

  el.tokenInput.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      el.tokenSaveButton.click();
    }
  });

  document.addEventListener("toggle", (event) => {
    const details = event.target;
    if (!(details instanceof HTMLDetailsElement)) return;

    const itemId = details.getAttribute("data-tool-panel");
    if (itemId) {
      state.toolPanelOpen.set(itemId, details.open);
      return;
    }

    // Message-level sections (Thinking / Tool calls / Details).
    const messageId = details.getAttribute("data-detail-message");
    const title = details.getAttribute("data-detail-title");
    if (!messageId || !title) return;
    const key = detailStateKey(messageId, title);
    state.detailOpen.set(key, details.open);
    // Not marked as a user action here on purpose: re-rendering the transcript
    // also fires toggle events, and recording those would freeze the auto
    // thinking behaviour. syncDetailStateFromDom() detects real toggles by
    // comparing the DOM against the state it last wrote.
  }, true);

  window.addEventListener("beforeunload", () => {
    state.manuallyClosed = true;
    if (state.socket) state.socket.close();
    clearAttachments();
  });
}
