const SERVER_KEY = "pi-phone-server";
const LEGACY_TOKEN_KEY = "pi-phone-token";

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

export function getServerBase() {
  const params = new URLSearchParams(window.location.search);
  const query = normalizeServerBase(params.get("server"));
  if (query) {
    localStorage.setItem(SERVER_KEY, query);
    return query;
  }
  const hash = new URLSearchParams(window.location.hash.replace(/^#/, ""));
  const hashed = normalizeServerBase(hash.get("server"));
  if (hashed) {
    localStorage.setItem(SERVER_KEY, hashed);
    return hashed;
  }
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

export function loadStoredToken() {
  const key = tokenStorageKey();
  const current = localStorage.getItem(key);
  if (current) return current;
  const legacy = localStorage.getItem(LEGACY_TOKEN_KEY);
  if (legacy) {
    localStorage.setItem(key, legacy);
    localStorage.removeItem(LEGACY_TOKEN_KEY);
    return legacy;
  }
  return "";
}

export function storeStoredToken(token) {
  const key = tokenStorageKey();
  if (token) localStorage.setItem(key, token);
  else localStorage.removeItem(key);
}

export function encodeWsToken(token) {
  const bytes = new TextEncoder().encode(token);
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}
