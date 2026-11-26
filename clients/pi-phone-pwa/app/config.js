const SERVER_KEY = "pi-phone-server";
const LEGACY_TOKEN_KEY = "pi-phone-token";
let initialServerResolved = false;
let initialServerWasExplicit = false;

export function normalizeServerBase(raw) {
  const value = String(raw || "").trim();
  if (!value) return "";
  try {
    const url = new URL(value);
    if (url.protocol !== "http:" && url.protocol !== "https:") return "";
    return `${url.origin}${url.pathname.replace(/\/$/, "")}`;
  } catch {
    return "";
  }
}

function deepLinkedServerBase() {
  const params = new URLSearchParams(window.location.search);
  const query = normalizeServerBase(params.get("server"));
  if (query) return query;
  const hash = new URLSearchParams(window.location.hash.replace(/^#/, ""));
  return normalizeServerBase(hash.get("server"));
}

function consumeDeepLink() {
  if (initialServerResolved) return;
  initialServerResolved = true;
  const selected = deepLinkedServerBase();
  if (!selected) return;
  initialServerWasExplicit = true;
  localStorage.setItem(SERVER_KEY, selected);
  try {
    const url = new URL(window.location.href);
    url.searchParams.delete("server");
    const hash = new URLSearchParams(url.hash.replace(/^#/, ""));
    hash.delete("server");
    const nextHash = hash.toString();
    url.hash = nextHash ? `#${nextHash}` : "";
    window.history.replaceState({}, document.title, `${url.pathname}${url.search}${url.hash}`);
  } catch {
    // History APIs may be unavailable in embedded/test documents; the
    // in-memory selection is still safe because all callers use state.serverBase.
  }
}

function sameOriginServer(base) {
  try {
    return new URL(base).origin === window.location.origin;
  } catch {
    return false;
  }
}

export function getServerBase() {
  consumeDeepLink();
  const stored = normalizeServerBase(localStorage.getItem(SERVER_KEY));
  if (stored) return stored;
  const injected = normalizeServerBase(window.PI_PHONE_SERVER);
  if (injected) return injected;
  const basePath = String(window.PI_PHONE_BASE || "/").replace(/\/$/, "");
  return `${window.location.origin}${basePath}`;
}

export function setServerBase(raw) {
  const normalized = normalizeServerBase(raw);
  if (normalized) localStorage.setItem(SERVER_KEY, normalized);
  else localStorage.removeItem(SERVER_KEY);
  return normalized;
}

export function tokenStorageKey(base = getServerBase()) {
  return `pi-phone-token:${base}`;
}

export function loadStoredToken(base = getServerBase()) {
  const key = tokenStorageKey(base);
  // An explicit deep link is an untrusted relay selection. Do not send any
  // previously stored credential automatically; the user must enter it.
  if (initialServerWasExplicit) return "";
  const current = localStorage.getItem(key);
  if (current) return current;
  // The legacy key has no server identity. Never migrate it to a remote
  // stored relay: either case could send an old bearer token to an attacker-
  // controlled endpoint.
  if (!sameOriginServer(base)) return "";
  const legacy = localStorage.getItem(LEGACY_TOKEN_KEY);
  if (legacy) {
    localStorage.setItem(key, legacy);
    localStorage.removeItem(LEGACY_TOKEN_KEY);
    return legacy;
  }
  return "";
}

export function storeStoredToken(token, base = getServerBase()) {
  const key = tokenStorageKey(base);
  if (token) localStorage.setItem(key, token);
  else localStorage.removeItem(key);
}

export function encodeWsToken(token) {
  const bytes = new TextEncoder().encode(token);
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}
