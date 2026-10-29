/*
 * Markdown rendering for the phone UI.
 *
 * `marked` (vendored, MIT) is loaded LAZILY: it is the single largest asset
 * in the app shell (~42KB raw), yet nothing needs it until the first
 * transcript message renders. Before it arrives we fall back to escaped
 * plain text, so first paint never waits on this file; once loaded, a
 * repaint event upgrades the transcript to real markdown.
 *
 * Security: everything is escaped by default. Raw HTML in the source is NOT
 * passed through — we render it as text. Links are restricted to http/https/
 * mailto, and code fences are escaped. This matters because assistant output
 * can contain arbitrary text, and the phone UI renders extension messages too.
 */
import { escapeHtml, escapeAttribute } from "./formatters.js";

const SAFE_LINK_PROTOCOLS = new Set(["http:", "https:", "mailto:"]);

function sanitizeUrl(href) {
  const value = String(href || "").trim();
  if (!value) return "";
  // Relative links and fragments are fine; they stay on the same origin.
  if (/^[./#?]/.test(value)) return value;
  try {
    const url = new URL(value, "https://example.invalid");
    if (!SAFE_LINK_PROTOCOLS.has(url.protocol)) return "";
    return value;
  } catch {
    return "";
  }
}

/* Language label -> the class the stylesheet understands. */
function languageClass(info) {
  const lang = String(info || "").trim().split(/\s+/)[0].toLowerCase();
  if (!lang) return "";
  if (!/^[a-z0-9_+#.-]{1,24}$/.test(lang)) return "";
  return `language-${lang}`;
}

function renderCodeBlock(code, info) {
  const lang = languageClass(info);
  const label = lang ? lang.replace(/^language-/, "") : "";
  const langBadge = label ? `<span class="code-lang">${escapeHtml(label)}</span>` : "";
  return `<div class="message-code-wrap">${langBadge}<pre class="message-code-block"><code class="${lang}">${escapeHtml(code)}</code></pre></div>`;
}

export function createMarkdownRenderer(Marked) {
  const marked = new Marked({
    gfm: true,
    breaks: false,
    pedantic: false,
  });

  marked.use({
    renderer: {
      // Do not emit raw HTML from the source; show it as escaped text.
      // Marked calls this for both inline and block HTML tokens, so return
      // bare inline content (never a block wrapper, which would nest <p><p>).
      html({ text }) {
        return escapeHtml(text);
      },
      code({ text, lang }) {
        return renderCodeBlock(text, lang);
      },
      codespan({ text }) {
        return `<code>${escapeHtml(text)}</code>`;
      },
      link({ href, title, tokens }) {
        const safe = sanitizeUrl(href);
        const inner = this.parser.parseInline(tokens);
        if (!safe) return inner; // degrade to plain text for unsafe schemes
        const titleAttr = title ? ` title="${escapeAttribute(title)}"` : "";
        return `<a href="${escapeAttribute(safe)}"${titleAttr} target="_blank" rel="noopener noreferrer nofollow">${inner}</a>`;
      },
      image({ href, title, text }) {
        const safe = sanitizeUrl(href);
        if (!safe) return escapeHtml(text || "");
        const titleAttr = title ? ` title="${escapeAttribute(title)}"` : "";
        return `<img src="${escapeAttribute(safe)}" alt="${escapeAttribute(text || "")}"${titleAttr} loading="lazy" decoding="async" />`;
      },
      // Tighten list/heading output for a phone-width column.
      heading({ tokens, depth }) {
        const level = Math.min(Math.max(depth, 1), 4);
        return `<h${level}>${this.parser.parseInline(tokens)}</h${level}>`;
      },
    },
  });

  return marked;
}

let sharedRenderer = null;
let rendererPromise = null;
let readyListeners = [];

/**
 * Kick off the marked load once, on first render request. `onReady` fires
 * when the real parser arrives so the caller can repaint markdown that was
 * rendered with the plain-text fallback.
 */
function loadRenderer(onReady) {
  if (sharedRenderer) {
    onReady();
    return;
  }
  readyListeners.push(onReady);
  if (rendererPromise) return;
  rendererPromise = import("../vendor/marked.esm.js")
    .then((module) => {
      sharedRenderer = createMarkdownRenderer(module.Marked);
      for (const fn of readyListeners.splice(0)) {
        try {
          fn();
        } catch {
          // a broken repaint listener must not block the others
        }
      }
    })
    .catch(() => {
      // Vendor failed to load (offline first paint, SW gap): stay on the
      // escaped-text fallback permanently rather than throwing per render.
      readyListeners.length = 0;
    });
}

function renderer() {
  return sharedRenderer;
}

/**
 * Render markdown to HTML.
 * Falls back to escaped plain text if the parser throws, so a malformed
 * message can never blank out the transcript.
 */
export function renderMarkdownLite(text = "") {
  const normalized = String(text ?? "").replace(/\r\n?/g, "\n");
  // Empty text renders as nothing at all. A visible "(no text)" placeholder
  // made image-only prompts and tool-call-only assistant turns look broken.
  if (!normalized.trim()) return "";

  try {
    const parsed = renderer()?.parse(normalized);
    if (typeof parsed === "string" && parsed) return parsed;
  } catch {
    // fall through to the plain-text fallback below
  }

  // Fallback: escaped plain text with paragraph breaks. Good enough for the
  // first frames; a markdown-ready event triggers a transcript repaint.
  loadRenderer(() => {
    document.dispatchEvent(new CustomEvent("pi-phone:markdown-ready"));
  });
  return normalized
    .split(/\n{2,}/)
    .map((block) => `<p>${escapeHtml(block).replace(/\n/g, "<br />")}</p>`)
    .join("");
}

