const CACHE = "pi-phone-v50";
const ASSETS = [
  "/",
  "/styles.css",
  "/app.js",
  "/vendor/marked.esm.js",
  "/app/attachments.js",
  "/app/autocomplete-controller.js",
  "/app/autocomplete.js",
  "/app/bindings.js",
  "/app/command-catalog.js",
  "/app/commands.js",
  "/app/constants.js",
  "/app/formatters.js",
  "/app/handlers.js",
  "/app/main.js",
  "/app/markdown.js",
  "/app/messages.js",
  "/app/sheet-actions.js",
  "/app/sheet-navigation.js",
  "/app/sheets-view.js",
  "/app/state.js",
  "/app/tool-rendering.js",
  "/app/transport.js",
  "/app/ui.js",
  "/manifest.webmanifest",
  "/icon.svg",
];
const APP_SHELL = new Set(ASSETS);

self.addEventListener("install", (event) => {
  event.waitUntil(caches.open(CACHE).then((cache) => cache.addAll(ASSETS)));
  self.skipWaiting();
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches.keys().then((keys) => Promise.all(keys.filter((key) => key !== CACHE).map((key) => caches.delete(key)))),
  );
  self.clients.claim();
});

async function updateCache(request) {
  const response = await fetch(request);
  if (response.ok) {
    const copy = response.clone();
    caches.open(CACHE).then((cache) => cache.put(request, copy));
  }
  return response;
}

self.addEventListener("fetch", (event) => {
  const { request } = event;
  if (request.method !== "GET") return;

  const url = new URL(request.url);
  if (url.origin !== self.location.origin) return;
  // Mounted under a per-instance prefix (e.g. /phone-8787) when reverse-proxied.
  const base = (self.PI_PHONE_BASE || new URL(self.registration.scope).pathname).replace(/\/$/, "");
  const relative = url.pathname.startsWith(`${base}/`) ? url.pathname.slice(base.length) : url.pathname;
  if (relative.startsWith("/api/") || relative === "/ws") return;

  const useNetworkFirst = request.mode === "navigate" || APP_SHELL.has(relative);

  if (useNetworkFirst) {
    event.respondWith(
      updateCache(request).catch(async () => {
        const cached = await caches.match(request);
        if (cached) return cached;
        return caches.match("/");
      }),
    );
    return;
  }

  event.respondWith(
    caches.match(request).then((cached) => {
      if (cached) return cached;
      return updateCache(request);
    }),
  );
});
