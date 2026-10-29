import { el, state } from "./state.js";
import {
  assistantParts,
  contentToText,
  countImages,
  escapeAttribute,
  escapeHtml,
  formatDuration,
  formatTimestamp,
  stripTerminalControlSequences,
} from "./formatters.js";
import { renderMarkdownLite } from "./markdown.js";
import { getToolPath, isToolPanelOpen, rememberToolPanelState, renderRichToolContent, renderToolBadge, summarizeCommand, truncateLabel } from "./tool-rendering.js";
import { scrollMessagesToBottom, showToast, updateJumpToLatestButton } from "./ui.js";

const INLINE_USER_CUSTOM_TYPES = new Set(["phone-inline-user-message"]);

function userContentDisplayText(content) {
  const imageCount = countImages(content);
  if (!Array.isArray(content)) return contentToText(content);

  const text = content
    .filter((part) => part?.type === "text")
    .map((part) => part.text || "")
    .join("")
    .trim();

  if (text) return text;
  if (imageCount === 1) return "[1 image attached]";
  if (imageCount > 1) return `[${imageCount} images attached]`;
  return contentToText(content);
}

function imageSource(part) {
  if (!part || part.type !== "image") return "";
  if (typeof part.previewUrl === "string" && part.previewUrl) return part.previewUrl;
  if (typeof part.url === "string" && part.url) return part.url;
  if (typeof part.data === "string" && part.data && typeof part.mimeType === "string" && part.mimeType) {
    return `data:${escapeAttribute(part.mimeType)};base64,${escapeAttribute(part.data)}`;
  }
  return "";
}

function renderUserContent(content, fallbackText = "") {
  if (!Array.isArray(content)) {
    return {
      html: renderMarkdownLite(typeof content === "string" ? content : fallbackText),
      renderedImages: 0,
    };
  }

  const blocks = [];
  let imageIndex = 0;

  for (const part of content) {
    if (part?.type === "text") {
      const text = String(part.text || "");
      // Empty text parts are dropped, but a text part still counts as content:
      // image-only messages must not fall through to the fallback below, or
      // the attached images would disappear from the transcript.
      blocks.push(`<div class="user-message-text-block">${renderMarkdownLite(text)}</div>`);
      continue;
    }

    if (part?.type !== "image") continue;
    const src = imageSource(part);
    if (!src) continue;
    imageIndex += 1;
    const alt = part.name || `Attached image ${imageIndex}`;
    blocks.push(`
      <div class="user-message-image-wrap">
        <img class="user-message-image" src="${src}" alt="${escapeAttribute(alt)}" loading="lazy" />
      </div>
    `);
  }

  if (!blocks.length) {
    return {
      html: renderMarkdownLite(fallbackText),
      renderedImages: 0,
    };
  }

  return {
    html: `<div class="user-message-inline-content">${blocks.join("")}</div>`,
    renderedImages: imageIndex,
  };
}

export function transformMessage(message, index) {
  if (!message || typeof message !== "object") return [];

  if (message.role === "user") {
    return [{
      id: `user-${message.timestamp || index}`,
      kind: "user",
      meta: formatTimestamp(message.timestamp),
      text: userContentDisplayText(message.content),
      rawContent: message.content,
      imageCount: countImages(message.content),
    }];
  }

  if (message.role === "assistant") {
    const parts = assistantParts(message.content);
    return [{
      id: `assistant-${message.timestamp || index}`,
      // `message.timestamp` is preserved through the live -> committed swap, so
      // the thinking section keeps its expanded state when the answer starts.
      key: `assistant-${message.timestamp || index}`,
      kind: "assistant",
      meta: [message.model, formatTimestamp(message.timestamp)].filter(Boolean).join(" · "),
      text: parts.text,
      thinking: parts.thinking,
      toolCalls: parts.toolCalls,
      details: message.usage || message.stopReason ? {
        usage: message.usage,
        stopReason: message.stopReason,
      } : undefined,
    }];
  }

  if (message.role === "toolResult") {
    return [{
      id: `tool-${message.toolCallId || message.timestamp || index}`,
      // Stable disclosure key: the tool-call id is the one identifier that is
      // identical for the live tool and for its committed transcript entry.
      key: `tool-${message.toolCallId || message.timestamp || index}`,
      kind: "tool",
      toolCallId: message.toolCallId,
      toolName: message.toolName || "tool",
      title: message.toolName || "tool",
      status: message.isError ? "error" : "done",
      text: contentToText(message.content),
      rawContent: message.content,
      meta: formatTimestamp(message.timestamp),
      details: message.details,
      durationMs: Number.isFinite(message.durationMs) ? message.durationMs : null,
    }];
  }

  if (message.role === "bashExecution") {
    return [{
      id: `bash-${message.timestamp || index}`,
      key: `bash-${message.timestamp || index}`,
      kind: "tool",
      toolName: "bash",
      title: `bash · ${message.command || ""}`,
      command: message.command || "",
      args: { command: message.command || "" },
      status: message.cancelled ? "cancelled" : "done",
      text: message.output || "",
      meta: formatTimestamp(message.timestamp),
      details: {
        exitCode: message.exitCode,
        truncated: message.truncated,
        fullOutputPath: message.fullOutputPath,
      },
    }];
  }

  if (message.role === "custom") {
    if (message.display === false) return [];

    if (INLINE_USER_CUSTOM_TYPES.has(message.customType || "")) {
      return [{
        id: `custom-user-${message.timestamp || index}`,
        kind: "user",
        meta: formatTimestamp(message.timestamp),
        text: userContentDisplayText(message.content),
        rawContent: message.content,
        imageCount: countImages(message.content),
      }];
    }

    return [{
      id: `custom-${message.timestamp || index}`,
      kind: "custom",
      title: message.customType || "extension",
      text: contentToText(message.content),
      meta: formatTimestamp(message.timestamp),
      details: message.details,
      imageCount: countImages(message.content),
    }];
  }

  if (message.role === "branchSummary") {
    return [{
      id: `branch-summary-${message.timestamp || index}`,
      kind: "summary",
      title: "Branch summary",
      text: message.summary || "",
      meta: formatTimestamp(message.timestamp),
      details: { fromId: message.fromId },
    }];
  }

  if (message.role === "compactionSummary") {
    return [{
      id: `compaction-summary-${message.timestamp || index}`,
      kind: "summary",
      title: `Compaction summary${message.tokensBefore ? ` · ${message.tokensBefore.toLocaleString()} tokens` : ""}`,
      text: message.summary || "",
      meta: formatTimestamp(message.timestamp),
    }];
  }

  return [];
}

function renderMessageMeta(item, options = {}) {
  const pills = [];
  if (item.imageCount && !options.suppressImageCount) {
    pills.push(`<span class="inline-pill">${item.imageCount} image${item.imageCount === 1 ? "" : "s"}</span>`);
  }
  if (item.status) pills.push(`<span class="inline-pill">${escapeHtml(item.status)}</span>`);
  return pills.join("");
}

/*
 * Message-level disclosure state.
 *
 * Every render rebuilds the transcript HTML, so an open/closed <details> would
 * snap back to its default unless we remember it. Tool panels use
 * `data-tool-panel`; these are the same idea for the "Thinking" section.
 *
 * The key is written as TWO attributes rather than one concatenated string:
 * HTML attribute parsing normalises certain control characters (a NUL becomes
 * U+FFFD), so a single delimiter-packed key would not round-trip and the
 * lookup would miss. Ids and titles are plain text, so they survive intact.
 */
/**
 * Upper bound on remembered disclosure state.
 *
 * These maps are keyed per message / tool id, so a long session would otherwise
 * accumulate an entry for every panel ever rendered and never release them. The
 * oldest keys are dropped once the cap is reached; the only consequence is that
 * a very old panel returns to its default (collapsed) state.
 */
const DISCLOSURE_STATE_LIMIT = 500;

function rememberBounded(map, key, value) {
  map.set(key, value);
  if (map.size <= DISCLOSURE_STATE_LIMIT) return;
  // Map preserves insertion order, so the first key is the oldest.
  const oldest = map.keys().next();
  if (!oldest.done) map.delete(oldest.value);
}

export function detailStateKey(messageId, title) {
  return `${messageId}\u241f${title}`;
}

function isDetailOpen(messageId, title, defaultOpen = false) {
  const key = detailStateKey(messageId, title);
  // An explicit user toggle always wins, so the auto behaviour never fights
  // someone who opened or closed the section themselves.
  if (state.detailOpen.has(key)) return Boolean(state.detailOpen.get(key));
  return defaultOpen;
}

/**
 * Auto-open the live message's Thinking section while reasoning streams, then
 * collapse it once the answer starts.
 *
 * Keyed by message so the collapse is remembered: without recording it, the
 * next render would fall back to the default and re-open it.
 *
 * Once the user opens the section by hand we stop managing it entirely: the
 * auto-collapse fires on the first answer delta, which is exactly when a
 * reader starts scrolling the reasoning — fighting them here made the section
 * snap shut every few hundred milliseconds while streaming.
 */
function syncThinkingDisclosure(item) {
  const key = detailStateKey(item.id, "Thinking");

  // A manual toggle always wins: once the user opens or closes the section we
  // stop managing it.
  if (state.thinkingUserToggled?.has(key)) return;
  // The user opened this section and is reading it: stand down until they
  // collapse it themselves, no matter how many answer deltas arrive.
  if (userOpenedThinking(item)) return;

  if (item.live) {
    // While reasoning streams and no answer text exists yet, keep it open;
    // as soon as the answer starts, collapse it.
    rememberBounded(state.detailOpen, key, Boolean(item.thinking && !item.text));
    return;
  }

  // The message is finished. If the user never touched it, leave it collapsed
  // rather than storing an "open" state that would persist forever.
  if (state.detailOpen.get(key) === true) state.detailOpen.set(key, false);
}

function renderDetailSection(title, value, options = {}) {
  if (!value) return "";

  const body = options.markdown
    ? `<div class="detail-content detail-markdown">${renderMarkdownLite(value)}</div>`
    : `<pre class="detail-pre">${escapeHtml(value)}</pre>`;

  const messageId = options.messageId || "";
  const keyAttrs = messageId
    ? ` data-detail-message="${escapeAttribute(messageId)}" data-detail-title="${escapeAttribute(title)}"`
    : "";
  const open = messageId ? isDetailOpen(messageId, title, options.defaultOpen) : Boolean(options.defaultOpen);

  return `
    <details${keyAttrs}${open ? " open" : ""}>
      <summary>${escapeHtml(title)}</summary>
      ${body}
    </details>
  `;
}

/**
 * Secondary sections for an assistant message.
 *
 * Only "Thinking" is shown. A raw "Details" section dumped the message's usage
 * and stop-reason JSON, which is diagnostic noise rather than something worth
 * reading on a phone; "Tool calls" was redundant with the tool panels rendered
 * directly below the message.
 */
function renderAssistantDetails(item) {
  const messageId = item.id;
  const sections = [];
  if (item.thinking) {
    syncThinkingDisclosure(item);
    sections.push(renderDetailSection("Thinking", item.thinking, {
      markdown: true,
      messageId,
      // Live reasoning starts expanded; the sync above records the collapse.
      defaultOpen: Boolean(item.live),
    }));
  }
  return sections.join("");
}

/**
 * True when the user has manually opened this message's Thinking section.
 *
 * Read AFTER the DOM sync but BEFORE the next auto-write, so the auto-collapse
 * can stand down for messages the user is reading. Lives outside the render
 * path state (detailOpen / thinkingUserToggled) because the user's open intent
 * may land between renders, where the toggle-based detection cannot see it.
 */
function userOpenedThinking(item) {
  if (!item?.id) return false;
  if (state.thinkingUserToggled?.has(detailStateKey(item.id, "Thinking"))) return true;
  const liveItem = item.live ? state.liveAssistant : null;
  const expandedId = state.userExpandedThinkingId;
  if (!expandedId) return false;
  return expandedId === item.id || (liveItem && expandedId === liveItem.id);
}

/**
 * Queued (not yet delivered) messages.
 *
 * When the phone sends a steer or follow-up while the agent is running, the
 * message is queued but pi has not consumed it yet. Previously the UI showed
 * nothing at all, so tapping Steer looked like it did nothing and there was no
 * way to take it back. These chips make the queue visible and cancellable.
 */
function renderPendingMessages() {
  const pending = Array.isArray(state.pendingMessages) ? state.pendingMessages : [];
  if (!pending.length) {
    el.pendingStrip.classList.add("hidden");
    el.pendingStrip.innerHTML = "";
    return;
  }

  el.pendingStrip.innerHTML = pending.map((entry) => {
    const label = entry.kind === "steer" ? "steer" : "follow-up";
    const images = entry.imageCount > 0 ? `${entry.imageCount} image${entry.imageCount === 1 ? "" : "s"}` : "";
    // Image-only queued messages have no text; showing "(no text)" looked like
    // the message was lost, so the image count carries the description instead.
    const text = entry.text || "";
    const parts = [text, images].filter(Boolean).join(" · ");
    return `
      <div class="pending-chip">
        <span class="pending-chip-kind">${escapeHtml(label)}</span>
        <span class="pending-chip-text">${escapeHtml(parts || "(empty)")}</span>
        <button
          class="pending-chip-cancel"
          data-cancel-pending="${escapeHtml(entry.id)}"
          data-cancel-text="${escapeAttribute(text)}"
          aria-label="Cancel queued message"
          title="Cancel queued message"
        >×</button>
      </div>
    `;
  }).join("");
  el.pendingStrip.classList.remove("hidden");
}

/**
 * Collapse consecutive tool results into a single group.
 *
 * A busy turn emits one tool item per call, so reading a file, grepping, then
 * editing produced a wall of near-identical rows that pushed the actual reply
 * off screen. Grouping them keeps the transcript scannable: consecutive tool
 * items become one row showing the tool names and a count, expandable to the
 * individual calls.
 *
 * Only *consecutive* tools merge, so an assistant reply between two tool runs
 * still separates them into distinct groups.
 */
/**
 * The most useful one-line target for a tool: a file path, else a command.
 *
 * Tool *results* keep their arguments under `args` (linked from the matching
 * tool call), while live tools set a top-level `command`. Checking only one of
 * those made grouped bash calls render as a bare count instead of the command.
 */
function toolTargetOf(tool) {
  const path = getToolPath(tool);
  if (path) return { kind: "path", value: path };
  const command = tool.command || tool.args?.command || "";
  if (command) return { kind: "command", value: String(command) };
  return { kind: "none", value: "" };
}

function groupConsecutiveTools(items) {
  /**
   * Invisible assistant entries (toolCall blocks with no text/thinking) are
   * pure session plumbing and must never split a run — otherwise every call
   * renders as its own TOOL/PI/TOOL/PI sandwich, which is exactly why calls
   * never grouped on the phone.
   */
  const isInvisible = (item) =>
    item &&
    item.kind === "assistant" &&
    !String(item.text || "").trim() &&
    !String(item.thinking || "").trim();

  /**
   * Items that always start a new group and always stay visible as-is.
   */
  /**
   * Does another tool call follow this index before the next hard break?
   *
   * Used for the live-streaming tail: narration deltas land BEFORE the next
   * tool call exists, so a naive rule would split the group at every sentence
   * and re-open a new group for the next call. When a tool DOES follow, the
   * live entry is narration mid-run and joins the group instead of breaking
   * it.
   */
  const toolFollows = (from) => {
    for (let i = from + 1; i < items.length; i += 1) {
      const item = items[i];
      if (item.kind === "tool") return true;
      if (item.kind === "assistant") continue;
      return false;
    }
    return false;
  };

  const isHardBreak = (item) =>
    item.kind === "user" ||
    item.kind === "summary" ||
    item.kind === "custom" ||
    (item.kind === "assistant" && item.live === true && !toolFollows(items.indexOf(item)));

  /**
   * Indices of the FINAL answer per turn: the last VISIBLE assistant item of
   * a turn, when at least one tool ran in that turn. That entry is the reply
   * the user is waiting for and must stay in the body — only narration
   * BEFORE the last call joins the group. The live streaming tail is never
   * the final answer (it is still being written).
   */
  const finalAnswerIndices = new Set();
  {
    let turnStart = -1;
    for (let i = 0; i <= items.length; i += 1) {
      const item = items[i];
      const endsTurn = !item || isHardBreak(item);
      if (!endsTurn) {
        if (turnStart === -1) turnStart = i;
        continue;
      }
      if (turnStart !== -1) {
        const turnHasTool = items
          .slice(turnStart, i)
          .some((entry) => entry.kind === "tool");
        if (turnHasTool) {
          for (let j = i - 1; j >= turnStart; j -= 1) {
            const candidate = items[j];
            if (candidate.kind !== "assistant" || isInvisible(candidate)) continue;
            // A live entry with a tool after it is mid-run narration, not the
            // answer (case: narration deltas streamed, next call started).
            if (candidate.live === true && toolFollows(j)) continue;
            finalAnswerIndices.add(j);
            break;
          }
        }
      }
      turnStart = item && !isHardBreak(item) ? turnStart : -1;
      if (item && !isHardBreak(item)) turnStart = i;
    }
  }

  const out = [];
  let run = [];
  let runNotes = [];

  const continuesRun = (previous, item) => {
    if (!previous || previous.kind !== "tool" || item.kind !== "tool") return false;
    // A duplicate pair (same call committed + still live) starts its own run:
    // the committed entry wins for display, and the duplicate is dropped by
    // the caller before grouping.
    if (previous.toolCallId && item.toolCallId && previous.toolCallId === item.toolCallId) return false;
    return true;
  };

  const flush = () => {
    if (!run.length) {
      out.push(...runNotes);
      runNotes = [];
      return;
    }
    if (run.length === 1) {
      // Single call: prose stays prose, nothing is folded into a group. The
      // absorbed narration must be released too, not dropped.
      out.push(...runNotes, run[0]);
    } else {
      // Keyed by the first tool in the run, and deliberately NOT prefixed with
      // the item kind: a run that starts as a single tool and grows into a
      // group must keep the same disclosure key, or the panel collapses the
      // instant the second call arrives.
      const { key: groupKey } = run[0];
      out.push({
        id: `toolgroup-${groupKey || run[0].id}`,
        panelKey: groupKey || run[0].id,
        kind: "toolgroup",
        tools: run.slice(),
        notes: runNotes.slice(),
        live: run.some((tool) => tool.live),
        errorCount: run.reduce((n, tool) => n + (tool.status === "error" ? 1 : 0), 0),
      });
    }
    run = [];
    runNotes = [];
  };

  /**
   * TURN-SCOPED grouping.
   *
   * One assistant turn = one group: every tool call from the first to the
   * last joins the same group, and narration between calls is carried as
   * group notes. This is deliberate over a stricter "only while another call
   * follows" rule: during STREAMING a narration delta lands BEFORE the next
   * tool call exists, so a lookahead-based rule would split the group at
   * every sentence and re-open a new group for the next call — the exact
   * "merges keep breaking apart" churn. With a turn boundary the group's
   * first tool never changes, so the panel key (and its open state) stays
   * stable no matter how the deltas arrive.
   *
   * The turn's final answer (last visible assistant entry, after the last
   * call) is exempted above and renders as normal body text.
   */
  for (let i = 0; i < items.length; i += 1) {
    const item = items[i];
    if (isInvisible(item)) continue;
    if (finalAnswerIndices.has(i) || isHardBreak(item)) {
      flush();
      out.push(item);
      continue;
    }
    if (item.kind === "tool") {
      const previous = run[run.length - 1] || null;
      if (!continuesRun(previous, item)) flush();
      run.push(item);
      continue;
    }
    if (item.kind === "assistant" && run.length) {
      // Narration between calls: carried inside the group as notes.
      runNotes.push(item);
      continue;
    }
    flush();
    out.push(item);
  }
  flush();

  return out;
}

/** Distinct tool names in a group, in first-seen order. */
function groupToolNames(group) {
  const seen = [];
  for (const tool of group.tools) {
    const name = String(tool.toolName || tool.title || "tool").trim().split(" \u00b7 ")[0].toLowerCase();
    if (name && !seen.includes(name)) seen.push(name);
  }
  return seen;
}

/**
 * Body for a tool inside an expanded group.
 *
 * The group row above already names the tool and shows its target, so a nested
 * panel's own summary line just repeats it. Shell calls therefore show only
 * their output; other tools keep their normal rich rendering with the summary
 * stripped.
 */
function renderGroupedToolBody(tool) {
  const rich = renderRichToolContent(tool);
  if (rich) {
    // The group row above already names the tool and its target, so a nested
    // collapsible panel would just repeat it. Unwrap the panel entirely and
    // keep only its body: leaving a <details> without a <summary> makes the
    // browser draw its own "Details" label.
    const body = rich.match(/<div class="tool-panel-body">([\s\S]*)<\/div>\s*<\/details>\s*$/);
    return body ? body[1] : rich;
  }
  const text = String(tool.text || "").slice(0, 4000);
  if (!text.trim()) return "";
  return `<pre class="tool-terminal-block">${escapeHtml(text)}</pre>`;
}

/**
 * One-line digest for a live tail: tool name, target, status badge. Rendered
 * under the group/single summary while the call runs — like appending a
 * single tool row, never the full output block.
 */
function livetailDigest(tool) {
  const label = String(tool.toolName || tool.title || "tool").trim().split(" · ")[0];
  const targetInfo = toolTargetOf(tool);
  const target = targetInfo.kind === "command"
    ? (() => {
        const c = summarizeCommand(targetInfo.value, 64);
        return `<span class="cmd-program">${escapeHtml(c.program)}</span>${c.rest ? " " + escapeHtml(c.rest) : ""}`;
      })()
    : targetInfo.value
      ? escapeHtml(truncateLabel(targetInfo.value, 80))
      : "";
  const badge = tool.status === "error"
    ? renderToolBadge("failed", "removed")
    : renderToolBadge("running", "accent");
  return `
    <div class="livetail-digest">
      <span class="livetail-spinner" aria-hidden="true"></span>
      <span class="livetail-name">${escapeHtml(label)}</span>
      ${target ? `<span class="livetail-target mono">${target}</span>` : ""}
      ${badge}
    </div>
  `;
}

/**
 * One-line rendering for a standalone tool call.
 *
 * Previously a tool produced a two-line block: a "TOOL <time>" header and then
 * a "SHELL COMMAND <cmd>" row. That wasted vertical space and made a single
 * call look different from a group. Now it is one row — tool name, target,
 * duration — with the detail available on expand.
 */
function renderSingleTool(item) {
  const targetInfo = toolTargetOf(item);
  const label = String(item.toolName || item.title || "tool").trim().split(" · ")[0].toUpperCase();
  const isOpen = isToolPanelOpen(item.panelKey || item.key || item.id, false);

  const target = targetInfo.kind === "command"
    ? (() => {
        const c = summarizeCommand(targetInfo.value, 60);
        return `<span class="cmd-program">${escapeHtml(c.program)}</span>${c.rest ? " " + escapeHtml(c.rest) : ""}`;
      })()
    : targetInfo.value
      ? escapeHtml(truncateLabel(targetInfo.value, 68))
      : "";

  const statusBadge = item.status === "error"
    ? renderToolBadge("failed", "removed")
    : item.status === "running"
      ? renderToolBadge("running", "accent")
      : item.status === "done"
        ? renderToolBadge("done", "added")
        : "";
  const ms = Number(item.durationMs);
  const duration = Number.isFinite(ms) && ms > 0 ? renderToolBadge(formatDuration(ms), "neutral") : "";

  /**
   * Live tail BELOW the summary row: a compact digest line (tool + target +
   * status + duration) for the running call — not the full output block.
   * Same visual as a single-tool row, so the running step reads as one line.
   */
  const liveTail = item.live || item.status === "running"
    ? `<div class="tool-group-livetail" data-livetail-key="${escapeHtml(item.key || item.id || "")}">${livetailDigest(item)}</div>`
    : "";

  return `
    <div class="toolgroup-wrap">
      <details class="tool-panel tool-panel-single" data-tool-panel="${escapeHtml(item.panelKey || item.key || item.id)}" ${isOpen ? "open" : ""}>
        <summary class="tool-panel-summary">
          <div class="tool-panel-summary-copy">
            <div class="tool-panel-eyebrow">${escapeHtml(label)}</div>
            <div class="tool-panel-path mono">${target}</div>
          </div>
          <div class="tool-panel-badges">${statusBadge}${duration}</div>
        </summary>
        <div class="tool-panel-body">
          ${renderGroupedToolBody(item)}
        </div>
      </details>
      ${liveTail}
    </div>
  `;
}

function renderToolGroup(group) {
  const count = group.tools.length;
  const names = groupToolNames(group);
  const isOpen = isToolPanelOpen(group.panelKey || group.id, false);

  // Collapsed view shows what the group is doing NOW: the LAST call's target,
  // with the program emphasized, plus that call's own duration. The right-side
  // badges still carry the whole-group rollup (count / failed / total time).
  const targets = group.tools.map((tool) => toolTargetOf(tool)).filter((t) => t.value);
  const lastTarget = targets[targets.length - 1];
  const earlierCount = Math.max(0, targets.length - 1);
  const lastTool = group.tools[group.tools.length - 1];
  const lastMs = Number(lastTool?.durationMs);
  const lastDuration = Number.isFinite(lastMs) && lastMs > 0 ? formatDuration(lastMs) : "";
  const digestHtml = lastTarget
    ? (lastTarget.kind === "command"
        ? (() => {
            const c = summarizeCommand(lastTarget.value, 48);
            return `<span class="cmd-program">${escapeHtml(c.program)}</span>${c.rest ? " " + escapeHtml(c.rest) : ""}`;
          })()
        : escapeHtml(truncateLabel(lastTarget.value, 60)))
      + (earlierCount > 0 ? `<span class="cmd-more">  +${earlierCount}</span>` : "")
    : "";
  // When the tail call is still running, its live badge (below) says so; the
  // collapsed title never hides an in-flight call behind an old duration.
  const running = group.tools.some((tool) => tool.status === "running" || tool.live);

  const badges = [`${count} call${count === 1 ? "" : "s"}`];
  if (group.errorCount) badges.push(`${group.errorCount} failed`);
  if (running) badges.push("running");
  // Total elapsed for the group, so a long batch is visible when collapsed.
  const totalMs = group.tools.reduce((sum, tool) => sum + (Number(tool.durationMs) || 0), 0);
  if (totalMs > 0) badges.push(formatDuration(totalMs));

  /**
   * Narration absorbed between calls. Rendered as REAL body text (not hidden
   * in a collapsed pane) so the run still reads like prose with the tool
   * block appended underneath. The live streaming note stays visible.
   */
  const noteRows = (group.notes || []).map((note) => `
      <article class="message assistant group-note">${renderMarkdownLite(note.text || note.thinking || "")}</article>
    `).join("");

  const rows = group.tools.map((tool) => {
    const label = String(tool.toolName || tool.title || "tool").trim().split(" \u00b7 ")[0];
    // Commands get the same program/args split as the dedicated bash panel so
    // a grouped run stays readable instead of showing a wall of flags.
    const targetInfo = toolTargetOf(tool);
    const target = targetInfo.kind === "command" ? summarizeCommand(targetInfo.value, 64) : null;
    const targetText = targetInfo.kind === "path" ? truncateLabel(targetInfo.value, 80) : "";
    const badge = tool.status === "error"
      ? renderToolBadge("failed", "removed")
      : tool.status === "running"
        ? renderToolBadge("running", "accent")
        : renderToolBadge("done", "added");
    const ms = Number(tool.durationMs);
    const duration = Number.isFinite(ms) && ms > 0 ? renderToolBadge(formatDuration(ms), "neutral") : "";

    // Each child row toggles its own output. Collapsed by default: opening a
    // group should show the list of calls, not every byte of their output.
    const childKey = `child-${tool.key || tool.id}`;
    const childOpen = isToolPanelOpen(childKey, false);

    return `
      <details class="tool-group-item" data-tool-panel="${escapeHtml(childKey)}" ${childOpen ? "open" : ""}>
        <summary class="tool-group-row">
          <span class="tool-group-chevron" aria-hidden="true"></span>
          <span class="tool-group-name">${escapeHtml(label)}</span>
          ${target
            ? `<span class="tool-group-target mono"><span class="cmd-program">${escapeHtml(target.program)}</span>${target.rest ? " " + escapeHtml(target.rest) : ""}</span>`
            : targetText ? `<span class="tool-group-target mono">${escapeHtml(targetText)}</span>` : ""}
          ${badge}
          ${duration}
        </summary>
        <div class="tool-group-detail">${renderGroupedToolBody(tool)}</div>
      </details>
    `;
  }).join("");

  const summaryText = names.join(", ") || "tools";

  /**
   * Live tail: the LAST call's output, rendered OUTSIDE the collapsed
   * details so it is always visible — even when the group is folded. This is
   * what "what is Pi doing right now" means: the current call's full detail
   * (command + streaming output) sitting directly under the group header,
   * exactly like the CLI transcript. Only shown while that call is running;
   * finished calls collapse back into the group.
   */
  /**
   * Live tail: a compact digest line for the group's LAST call while the
   * turn is ongoing — running or just-finished — positioned BELOW the
   * collapsible group. When call N finishes it keeps showing N's digest;
   * when call N+1 starts, the same line swaps to N+1. Disappears once the
   * whole turn settles.
   */
  const liveTail = lastTool && (group.live || lastTool.live || lastTool.status === "running")
    ? `<div class="tool-group-livetail" data-livetail-key="${escapeHtml(lastTool.key || lastTool.id || "")}">${livetailDigest(lastTool)}</div>`
    : "";

  // Narration first (real prose), then the collapsible tool block. The group
  // renders as one compact unit: text, calls, text, calls — without ever
  // splitting the run into separate TOOL/PI bubbles.
  return `
    <div class="toolgroup-wrap">
      ${noteRows}
      <details class="tool-group" data-tool-panel="${escapeHtml(group.panelKey || group.id)}" ${isOpen ? "open" : ""}>
        <summary class="tool-group-head">
          <span class="tool-group-chevron" aria-hidden="true"></span>
          <span class="tool-group-icon" aria-hidden="true">
            <svg viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round">
              <path d="M2 4h12M2 8h8M2 12h10" />
            </svg>
          </span>
          <span class="tool-group-head-main">
            <span class="tool-group-head-title">${escapeHtml(summaryText)}</span>
            <span class="tool-group-head-sub mono">${digestHtml || escapeHtml(`${count} call${count === 1 ? "" : "s"}`)}${!digestHtml && lastDuration ? ` · ${escapeHtml(lastDuration)}` : ""}</span>
          </span>
          <span class="tool-group-head-badges">${badges.map((b) => renderToolBadge(b, "neutral")).join("")}</span>
        </summary>
        <div class="tool-group-body">
          ${rows}
        </div>
      </details>
      ${liveTail}
    </div>
  `;
}

function renderMessage(item, options = {}) {
  if (item.kind === "toolgroup") return renderToolGroup(item);
  if (item.kind === "tool") return renderSingleTool(item);
  const richTool = "";
  const roleLabel = {
    assistant: "Pi",
    custom: item.title || "Extension",
    summary: item.title || "Summary",
    system: "System",
    tool: richTool ? "Tool" : item.title || "Tool",
    user: "You",
  }[item.kind] || "Message";

  const renderedUser = item.kind === "user"
    ? renderUserContent(item.rawContent, item.text || "")
    : { html: "", renderedImages: 0 };

  const bodyMain = richTool || (item.kind === "tool"
    ? `<pre>${escapeHtml(item.text || "")}</pre>`
    : item.kind === "user"
      ? renderedUser.html
      : renderMarkdownLite(item.text || ""));

  // A body that renders to nothing (empty user text, assistant turn with only
  // tool calls) must not leave an empty padded shell in the transcript.
  const bodyHasContent = Boolean(String(bodyMain).trim());

  // Only assistant messages carry a secondary section (Thinking). Tool results
  // and summaries no longer render a raw "Details" JSON dump.
  const extraDetails = item.kind === "assistant" ? renderAssistantDetails(item) : "";

  // Assistant output shows the "Pi / model · time" line only on its FIRST
  // message after a user turn. Repeating it before every assistant message was
  // noise; showing it once delimits the reply from what came before.
  const showHeader = item.kind !== "user" && (item.kind !== "assistant" || options.firstOfTurn === true);
  const header = showHeader
    ? `<div class="message-header">
        <div class="role-badge">${escapeHtml(roleLabel)}${item.live ? " · live" : ""}</div>
        <div class="meta">${escapeHtml(item.meta || "")}</div>
      </div>`
    : "";

  return `
    <article class="message ${item.kind}">
      ${header}
      ${bodyHasContent ? `
      <div class="message-body">
        ${bodyMain}
        ${richTool ? "" : renderMessageMeta(item, { suppressImageCount: renderedUser.renderedImages > 0 })}
        ${extraDetails}
      </div>
      ` : ""}
    </article>
  `;
}

function enrichToolItems(items) {
  const toolCalls = new Map();

  for (const item of items) {
    if (item.kind !== "assistant" || !Array.isArray(item.toolCalls)) continue;
    for (const toolCall of item.toolCalls) {
      if (!toolCall?.id) continue;
      toolCalls.set(toolCall.id, toolCall);
    }
  }

  const enriched = items.map((item) => {
    if (item.kind !== "tool" || item.args || !item.toolCallId) return item;
    const linked = toolCalls.get(item.toolCallId);
    if (!linked) return item;
    return { ...item, args: linked.arguments || {} };
  });

  // Assistant entries whose only content is toolCall blocks are pure session
  // plumbing: pi writes assistant(toolCall) -> toolResult for every single
  // call, so keeping the empty shells (a) split every consecutive tool run
  // into TOOL/PI/TOOL/PI — calls never grouped — and (b) rendered as blank
  // bubbles. Their call metadata has already been merged onto the matching
  // tool items above, so they are dropped here for good.
  return enriched.filter((item) => {
    if (item.kind !== "assistant") return true;
    if (String(item.text || "").trim()) return true;
    if (String(item.thinking || "").trim()) return true;
    // No text, no thinking: only keep it when it contributes nothing that the
    // tool items need — i.e. it never had tool calls at all (a truly empty
    // message), or this is the live streaming item still waiting for content.
    return false;
  });
}

function currentItems() {
  const items = [...state.messages];

  // A snapshot carries both the committed toolResult and the still-remembered
  // live tool for the same call. Rendering both duplicated the call and made
  // the surrounding group split and rejoin on every push, which is what made
  // an open panel flicker shut. The committed entry wins: its key is stable.
  const committedToolCallIds = new Set();
  for (const item of items) {
    if (item.kind === "tool" && item.toolCallId) committedToolCallIds.add(item.toolCallId);
  }

  for (const tool of state.liveTools.values()) {
    if (tool?.toolCallId && committedToolCallIds.has(tool.toolCallId)) continue;
    items.push(tool);
  }
  if (state.liveAssistant) items.push(state.liveAssistant);
  return groupConsecutiveTools(enrichToolItems(items));
}

function hasLiveItems() {
  if (state.liveAssistant?.live) return true;
  for (const tool of state.liveTools.values()) {
    if (tool?.live) return true;
  }
  return false;
}

export function clearTransientState() {
  state.liveAssistant = null;
  state.liveTools.clear();
  state.userExpandedThinkingId = null;
}

let streamRenderQueued = false;

/**
 * Coalesce streaming re-renders to one per animation frame.
 *
 * Each text/thinking delta and tool update used to rebuild the entire
 * transcript (innerHTML over up to 60 messages, tool groups included). At
 * token rates that is dozens of full DOM rebuilds per second on a phone —
 * visible jank and battery burn. Batching to a single frame keeps the stream
 * smooth and cuts client CPU sharply.
 */
export function scheduleStreamRender() {
  if (streamRenderQueued) return;
  streamRenderQueued = true;
  const flush = () => {
    streamRenderQueued = false;
    renderMessages();
  };
  if (typeof requestAnimationFrame === "function") requestAnimationFrame(flush);
  else setTimeout(flush, 16);
}

// When the lazy markdown parser arrives, repaint the transcript once: early
// frames rendered with the escaped-text fallback, and this upgrades them to
// real markdown (code fences, lists, tables…) without any user action.
document.addEventListener("pi-phone:markdown-ready", () => scheduleStreamRender());

/**
 * The live assistant item is about to become its committed transcript entry.
 * The ids are identical by design, so the "user is reading this Thinking
 * section" marker carries over instead of being dropped at the swap.
 */
export function carryThinkingExpansionTo(previousId, nextId) {
  if (!state.userExpandedThinkingId) return;
  if (previousId && state.userExpandedThinkingId === previousId) {
    state.userExpandedThinkingId = nextId || previousId;
  }
}

export function clearSnapshotView() {
  state.snapshotState = null;
  state.snapshotWorkerId = null;
  state.messages = [];
  // Windowing bookkeeping resets with the transcript.
  state.historyTruncated = false;
  state.totalMessageCount = 0;
  state.loadedMessageCount = 0;
  state.loadingEarlier = false;
  clearTransientState();
}

export function handleAssistantEvent(event, providerMessage) {
  if (!event) return;
  if (!state.liveAssistant) {
    // Key the live item by the provider message's timestamp when it is known:
    // that is exactly what `transformMessage` uses once the message is
    // committed, so the id survives the live -> committed swap and the thinking
    // section keeps its expanded state instead of snapping shut when the answer
    // begins. The fallback keeps successive turns from sharing one detail key.
    const stableId = providerMessage?.timestamp
      ? `assistant-${providerMessage.timestamp}`
      : `assistant-live-${Date.now()}`;
    state.liveAssistant = {
      id: stableId,
      key: stableId,
      kind: "assistant",
      live: true,
      text: "",
      thinking: "",
      toolCalls: [],
      meta: "Streaming…",
    };
  }

  if (event.type === "text_delta") state.liveAssistant.text += stripTerminalControlSequences(event.delta || "");
  if (event.type === "thinking_delta") state.liveAssistant.thinking += stripTerminalControlSequences(event.delta || "");
  if (event.type === "toolcall_end" && event.toolCall) {
    state.liveAssistant.toolCalls.push({ id: event.toolCall.id || "", name: event.toolCall.name || "tool", arguments: event.toolCall.arguments || {} });
  }
  if (event.type === "error") showToast(event.message || "Agent error", "error");
  scheduleStreamRender();
}

export function upsertLiveTool(toolId, value) {
  state.liveTools.set(toolId, value);
  scheduleStreamRender();
}

export function renderLoadEarlier() {
  if (!el.loadEarlierButton) return;
  const hasItems = state.messages.length > 0;
  const show = hasItems && (state.historyTruncated || state.loadedMessageCount < state.totalMessageCount) && !state.loadingEarlier;
  el.loadEarlierButton.classList.toggle("hidden", !show);
  if (show) {
    const remaining = Math.max(0, state.totalMessageCount - state.loadedMessageCount);
    el.loadEarlierButton.textContent = remaining > 0
      ? `Load earlier messages (${remaining} older)`
      : "Load earlier messages";
    el.loadEarlierButton.disabled = false;
  }
  if (state.loadingEarlier && !el.loadEarlierButton.classList.contains("hidden")) {
    el.loadEarlierButton.textContent = "Loading…";
    el.loadEarlierButton.disabled = true;
  }
}

/**
 * Prepend a page of older messages, preserving scroll position so the view
 * does not jump when history is inserted above the fold.
 */
export function prependEarlierMessages(olderItems) {
  if (!Array.isArray(olderItems) || olderItems.length === 0) return;

  const before = el.messages.getBoundingClientRect().height;
  const scrollY = window.scrollY;

  state.messages = [...olderItems, ...state.messages];
  state.loadedMessageCount = state.messages.length;
  renderMessages({ forceScroll: false });

  // Restore the reading position: keep the message the user was looking at
  // in place by offsetting by however much the list grew.
  const after = el.messages.getBoundingClientRect().height;
  const grew = after - before;
  if (grew > 0) window.scrollTo({ top: scrollY + grew, behavior: "auto" });
}

export function setHistoryWindow({ truncated, total, loaded }) {
  if (typeof truncated === "boolean") state.historyTruncated = truncated;
  if (Number.isFinite(total)) state.totalMessageCount = total;
  if (Number.isFinite(loaded)) state.loadedMessageCount = loaded;
  renderLoadEarlier();
}

/**
 * Read open/closed state straight out of the live DOM before we replace it.
 *
 * The `toggle` listener alone is not enough: toggle events are dispatched
 * asynchronously, so a re-render that lands in the same task as the tap would
 * still see stale state and collapse the panel the user just opened. Reading
 * the DOM synchronously at render time removes that race entirely.
 */
/**
 * Read open/closed state out of the live DOM before we replace it.
 *
 * Toggle events are dispatched asynchronously, so a re-render landing in the
 * same task as a tap would otherwise see stale state and close what the user
 * just opened. Reading the DOM synchronously removes that race.
 *
 * Only sections the user actually toggled are read back. The auto thinking
 * behaviour writes its own value to the same map (open while reasoning, closed
 * once the answer starts); copying every node back would overwrite that with
 * the previous render's state and the auto-collapse would never take effect.
 */
function syncDetailStateFromDom() {
  for (const details of el.messages.querySelectorAll("details[data-detail-message]")) {
    const messageId = details.getAttribute("data-detail-message");
    const title = details.getAttribute("data-detail-title");
    if (!messageId || !title) continue;
    const key = detailStateKey(messageId, title);

    // A section the user opened between renders (their tap landed after the
    // last DOM sync but before this one) must register as user intent even if
    // it matches a value the auto behaviour wrote earlier — otherwise the
    // streaming auto-collapse would close it right back.
    if (title === "Thinking" && details.open && state.detailOpen.get(key) !== true) {
      state.userExpandedThinkingId = messageId;
    }
    // The user collapsed the section themselves: reading is over, so the
    // auto behaviour may manage the message again.
    if (title === "Thinking" && !details.open && state.userExpandedThinkingId === messageId) {
      state.userExpandedThinkingId = null;
    }

    // Distinguish a real user toggle from the DOM churn of re-rendering.
    // Replacing innerHTML makes the browser fire `toggle` events for the nodes
    // that changed, so trusting every event would permanently mark sections as
    // "user managed" and the auto thinking behaviour could never collapse them.
    // A genuine toggle is one where the DOM disagrees with what we last wrote.
    const known = state.detailOpen.get(key);
    if (known === undefined || known === details.open) continue;

    rememberBounded(state.detailOpen, key, details.open);
    state.thinkingUserToggled.add(key);
  }
  for (const panel of el.messages.querySelectorAll("details[data-tool-panel]")) {
    const key = panel.getAttribute("data-tool-panel");
    if (key) rememberToolPanelState(key, panel.open);
  }
}

export function renderMessages({ forceScroll = false, streaming = hasLiveItems() } = {}) {
  const items = currentItems();
  if (!items.length) {
    el.messages.innerHTML = `
      <article class="message system">
        <div class="message-header"><div class="role-badge">Ready</div></div>
        <div class="message-body">
          <p>Send a message to Pi, or use Commands to browse skills, prompts, and extension commands.</p>
        </div>
      </article>
    `;
    updateJumpToLatestButton();
    renderLoadEarlier();
    renderWorkingIndicator();
    return;
  }

  renderPendingMessages();
  syncDetailStateFromDom();
  // Flag the first assistant message in each turn so only that one gets the
  // role/model header.
  let seenUserSinceAssistant = true;
  const decorated = items.map((item) => {
    if (item.kind === "user") {
      seenUserSinceAssistant = true;
      return item;
    }
    if (item.kind !== "assistant") return item;
    const firstOfTurn = seenUserSinceAssistant;
    seenUserSinceAssistant = false;
    return { ...item, firstOfTurn };
  });

  el.messages.innerHTML = decorated.map((item) => renderMessage(item, { firstOfTurn: item.firstOfTurn })).join("");
  updateJumpToLatestButton();
  renderLoadEarlier();
  renderWorkingIndicator();
  scrollMessagesToBottom({ force: forceScroll, streaming, behavior: "smooth" });
}

/**
 * "Working" spinner pinned to the bottom of the transcript.
 *
 * Shown exactly while a response is streaming or tools are running; hidden
 * the moment the agent settles. Lives OUTSIDE the #messages innerHTML rebuild
 * so it never flickers or resets across the per-frame re-renders.
 */
function renderWorkingIndicator() {
  if (!el.workingIndicator) return;
  // Latched for the whole turn: agent_start sets it, agent_end clears it.
  // hasLiveItems() alone blinks off during inter-call snapshot gaps, and the
  // queued-before-first-message window has no live items at all.
  const working = state.workingUntilSettled === true;
  el.workingIndicator.classList.toggle("hidden", !working);
}

export function renderWidgets() {
  const widgets = [...state.widgets.entries()];
  if (!widgets.length && !state.footerStatus) {
    el.widgetStack.classList.add("hidden");
    el.widgetStack.innerHTML = "";
    return;
  }

  const cards = widgets.map(([key, lines]) => `
    <article class="widget-card">
      <h3>${escapeHtml(key)}</h3>
      <ul>${lines.map((line) => `<li>${escapeHtml(line)}</li>`).join("")}</ul>
    </article>
  `);

  if (state.footerStatus) {
    cards.unshift(`
      <article class="widget-card">
        <h3>Extension status</h3>
        <div>${escapeHtml(state.footerStatus)}</div>
      </article>
    `);
  }

  el.widgetStack.innerHTML = cards.join("");
  el.widgetStack.classList.remove("hidden");
}
