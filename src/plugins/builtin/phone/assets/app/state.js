import { TOKEN_STORAGE_KEY } from "./constants.js";

export const state = {
  health: null,
  status: null,
  snapshotState: null,
  snapshotWorkerId: null,
  messages: [],
  /**
   * Messages queued with pi but not yet delivered (steer / follow-up).
   * Mirrors SessionStatus.pendingMessages so the UI can show and cancel them.
   */
  pendingMessages: [],
  // Windowing: the server sends only the most recent messages. These track
  // whether older history exists on the server and how much is already loaded.
  historyTruncated: false,
  totalMessageCount: 0,
  loadedMessageCount: 0,
  loadingEarlier: false,
  commands: [],
  models: [],
  sessions: [],
  /** Unified Sessions panel state. */
  sessionsLoading: false,
  sessionQuery: "",
  sessionListLimit: 12,
  showSubagents: false,
  subagentCount: 0,
  activeSessions: [],
  activeSessionId: null,
  tree: null,
  stats: null,
  widgets: new Map(),
  footerStatus: "",
  quota: null,
  quotaRequestId: 0,
  liveAssistant: null,
  liveTools: new Map(),
  followLatest: true,
  lastAutoFollowAt: 0,
  lastAutoFollowHeight: 0,
  ignoreScrollTrackingUntil: 0,
  lastUserScrollIntentAt: 0,
  pendingUiRequest: null,
  socket: null,
  reconnectTimer: null,
  manuallyClosed: false,
  token: localStorage.getItem(TOKEN_STORAGE_KEY) || "",
  sheetMode: "actions",
  commandSheetCategory: "local",
  autocompleteContext: null,
  autocompleteItems: [],
  autocompleteRemoteRequestId: 0,
  autocompleteRemoteTimer: null,
  attachments: [],
  nextAttachmentTokenId: 1,
  lastSheetPointerAction: "",
  lastSheetPointerActionAt: 0,
  toolPanelOpen: new Map(),
  // Message-level <details> (Thinking / Tool calls / Details), keyed by
  // message id + section title so open state survives re-renders.
  detailOpen: new Map(),
  /**
   * Detail keys the user opened/closed by hand. The auto thinking behaviour
   * (open while reasoning, collapse when the answer starts) must not override
   * a deliberate choice.
   */
  thinkingUserToggled: new Set(),
  /**
   * Id of the message whose Thinking section the user manually expanded and
   * is reading. The streaming auto-collapse stands down for that message
   * until the user collapses it themselves (or the transcript resets).
   */
  userExpandedThinkingId: null,
  statusCardOpen: false,
  isComposing: false,
  /**
   * Working-indicator latch: set on agent_start, cleared on agent_end.
   * Keeps the bottom spinner visible through snapshot gaps between tool
   * calls instead of blinking off while the next call is still coming.
   */
  workingUntilSettled: false,
};

export const el = {
  abortButton: document.querySelector("#abort-button"),
  actionsButton: document.querySelector("#actions-button"),
  attachImageButton: document.querySelector("#attach-image-button"),
  attachmentStrip: document.querySelector("#attachment-strip"),
  banner: document.querySelector("#banner"),
  commandStrip: document.querySelector("#command-strip"),
  composerWrap: document.querySelector(".composer-wrap"),
  connectionPill: document.querySelector("#connection-pill"),
  cwdValue: document.querySelector("#cwd-value"),
  imageInput: document.querySelector("#image-input"),
  insertCommandButton: document.querySelector("#insert-command-button"),
  jumpToLatestButton: document.querySelector("#jump-to-latest-button"),
  loadEarlierButton: document.querySelector("#load-earlier-button"),
  loginModal: document.querySelector("#login-modal"),
  messages: document.querySelector("#messages"),
  workingIndicator: document.querySelector("#working-indicator"),
  modelValue: document.querySelector("#model-value"),
  pendingStrip: document.querySelector("#pending-strip"),
  promptInput: document.querySelector("#prompt-input"),
  quotaContext: document.querySelector("#quota-context"),
  quotaCwd: document.querySelector("#quota-cwd"),
  quotaMetaRow: document.querySelector("#quota-meta-row"),
  quotaPillsRow: document.querySelector("#quota-pills-row"),
  quotaPrimary: document.querySelector("#quota-primary"),
  quotaRow: document.querySelector("#quota-row"),
  quotaSecondary: document.querySelector("#quota-secondary"),
  refreshButton: document.querySelector("#refresh-button"),
  sendButton: document.querySelector("#send-button"),
  statusCard: document.querySelector("#status-card"),
  statusDot: document.querySelector("#status-dot"),
  stopButton: document.querySelector("#stop-button"),
  serverValue: document.querySelector("#server-value"),
  sessionSidebarButton: document.querySelector("#session-sidebar-button"),
  sessionValue: document.querySelector("#session-value"),
  sheetCloseButton: document.querySelector("#sheet-close-button"),
  sheetContent: document.querySelector("#sheet-content"),
  sheetModal: document.querySelector("#sheet-modal"),
  sheetSavedSessionsButton: document.querySelector("#sheet-saved-sessions-button"),
  sheetTitle: document.querySelector("#sheet-title"),
  steerButton: document.querySelector("#steer-button"),
  streamingValue: document.querySelector("#streaming-value"),
  thinkingValue: document.querySelector("#thinking-value"),
  toastHost: document.querySelector("#toast-host"),
  tokenInput: document.querySelector("#token-input"),
  tokenSaveButton: document.querySelector("#token-save-button"),
  topbarTitle: document.querySelector("#topbar-title"),
  uiModal: document.querySelector("#ui-modal"),
  uiModalButtons: document.querySelector("#ui-modal-buttons"),
  uiModalInput: document.querySelector("#ui-modal-input"),
  uiModalMessage: document.querySelector("#ui-modal-message"),
  uiModalOptions: document.querySelector("#ui-modal-options"),
  uiModalTitle: document.querySelector("#ui-modal-title"),
  widgetStack: document.querySelector("#widget-stack"),
};
