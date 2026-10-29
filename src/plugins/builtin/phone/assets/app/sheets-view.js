import { THINKING_LEVELS } from "./constants.js";
import { commandCategoryLabel, groupedCommands, selectedCommandCategory, sortCommandCategories } from "./command-catalog.js";
import { escapeHtml, formatDateTime, formatTokenCount } from "./formatters.js";
import { el, state } from "./state.js";

function renderStatsSection() {
  if (!state.stats) return '<div class="label">Session stats will appear here after refresh.</div>';
  const tokens = state.stats.tokens || {};

  // Cache economics: providers bill the non-cached input at full price, a
  // cache READ at ~10%, and a cache WRITE at ~125%. The hit rate is the
  // single number that explains why one session costs pennies and another
  // dollars at the same output volume.
  const uncachedInput = Number(tokens.input) || 0;
  const cacheRead = Number(tokens.cacheRead) || 0;
  const cacheWrite = Number(tokens.cacheWrite) || 0;
  const cacheTotal = cacheRead + cacheWrite;
  const hitRate = cacheTotal > 0 ? (cacheRead / cacheTotal) * 100 : null;

  return `
    <div class="stat-grid">
      <div class="stat-chip"><span>Cache hit rate</span><strong>${hitRate != null ? `${hitRate.toFixed(1)}%` : "—"}</strong></div>
      <div class="stat-chip"><span>Input (uncached)</span><strong>${escapeHtml(uncachedInput.toLocaleString())}</strong></div>
      <div class="stat-chip"><span>Cache read</span><strong>${escapeHtml(cacheRead.toLocaleString())}</strong></div>
      <div class="stat-chip"><span>Cache write</span><strong>${escapeHtml(cacheWrite.toLocaleString())}</strong></div>
      <div class="stat-chip"><span>Output</span><strong>${escapeHtml((Number(tokens.output) || 0).toLocaleString())}</strong></div>
      <div class="stat-chip"><span>Output total</span><strong>${escapeHtml((Number(tokens.output) + (Number(tokens.reasoning) || 0)).toLocaleString())}</strong></div>
      <div class="stat-chip"><span>Tool calls</span><strong>${escapeHtml(String(state.stats.toolCalls || 0))}</strong></div>
      <div class="stat-chip"><span>Messages</span><strong>${escapeHtml(String(state.stats.totalMessages || 0))}</strong></div>
      <div class="stat-chip"><span>Cost</span><strong>${escapeHtml(state.stats.cost != null ? `$${Number(state.stats.cost).toFixed(4)}` : "—")}</strong></div>
    </div>
  `;
}

function renderActionsSheet() {
  const session = state.snapshotState || {};
  const status = state.status || state.health || {};
  const streaming = Boolean(status.isStreaming || session.isStreaming);

  // Context window + cache rate, from the last usage entry: the numbers that
  // actually explain spend, without opening the stats sheet.
  const contextUsage = session.contextUsage;
  const contextText = contextUsage && Number.isFinite(contextUsage.percent)
    ? `${contextUsage.percent.toFixed(1)}% · ${formatTokenCount(contextUsage.tokens)} / ${formatTokenCount(contextUsage.contextWindow)}`
    : "—";
  const t = state.stats?.tokens || {};
  const cacheTotal = (Number(t.cacheRead) || 0) + (Number(t.cacheWrite) || 0);
  const cacheText = cacheTotal > 0
    ? `${((Number(t.cacheRead) || 0) / cacheTotal * 100).toFixed(1)}% (${formatTokenCount(t.cacheRead)} read)`
    : "—";

  // One compact details block replaces the old top-bar info popover.
  const detailRows = [
    ["Directory", status.cwd || "—"],
    ["Session", session.sessionName || session.sessionId || "Current session"],
    ["Model", session.model?.name || session.model?.id || "Default"],
    ["Thinking", session.thinkingLevel || "—"],
    ["Context", contextText],
    ["Cache hit", cacheText],
    ["State", `${streaming ? "Streaming" : "Idle"} · ${status.controlOwner || "cli"}`],
    ["Server", status.port ? `${status.host || "127.0.0.1"}:${status.port}` : "—"],
  ];

  return `
    <section class="sheet-section">
      <h3>Session</h3>
      <div class="detail-list">
        ${detailRows.map(([k, v]) => `
          <div class="detail-row">
            <span class="detail-key">${escapeHtml(k)}</span>
            <span class="detail-val mono">${escapeHtml(v)}</span>
          </div>
        `).join("")}
      </div>
    </section>

    <section class="sheet-section">
      <h3>Actions</h3>
      <div class="action-list">
        <button class="secondary" data-sheet-action="commands">Commands &amp; skills</button>
        <button class="secondary" data-sheet-action="refresh">Refresh snapshot</button>
        <button class="secondary" data-sheet-action="new-session">New session</button>
        <button class="secondary" data-sheet-action="compact"${streaming ? " disabled" : ""}>Compact session</button>
        <button class="secondary" data-sheet-action="tree">Session tree</button>
        <button class="secondary" data-sheet-action="cd">Change directory</button>
      </div>
    </section>

    <section class="sheet-section">
      <h3>Configure</h3>
      <div class="action-list">
        <button class="secondary" data-sheet-action="models">Model</button>
        <button class="secondary" data-sheet-action="thinking">Thinking level</button>
        <button class="secondary" data-sheet-action="commands">Commands &amp; skills</button>
        <button class="secondary" data-sheet-action="sessions">Saved sessions</button>
      </div>
    </section>

    <section class="sheet-section">
      <h3>Usage</h3>
      ${renderStatsSection()}
    </section>
  `;
}

function renderThinkingSheet() {
  return `
    <section class="sheet-section">
      <h3>Thinking levels</h3>
      <div class="sheet-list">
        ${THINKING_LEVELS.map((level) => `
          <button class="secondary" data-thinking-level="${escapeHtml(level)}">
            ${escapeHtml(level)}${state.snapshotState?.thinkingLevel === level ? " · current" : ""}
          </button>
        `).join("")}
      </div>
    </section>
  `;
}

function renderModelsSheet() {
  return `
    <section class="sheet-section">
      <h3>Models</h3>
      <div class="model-list">
        ${state.models.length
          ? state.models.map((model) => `
            <button class="secondary" data-model-provider="${escapeHtml(model.provider)}" data-model-id="${escapeHtml(model.id)}">
              <div><strong>${escapeHtml(model.name || model.id)}</strong></div>
              <div class="label">${escapeHtml(`${model.provider}/${model.id}`)}${state.snapshotState?.model?.id === model.id && state.snapshotState?.model?.provider === model.provider ? " · current" : ""}</div>
            </button>
          `).join("")
          : '<div class="label">Loading available models…</div>'}
      </div>
    </section>
  `;
}

function renderCommandsSheet() {
  const groups = groupedCommands();
  const categories = sortCommandCategories([...new Set([...groups.keys()])]);
  const activeCategory = selectedCommandCategory(categories);
  const commands = groups.get(activeCategory) || [];
  const emptyLabel = activeCategory ? `${commandCategoryLabel(activeCategory).toLowerCase()} commands` : "commands";

  return `
    <section class="sheet-section">
      <h3>Commands, skills, prompts</h3>
      <label class="sheet-filter">
        <span class="label">Category</span>
        <select class="sheet-select" data-command-category-select aria-label="Command category">
          ${categories.map((category) => `
            <option value="${escapeHtml(category)}" ${category === activeCategory ? "selected" : ""}>${escapeHtml(commandCategoryLabel(category))}</option>
          `).join("")}
        </select>
      </label>
      <div class="sheet-list">
        ${commands.length ? commands.map((command) => `
          <button
            class="secondary"
            ${command.source === "local"
              ? `data-run-local-command="${escapeHtml(command.name)}"`
              : `data-run-command="/${escapeHtml(command.name)}"`}
          >
            <div><strong>${escapeHtml(`/${command.name}`)}</strong></div>
            <div class="label">${escapeHtml(command.description || "No description")}</div>
          </button>
        `).join("") : `<div class="label">No ${escapeHtml(emptyLabel)} available.</div>`}
      </div>
    </section>
  `;
}

function relativeTime(value) {
  if (!value) return "";
  const then = new Date(value).getTime();
  if (Number.isNaN(then)) return "";
  const diff = Date.now() - then;
  if (diff < 60_000) return "just now";
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)}m ago`;
  if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)}h ago`;
  if (diff < 7 * 86_400_000) return `${Math.floor(diff / 86_400_000)}d ago`;
  return formatDateTime(value);
}

function formatSize(bytes) {
  if (!Number.isFinite(bytes) || bytes <= 0) return "";
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)}K`;
  return `${(bytes / 1024 / 1024).toFixed(1)}M`;
}

function sessionMatchesQuery(session, query) {
  if (!query) return true;
  const haystack = [session.name, session.firstMessage, session.id]
    .filter(Boolean)
    .join(" ")
    .toLowerCase();
  return haystack.includes(query);
}

/** Generated titles ("Session 01a0…") are noise; prefer real text. */
function sessionTitle(session) {
  const name = String(session.name || "").trim();
  if (name && !/^subagent-/.test(name)) return name;
  const first = String(session.firstMessage || "").trim();
  if (first) return first.slice(0, 90);
  if (name) return name;
  return session.id ? `Session ${String(session.id).slice(0, 8)}` : "Session";
}

/**
 * One unified Sessions panel.
 *
 * Previously this was two separate modes ("active" and "saved") behind a
 * toggle button, which was confusing: the same word meant two different lists.
 * Now live sessions, saved sessions and the tree all live in one place, with
 * filtering, so there is a single mental model.
 */
function renderSessionsSheet() {
  state.sessionListLimit ||= 12;
  const query = String(state.sessionQuery || "").trim().toLowerCase();
  const active = sortedActiveSessions();

  const saved = state.sessions
    .filter((session) => sessionMatchesQuery(session, query))
    .slice(0, state.sessionListLimit);
  const totalSaved = state.sessions.length;
  const hiddenDerived = Number(state.subagentCount || 0);

  const renderActiveRow = (session) => {
    const isCurrent = session.id === state.activeSessionId;
    const bits = [
      relativeTime(session.lastActivityAt),
      session.isStreaming ? "working" : "",
      session.hasPendingUiRequest ? "needs input" : "",
      session.model?.name || "",
    ].filter(Boolean).join(" · ");

    return `
      <button class="session-row${isCurrent ? " current" : ""}" data-active-session-id="${escapeHtml(session.id)}">
        <span class="session-row-main">
          <span class="session-row-title">${escapeHtml(session.label || "Session")}</span>
          ${bits ? `<span class="session-row-meta">${escapeHtml(bits)}</span>` : ""}
        </span>
        <span class="session-row-tag">${isCurrent ? "active" : (session.kind === "parent" ? "cli" : "parallel")}</span>
      </button>
    `;
  };

  const renderSavedRow = (session) => {
    const meta = [relativeTime(session.modified), session.isSubagent ? "derived" : "", session.sizeBytes ? formatSize(session.sizeBytes) : ""]
      .filter(Boolean).join(" · ");
    return `
      <button class="session-row" data-session-path="${escapeHtml(session.path)}">
        <span class="session-row-main">
          <span class="session-row-title">${escapeHtml(sessionTitle(session))}</span>
          ${meta ? `<span class="session-row-meta">${escapeHtml(meta)}</span>` : ""}
        </span>
        <span class="session-row-tag">open</span>
      </button>
    `;
  };

  return `
    <section class="sheet-section">
      <h3>Switch</h3>
      <div class="session-actions">
        <button class="secondary" data-sheet-action="new-parallel-session">New parallel</button>
        <button class="secondary" data-sheet-action="new-parent-session">New parent</button>
        <button class="secondary" data-sheet-action="tree">Tree</button>
      </div>
      <div class="session-rows">
        ${active.length ? active.map(renderActiveRow).join("") : '<div class="label">No live sessions.</div>'}
      </div>
    </section>

    <section class="sheet-section">
      <h3>Saved${totalSaved ? ` (${totalSaved})` : ""}</h3>
      <input
        id="session-search"
        class="session-search"
        type="search"
        placeholder="Filter by title or text…"
        value="${escapeHtml(state.sessionQuery || "")}"
        autocomplete="off"
        autocapitalize="none"
        spellcheck="false"
      />
      <div class="session-rows">
        ${state.sessionsLoading
          ? '<div class="label">Loading…</div>'
          : saved.length
            ? saved.map(renderSavedRow).join("")
            : `<div class="label">${totalSaved ? "No matches." : "No saved sessions for this project yet."}</div>`}
      </div>
      ${totalSaved > saved.length ? `
        <button class="session-more" data-sheet-action="more-sessions">
          Show more (${totalSaved - saved.length} remaining)
        </button>
      ` : ""}
      ${hiddenDerived > 0 && !state.showSubagents ? `
        <button class="session-more" data-sheet-action="show-subagents">
          Show ${hiddenDerived} derived session${hiddenDerived === 1 ? "" : "s"}
        </button>
      ` : ""}
    </section>
  `;
}

function sortedActiveSessions() {
  return [...state.activeSessions].sort((left, right) => {
    const leftCurrent = left.id === state.activeSessionId ? 1 : 0;
    const rightCurrent = right.id === state.activeSessionId ? 1 : 0;
    if (leftCurrent !== rightCurrent) return rightCurrent - leftCurrent;

    const leftLive = left.isStreaming ? 1 : 0;
    const rightLive = right.isStreaming ? 1 : 0;
    if (leftLive !== rightLive) return rightLive - leftLive;

    const leftLabel = left.label || left.sessionId || left.id;
    const rightLabel = right.label || right.sessionId || right.id;
    return leftLabel.localeCompare(rightLabel);
  });
}

function renderTreeSheet() {
  if (!state.tree) {
    return `
      <section class="sheet-section">
        <h3>Session tree</h3>
        <div class="label">Loading tree…</div>
      </section>
    `;
  }

  return `
    <section class="sheet-section">
      <h3>Session tree</h3>
      <div class="label mono">${escapeHtml(state.tree.sessionFile || "")}</div>
      <div class="sheet-list">
        ${state.tree.nodes.map((node) => {
          const isCurrent = state.tree.currentLeafId === node.id;
          const onPath = state.tree.currentPathIds.includes(node.id);
          return `
            <div class="sheet-section" style="margin-left:${Math.min(node.depth * 12, 72)}px">
              <div><strong>${escapeHtml(node.summary.kind)}</strong>${node.summary.role ? ` <span class="label">${escapeHtml(node.summary.role)}</span>` : ""}${node.label ? ` <span class="label">#${escapeHtml(node.label)}</span>` : ""}${isCurrent ? ' <span class="label">current</span>' : onPath ? ' <span class="label">path</span>' : ''}</div>
              <div class="label">${escapeHtml(formatDateTime(node.timestamp))}</div>
              <div>${escapeHtml(node.summary.preview || "(empty)")}</div>
              <div class="button-row compact">
                <button class="secondary" data-open-branch-entry="${escapeHtml(node.id)}">Open path</button>
                ${node.summary.role === "user" ? `<button class="secondary" data-fork-entry="${escapeHtml(node.id)}">Fork here</button>` : ""}
              </div>
            </div>
          `;
        }).join("")}
      </div>
    </section>
  `;
}

export function renderSheet() {
  if (el.sheetModal.classList.contains("hidden")) return;

  const titles = {
    actions: "Actions",
    commands: "Commands",
    models: "Models",
    thinking: "Thinking",
    sessions: "Sessions",
    "active-sessions": "Sessions",
    tree: "Tree",
  };
  const nextTitle = titles[state.sheetMode] || "Actions";
  if (el.sheetTitle.textContent !== nextTitle) {
    el.sheetTitle.textContent = nextTitle;
  }

  if (el.sheetSavedSessionsButton) {
    el.sheetSavedSessionsButton.classList.toggle("hidden", state.sheetMode !== "active-sessions");
  }

  const sections = {
    actions: renderActionsSheet() + renderThinkingSheet() + renderModelsSheet(),
    commands: renderCommandsSheet(),
    models: renderModelsSheet() + renderThinkingSheet(),
    thinking: renderThinkingSheet() + renderModelsSheet(),
    sessions: renderSessionsSheet(),
    // "active-sessions" is kept as an alias so any older caller still works,
    // but both entry points now render the same unified panel.
    "active-sessions": renderSessionsSheet(),
    tree: renderTreeSheet(),
  };

  const nextHtml = sections[state.sheetMode] || sections.actions;
  if (el.sheetContent.innerHTML !== nextHtml) {
    el.sheetContent.innerHTML = nextHtml;
  }
}
