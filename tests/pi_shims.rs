//! Phase 4c: pi-* virtual shim surface (`@earendil-works/pi-tui` Key/Input/
//! SelectList/Markdown, `@earendil-works/pi-coding-agent` theme/code/hint/
//! frontmatter/estimate helpers, `@earendil-works/pi-agent-core` Agent stub)
//! and the real-extension load matrix for the pi-subagents static chain.
//!
//! Fixture extensions are seeded from source strings at runtime (the repo
//! hygiene gate forbids tracked `.ts` files), mirroring tests/node_shims.rs.

use std::path::{Path, PathBuf};

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

/// See tests/extensions_runtime.rs: fixtures must be written at runtime.
fn write_temp_fixture(test_name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pir-pi-shims-{}-{test_name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    for (name, content) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create fixture subdir");
        }
        std::fs::write(&path, content).expect("write fixture");
    }
    dir
}

fn test_config_at(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("test-model".to_string()),
        session_id: Some("test-session".to_string()),
        system_prompt: "You are a test.".to_string(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

async fn new_runtime_at(dir: &Path) -> ExtensionRuntime {
    let rt = ExtensionRuntime::new(test_config_at(dir))
        .await
        .expect("runtime construction");
    rt.allow_root(dir.to_path_buf());
    rt
}

/// One seeded extension that links every phase-4c shim export and records the
/// behaviour probes on `globalThis.__piShims` from `session_start`.
const PI_SHIMS_TS: &str = r##"
import {
  Key, matchesKey, isKeyRelease, fuzzyFilter, fuzzyMatch,
  Input, SelectList, Markdown, Box, Text, Container, Spacer,
} from "@earendil-works/pi-tui";
import {
  getMarkdownTheme, highlightCode, getLanguageFromPath,
  keyText, keyHint, rawKeyHint, DynamicBorder,
  convertToLlm, createReadOnlyTools,
  parseFrontmatter, stripFrontmatter,
  estimateTokens, calculateContextTokens,
} from "@earendil-works/pi-coding-agent";
import { Agent } from "@earendil-works/pi-agent-core";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};

    // --- Key identifiers ---
    out.key = {
      escape: Key.escape,
      enter: Key.enter,
      up: Key.up,
      ctrlC: Key.ctrl("c"),
      shiftTab: Key.shift("tab"),
      ctrlShiftP: Key.ctrlShift("p"),
    };

    // --- matchesKey common shapes ---
    out.mk = {
      ctrlA: matchesKey("\x01", "ctrl+a"),
      ctrlAViaKeyHelper: matchesKey("\x01", Key.ctrl("a")),
      ctrlC: matchesKey("\x03", "ctrl+c"),
      shiftTab: matchesKey("\x1b[Z", "shift+tab"),
      tab: matchesKey("\t", "tab"),
      singleChar: matchesKey("a", "a"),
      shiftLetterUpper: matchesKey("A", "shift+a"),
      escape: matchesKey("\x1b", "escape"),
      enterCr: matchesKey("\r", "enter"),
      enterReturn: matchesKey("\r", "return"),
      up: matchesKey("\x1b[A", "up"),
      down: matchesKey("\x1b[B", "down"),
      altLeft: matchesKey("\x1b[1;3D", "alt+left"),
      ctrlLeft: matchesKey("\x1b[1;5D", "ctrl+left"),
      altEnterLegacy: matchesKey("\x1b\r", "alt+enter"),
      backspace: matchesKey("\x7f", "backspace"),
      space: matchesKey(" ", "space"),
      f1: matchesKey("\x1bOP", "f1"),
      pageUp: matchesKey("\x1b[5~", "pageUp"),
      delete: matchesKey("\x1b[3~", "delete"),
      kittyCtrlA: matchesKey("\x1b[97;5u", "ctrl+a"),
      modifyOtherKeysCtrlC: matchesKey("\x1b[27;5;99~", "ctrl+c"),
      noMatch: matchesKey("a", "b"),
      badKeyId: matchesKey("a", "not a key id"),
    };

    // --- key release detection ---
    out.release = {
      kittyRelease: isKeyRelease("\x1b[97;5:3u"),
      kittyPress: isKeyRelease("\x1b[97;5u"),
      legacy: isKeyRelease("\x1b[A"),
      bracketedPasteWithColons: isKeyRelease("\x1b[200~90:62:3F:A5\x1b[201~"),
    };

    // --- fuzzy matching ---
    const items = ["read", "browse", "record"];
    out.fuzzy = {
      hit: fuzzyFilter(items, "rd", (x: any) => x),
      prefix: fuzzyFilter(items, "re", (x: any) => x),
      none: fuzzyFilter(items, "zz", (x: any) => x),
      blank: fuzzyFilter(items, "   ", (x: any) => x).length,
      matchShape: fuzzyMatch("rd", "record"),
      missShape: fuzzyMatch("qq", "record"),
    };

    // --- Input state machine ---
    const input = new Input({ prompt: "> " });
    let submitted: any = null;
    let changeCount = 0;
    input.onSubmit = (value: any) => { submitted = value; };
    input.onChange = () => { changeCount++; };
    input.handleInput("h");
    input.handleInput("i");
    out.inputTyped = input.getValue();
    input.handleInput("\x7f");
    out.inputAfterBackspace = input.getValue();
    input.handleInput("!");
    input.handleInput("\x01");
    input.handleInput(">");
    out.inputAfterHomeInsert = input.getValue();
    input.handleInput("\x1b[C");
    input.handleInput("\x1b[199~ignored-unknown-sequence");
    input.handleInput("\r");
    out.inputSubmitted = submitted;
    out.inputChangeCount = changeCount;
    out.inputRenderLines = input.render(30).length;
    out.inputPlaceholderRenderLines = new Input().render(10).length;

    // --- SelectList ---
    const selTheme = {
      selectedPrefix: (t: any) => t,
      selectedText: (t: any) => t,
      description: (t: any) => t,
      scrollInfo: (t: any) => t,
      noMatch: (t: any) => t,
    };
    const sel = new SelectList(
      [{ value: "a", label: "Alpha" }, { value: "b", label: "Beta" }, { value: "c", label: "Gamma" }],
      5,
      selTheme,
    );
    let selected: any = null;
    sel.onSelect = (item: any) => { selected = item.value; };
    sel.handleInput("\x1b[B");
    out.selAfterDown = sel.getSelectedItem()?.value;
    sel.handleInput("\r");
    out.selSelected = selected;
    sel.setFilter("b");
    out.selFiltered = sel.getSelectedItem()?.value;
    out.selRenderLines = sel.render(30).length;

    // --- Box / Text render sanity ---
    const box = new Box(1, 1);
    box.addChild(new Text("hello", 0, 0));
    out.boxRenderLines = box.render(20).length;
    out.textRenderLines = new Text("hello", 0, 0).render(20).length;
    const container = new Container();
    container.addChild(new Spacer(1));
    container.addChild(new Text("x", 0, 0));
    out.containerRenderLines = container.render(10).length;

    // --- Markdown + markdown theme ---
    const mdTheme = getMarkdownTheme();
    const themeFns = [
      "heading", "link", "linkUrl", "code", "codeBlock", "codeBlockBorder",
      "quote", "quoteBorder", "hr", "listBullet", "bold", "italic",
      "underline", "strikethrough", "highlightCode",
    ].filter((name: any) => typeof mdTheme[name] === "function");
    out.mdThemeFnCount = themeFns.length;
    out.mdThemeHeading = mdTheme.heading("H");
    out.mdThemeBold = mdTheme.bold("B");
    out.mdThemeHighlight = mdTheme.highlightCode("const a = 1;\nlet b = 2;").length;

    const md = new Markdown(
      "# Title\n\nSome **bold** and `code` here.\n\n```js\nconst x = 1;\n```\n- item one\n\n> quoted\n",
      0, 0, mdTheme,
    );
    const mdLines: string[] = md.render(40);
    out.mdRenderLines = mdLines.length;
    out.mdAllStrings = mdLines.every((line: any) => typeof line === "string");
    out.mdJoinedHasTitle = mdLines.join("\n").includes("Title");
    out.mdJoinedHasBoldText = mdLines.join("\n").includes("bold");
    out.mdWrappedLines = new Markdown("word ".repeat(60).trim(), 0, 0, mdTheme).render(20).length;
    out.mdEmptyRenderLines = new Markdown("", 0, 0, mdTheme).render(20).length;

    // --- code / language helpers ---
    out.lang = {
      ts: getLanguageFromPath("/x/a.ts"),
      py: getLanguageFromPath("x.py"),
      md: getLanguageFromPath("notes.md"),
      none: getLanguageFromPath("noext") === undefined ? null : getLanguageFromPath("noext"),
    };
    out.highlightLines = highlightCode("const a = 1;\nlet b = 2;").length;
    out.highlightEmptyLines = highlightCode("").length;

    // --- key hints ---
    out.keyTexts = {
      expand: keyText("app.tools.expand"),
      confirm: keyText("tui.select.confirm"),
      unknown: keyText("no.such.binding"),
    };
    out.rawHint = rawKeyHint("↑↓", "navigate");
    out.keyHint = keyHint("tui.select.cancel", "cancel");

    // --- DynamicBorder ---
    out.borderDefault = new DynamicBorder().render(6).length;
    out.borderColored = new DynamicBorder((s: any) => "[" + s + "]").render(3)[0];

    // --- convertToLlm identity ---
    const messages = [{ role: "user", content: "hi" }];
    out.convertToLlmIdentity = convertToLlm(messages) === messages;

    // --- createReadOnlyTools ---
    const tools = createReadOnlyTools(process.cwd());
    out.toolNames = tools.map((t: any) => t.name);
    out.toolsShaped = tools.every((t: any) =>
      typeof t.name === "string" && t.name.length > 0 &&
      typeof t.description === "string" && t.description.length > 0 &&
      t.parameters && t.parameters.type === "object");

    // --- frontmatter ---
    const fm = parseFrontmatter("---\nname: demo\ntools: [read, grep]\ncount: 3\n---\n\nBody text\n");
    out.fm = {
      name: fm.frontmatter?.name,
      tools: fm.frontmatter?.tools,
      count: fm.frontmatter?.count,
      body: fm.body,
    };
    const plain = parseFrontmatter("no frontmatter here");
    out.fmPlain = {
      empty: Object.keys(plain.frontmatter).length === 0,
      body: plain.body,
    };
    out.stripped = stripFrontmatter("---\na: 1\n---\nBody");

    // --- token estimates ---
    out.tokens = {
      user: estimateTokens({ role: "user", content: "0123456789" }),
      assistant: estimateTokens({ role: "assistant", content: [{ type: "text", text: "abcd" }] }),
      ctx: calculateContextTokens({ input: 10, output: 5, cacheRead: 2, cacheWrite: 1 }),
      ctxTotal: calculateContextTokens({ totalTokens: 42, input: 1, output: 1, cacheRead: 0, cacheWrite: 0 }),
    };

    // --- Agent stub ---
    const agent = new Agent({ initialState: { systemPrompt: "s", model: "m", tools: [] } });
    out.agentConstructed = agent != null && typeof agent === "object";
    let promptError = "";
    try {
      await agent.prompt("hello");
    } catch (error: any) {
      promptError = String(error && error.message ? error.message : error);
    }
    out.agentPromptError = promptError;
    agent.abort();
    await agent.waitForIdle();
    out.agentAbortOk = true;

    globalThis.__piShims = out;
  });
};
"##;

/// Every phase-4c named export links, and the recorded behaviour probes match
/// the upstream-derived expectations.
#[tokio::test(flavor = "current_thread")]
async fn pi_shims_surface_and_behaviour() {
    let dir = write_temp_fixture("surface", &[]);
    let rt = new_runtime_at(&dir).await;
    rt.seed_module("pir-ext:pi-shims.ts", PI_SHIMS_TS)
        .expect("seed pi-shims extension");
    let loaded = rt.seeded_load("pir-ext:pi-shims.ts").await;
    assert_eq!(loaded.expect("seeded load"), 1);

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt
        .emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    assert!(
        outcome.errors.is_empty(),
        "handler errors while exercising pi shims: {:?}",
        outcome.errors
    );

    let v: Value = rt
        .eval_json("JSON.stringify(globalThis.__piShims)")
        .await
        .expect("read pi shim results");

    // Key helper objects.
    assert_eq!(v["key"]["escape"], "escape");
    assert_eq!(v["key"]["enter"], "enter");
    assert_eq!(v["key"]["up"], "up");
    assert_eq!(v["key"]["ctrlC"], "ctrl+c");
    assert_eq!(v["key"]["shiftTab"], "shift+tab");
    assert_eq!(v["key"]["ctrlShiftP"], "ctrl+shift+p");

    // matchesKey common shapes.
    let mk = &v["mk"];
    for key in [
        "ctrlA",
        "ctrlAViaKeyHelper",
        "ctrlC",
        "shiftTab",
        "tab",
        "singleChar",
        "shiftLetterUpper",
        "escape",
        "enterCr",
        "enterReturn",
        "up",
        "down",
        "altLeft",
        "ctrlLeft",
        "altEnterLegacy",
        "backspace",
        "space",
        "f1",
        "pageUp",
        "delete",
        "kittyCtrlA",
        "modifyOtherKeysCtrlC",
    ] {
        assert_eq!(mk[key], true, "matchesKey probe `{key}` must match: {v}");
    }
    assert_eq!(mk["noMatch"], false, "a/b must not match: {v}");
    assert_eq!(mk["badKeyId"], false, "invalid key id must not match: {v}");

    // Key release detection.
    assert_eq!(v["release"]["kittyRelease"], true, "{v}");
    assert_eq!(v["release"]["kittyPress"], false, "{v}");
    assert_eq!(v["release"]["legacy"], false, "{v}");
    assert_eq!(
        v["release"]["bracketedPasteWithColons"], false,
        "bracketed paste must never count as release: {v}"
    );

    // Fuzzy filtering: matches sorted best-first, misses dropped.
    assert_eq!(v["fuzzy"]["hit"], json!(["read", "record"]), "{v}");
    assert_eq!(v["fuzzy"]["prefix"][0], "read", "{v}");
    assert_eq!(v["fuzzy"]["none"], json!([]), "{v}");
    assert_eq!(v["fuzzy"]["blank"], 3, "blank query returns all items: {v}");
    assert_eq!(v["fuzzy"]["matchShape"]["matches"], true, "{v}");
    assert!(
        v["fuzzy"]["matchShape"]["score"].is_number(),
        "matchShape score must be numeric: {v}"
    );
    assert_eq!(v["fuzzy"]["missShape"]["matches"], false, "{v}");

    // Input state machine.
    assert_eq!(v["inputTyped"], "hi", "{v}");
    assert_eq!(v["inputAfterBackspace"], "h", "{v}");
    assert_eq!(v["inputAfterHomeInsert"], ">h!", "{v}");
    assert_eq!(
        v["inputSubmitted"], ">h!",
        "enter must submit the value: {v}"
    );
    assert_eq!(v["inputRenderLines"], 1, "{v}");
    assert_eq!(v["inputPlaceholderRenderLines"], 1, "{v}");

    // SelectList.
    assert_eq!(v["selAfterDown"], "b", "down must move the selection: {v}");
    assert_eq!(v["selSelected"], "b", "enter must select: {v}");
    assert_eq!(v["selFiltered"], "b", "filter must narrow items: {v}");
    assert_eq!(v["selRenderLines"], 1, "{v}");

    // Render sanity for the pre-existing components.
    assert_eq!(
        v["boxRenderLines"], 3,
        "Box 1/1 padding + 1 content line: {v}"
    );
    assert_eq!(v["textRenderLines"], 1, "{v}");
    assert_eq!(v["containerRenderLines"], 2, "Spacer(1) + Text line: {v}");

    // Markdown + theme.
    assert_eq!(
        v["mdThemeFnCount"], 15,
        "every MarkdownTheme slot must exist: {v}"
    );
    assert_eq!(
        v["mdThemeHeading"], "H",
        "headless theme is passthrough: {v}"
    );
    assert_eq!(
        v["mdThemeBold"], "\u{1b}[1mB\u{1b}[22m",
        "bold is minimal ANSI in the headless runtime: {v}"
    );
    assert_eq!(
        v["mdThemeHighlight"], 2,
        "highlightCode returns one entry per line: {v}"
    );
    assert!(
        v["mdRenderLines"].as_u64().unwrap() >= 5,
        "markdown render must produce content lines: {v}"
    );
    assert_eq!(v["mdAllStrings"], true, "{v}");
    assert_eq!(v["mdJoinedHasTitle"], true, "{v}");
    assert_eq!(v["mdJoinedHasBoldText"], true, "{v}");
    assert!(
        v["mdWrappedLines"].as_u64().unwrap() > 3,
        "long text must wrap into multiple lines: {v}"
    );
    assert_eq!(v["mdEmptyRenderLines"], 0, "{v}");

    // Language/highlight helpers.
    assert_eq!(v["lang"]["ts"], "typescript", "{v}");
    assert_eq!(v["lang"]["py"], "python", "{v}");
    assert_eq!(v["lang"]["md"], "markdown", "{v}");
    assert_eq!(v["lang"]["none"], Value::Null, "{v}");
    assert_eq!(v["highlightLines"], 2, "{v}");
    assert_eq!(
        v["highlightEmptyLines"], 1,
        "empty code is one empty line: {v}"
    );

    // Key hints.
    assert_eq!(v["keyTexts"]["expand"], "ctrl+o", "{v}");
    assert_eq!(v["keyTexts"]["confirm"], "enter", "{v}");
    assert_eq!(
        v["keyTexts"]["unknown"], "",
        "unknown bindings fall back to empty: {v}"
    );
    assert!(
        v["rawHint"].as_str().unwrap().starts_with("↑↓")
            && v["rawHint"].as_str().unwrap().contains("navigate"),
        "rawKeyHint must carry keys + description: {v}"
    );
    assert!(
        v["keyHint"].as_str().unwrap().contains("cancel"),
        "keyHint must carry the description: {v}"
    );

    // DynamicBorder.
    assert_eq!(v["borderDefault"], 1, "{v}");
    assert_eq!(v["borderColored"], "[───]", "{v}");

    // convertToLlm passthrough.
    assert_eq!(v["convertToLlmIdentity"], true, "{v}");

    // createReadOnlyTools.
    assert_eq!(v["toolNames"], json!(["read", "grep", "find", "ls"]), "{v}");
    assert_eq!(
        v["toolsShaped"], true,
        "each tool needs name/description/parameters: {v}"
    );

    // Frontmatter.
    assert_eq!(v["fm"]["name"], "demo", "{v}");
    assert_eq!(v["fm"]["tools"], json!(["read", "grep"]), "{v}");
    assert_eq!(v["fm"]["count"], 3, "{v}");
    assert_eq!(v["fm"]["body"], "Body text", "{v}");
    assert_eq!(v["fmPlain"]["empty"], true, "{v}");
    assert_eq!(v["fmPlain"]["body"], "no frontmatter here", "{v}");
    assert_eq!(v["stripped"], "Body", "{v}");

    // Token estimates.
    assert_eq!(v["tokens"]["user"], 3, "10 chars / 4 rounded up: {v}");
    assert_eq!(v["tokens"]["assistant"], 1, "{v}");
    assert_eq!(v["tokens"]["ctx"], 18, "component sum: {v}");
    assert_eq!(v["tokens"]["ctxTotal"], 42, "totalTokens wins: {v}");

    // Agent stub.
    assert_eq!(v["agentConstructed"], true, "{v}");
    assert_eq!(
        v["agentPromptError"], "Agent execution is not available in the pir extension runtime",
        "prompt must throw the documented message: {v}"
    );
    assert_eq!(v["agentAbortOk"], true, "{v}");

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Real installed extensions (phase-4c acceptance)
// ---------------------------------------------------------------------------

fn home_dir() -> Option<PathBuf> {
    std::env::var("USERPROFILE")
        .ok()
        .or_else(|| std::env::var("HOME").ok())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

async fn load_one(path: &Path) -> (usize, String) {
    let cwd = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let rt = ExtensionRuntime::new(test_config_at(&cwd))
        .await
        .expect("runtime construction");
    let path_buf = path.to_path_buf();
    let outcome = rt
        .load(std::slice::from_ref(&path_buf))
        .await
        .expect("load");
    let failed = outcome
        .errors
        .iter()
        .map(|error| error.error.clone())
        .collect::<Vec<_>>()
        .join(" | ");
    (outcome.loaded.len(), failed)
}

/// Report one installed-extension matrix and enforce the opt-in strictness:
/// with `PIR_REQUIRE_INSTALLED_EXTENSIONS=1` a matrix that found nothing fails
/// instead of reporting green with nothing exercised. Absence stays a skip by
/// default (CI has no `~/.pi` tree).
fn finish_installed_matrix(label: &str, executed: usize, skipped: usize) {
    eprintln!("[pi_shims] {label}: executed={executed} skipped={skipped}");
    if executed == 0 {
        assert!(
            std::env::var_os("PIR_REQUIRE_INSTALLED_EXTENSIONS").is_none(),
            "PIR_REQUIRE_INSTALLED_EXTENSIONS is set but no installed extension fixture was \
             found ({label})"
        );
    }
}

/// pi-subagents' full static chain must load (was: "Could not find export
/// 'getMarkdownTheme'"), and the rpiv-todo / codegraph / tool-repair
/// regressions keep loading. context-mode records its error string only —
/// phase 4c intentionally stops at the mkdir gate it used to fail on.
#[tokio::test(flavor = "current_thread")]
async fn real_pi_subagents_loads_and_regressions_hold() {
    let Some(home) = home_dir() else {
        eprintln!("[pi_shims] no home directory; skipping real-extension matrix");
        // One primary target + three regressions + context-mode.
        finish_installed_matrix("real-extension matrix", 0, 5);
        return;
    };
    let npm_root = home.join(".pi/agent/npm/node_modules");
    let mut executed = 0usize;
    let mut skipped = 0usize;

    // Primary target: pi-subagents must fully load when installed.
    let subagents = npm_root.join("pi-subagents/index.js");
    if subagents.is_file() {
        executed += 1;
        let (loaded, failed) = load_one(&subagents).await;
        eprintln!("[pi_shims] pi-subagents: loaded={loaded} errors={failed:?}");
        assert_eq!(failed, "", "pi-subagents must load without errors");
        assert_eq!(loaded, 1, "pi-subagents entry must register");
    } else {
        skipped += 1;
        eprintln!("[pi_shims] pi-subagents: skipped (not installed)");
    }

    // Loaded regressions from earlier phases.
    let regressions: &[(&str, &str)] = &[
        ("@juicesharp/rpiv-todo", "index.ts"),
        ("@vndv/pi-codegraph", "extensions/codegraph.ts"),
        ("pi-tool-repair", "tool-repair.ts"),
    ];
    for (name, entry) in regressions {
        let path = npm_root.join(name).join(entry);
        if !path.is_file() {
            skipped += 1;
            eprintln!("[pi_shims] {name}: skipped (not installed)");
            continue;
        }
        executed += 1;
        let (loaded, failed) = load_one(&path).await;
        eprintln!("[pi_shims] {name}: loaded={loaded} errors={failed:?}");
        assert_eq!(failed, "", "{name} must keep loading");
        assert_eq!(loaded, 1, "{name} entry must register");
    }

    // context-mode: recorded only (its next gap is the better-sqlite3 load
    // path, i.e. past the mkdir gate this phase fixed).
    let context_mode = npm_root.join("context-mode/build/adapters/pi/extension.js");
    if context_mode.is_file() {
        executed += 1;
        let (loaded, failed) = load_one(&context_mode).await;
        eprintln!("[pi_shims] context-mode: loaded={loaded} errors={failed:?}");
        assert!(
            !failed.contains("EACCES"),
            "context-mode must be past the sandbox mkdir gate: {failed}"
        );
    } else {
        skipped += 1;
        eprintln!("[pi_shims] context-mode: skipped (not installed)");
    }
    finish_installed_matrix("real-extension matrix", executed, skipped);
}

// ---------------------------------------------------------------------------
// Phase 4e: pi-phone / pi-web-access linked surface + real load matrix
// ---------------------------------------------------------------------------

/// Phase-4e additions to `@earendil-works/pi-coding-agent`: the thin
/// `buildSessionContext` projection, the host-backed `SessionManager`
/// (static constructors over the `pirSession*` host ops; the write face and
/// handle-less instances throw the standard unavailable error), and the
/// value-export presence check for the pi-phone import list. The session
/// directory/file paths are injected so probes never touch `~/.pi` or `C:/tmp`.
fn phase4e_pi_ts(session_dir: &str) -> String {
    format!(
        r#"
import {{
  buildSessionContext, SessionManager, getAgentDir,
  parseFrontmatter, stripFrontmatter, estimateTokens, calculateContextTokens,
}} from "@earendil-works/pi-coding-agent";

export default (pi: any) => {{
  pi.on("session_start", async () => {{
    const out: any = {{}};

    out.types = {{
      buildSessionContext: typeof buildSessionContext,
      SessionManager: typeof SessionManager,
      getAgentDir: typeof getAgentDir,
      parseFrontmatter: typeof parseFrontmatter,
      stripFrontmatter: typeof stripFrontmatter,
      estimateTokens: typeof estimateTokens,
      calculateContextTokens: typeof calculateContextTokens,
      resizeImage: typeof (await import("@earendil-works/pi-coding-agent")).resizeImage,
    }};

    const entries = [
      {{ type: "message", id: "a", parentId: null, message: {{ role: "user", content: "hello" }} }},
      {{ role: "assistant", content: [{{ type: "text", text: "world" }}] }},
      {{ type: "compaction", id: "b", parentId: "a", summary: "dropped" }},
      {{ type: "message", id: "c", parentId: "a", message: {{ role: "toolResult", content: [] }} }},
    ];
    const context = buildSessionContext(entries, "leaf-id");
    out.contextRoles = context.messages.map((message: any) => message.role);
    out.contextFirst = context.messages[0];
    out.contextSecond = context.messages[1].content[0].text;
    out.contextEmpty = buildSessionContext(undefined).messages.length;
    out.contextIsArray = Array.isArray(context.messages);

    const probeDir = "{session_dir}";
    const manager = new SessionManager({{ probe: true }});
    out.managerConstructed = manager != null && typeof manager === "object";

    // Host-backed read face: opening a missing file plans a fresh session
    // (upstream `open` semantics), the write face and handle-less instances
    // throw the standard unavailable error.
    const opened = SessionManager.open(probeDir + "/missing.jsonl");
    out.openedLeafId = opened.getLeafId();
    out.openedEntries = opened.getEntries().length;
    out.openedIsPersisted = opened.isPersisted();
    out.openedFile = typeof opened.getSessionFile();
    out.openedBranch = Array.isArray(opened.getBranch());
    out.openedTree = Array.isArray(opened.getTree());

    const created = SessionManager.create(probeDir);
    out.createdFile = typeof created.getSessionFile();
    out.createdHasConversation = created.hasConversation();
    try {{
      created.createBranchedSession("nope");
      out.branchMissing = "no-throw";
    }} catch (error: any) {{
      out.branchMissing = error.message;
    }}

    const garbage = probeDir + "/garbage.jsonl";
    (await import("node:fs")).writeFileSync(garbage, "not-a-session-line\\n");
    const statics = [
      () => SessionManager.open(garbage),
      () => SessionManager.forkFrom(probeDir + "/missing.jsonl", probeDir),
    ];
    out.staticErrors = statics.map((call: any) => {{
      try {{
        call();
        return "no-throw";
      }} catch (error: any) {{
        return error.message;
      }}
    }});
    out.asyncStatics = await Promise.all([
      SessionManager.list(probeDir).then(() => "resolved", (error: any) => error.message),
      SessionManager.listAll().then((sessions: any) => Array.isArray(sessions) ? "resolved" : "no-array", (error: any) => error.message),
    ]);

    const instanceCalls = [
      () => manager.getLeafId(),
      () => manager.getBranch(),
      () => manager.getTree(),
      () => manager.createBranchedSession("id"),
      () => manager.getEntries(),
      () => manager.appendMessage({{ role: "user", content: "x" }}),
    ];
    out.instanceErrors = instanceCalls.map((call: any) => {{
      try {{
        call();
        return "no-throw";
      }} catch (error: any) {{
        return error.message;
      }}
    }});

    out.agentDir = typeof getAgentDir() === "string" && getAgentDir().length > 0;

    globalThis.__phase4ePi = out;
  }});
}};
"#
    )
}

#[tokio::test(flavor = "current_thread")]
async fn phase4e_session_manager_and_build_session_context() {
    let dir = write_temp_fixture("phase4e-pi", &[("index.ts", "")]);
    let source = phase4e_pi_ts(&dir.to_string_lossy().replace('\\', "/"));
    std::fs::write(dir.join("index.ts"), &source).expect("write phase-4e fixture");
    let rt = new_runtime_at(&dir).await;
    rt.seed_module("pir-ext:phase4e-pi.ts", source.as_str())
        .expect("seed phase-4e fixture");
    let loaded = rt
        .seeded_load("pir-ext:phase4e-pi.ts")
        .await
        .expect("seeded load");
    assert_eq!(loaded, 1, "phase-4e fixture must load");

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt
        .emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    assert!(
        outcome.errors.is_empty(),
        "handler errors in phase-4e fixture: {:?}",
        outcome.errors
    );
    let v: Value = rt
        .eval_json("JSON.stringify(globalThis.__phase4ePi)")
        .await
        .expect("read phase-4e results");

    // Every audited export name links (typeof probes, not call probes).
    assert_eq!(v["types"]["buildSessionContext"], "function", "{v}");
    assert_eq!(v["types"]["SessionManager"], "function", "{v}");
    assert_eq!(v["types"]["getAgentDir"], "function", "{v}");
    assert_eq!(v["types"]["parseFrontmatter"], "function", "{v}");
    assert_eq!(v["types"]["stripFrontmatter"], "function", "{v}");
    assert_eq!(v["types"]["estimateTokens"], "function", "{v}");
    assert_eq!(v["types"]["calculateContextTokens"], "function", "{v}");
    assert_eq!(
        v["types"]["resizeImage"], "function",
        "pi-web-access dynamic-import face must exist: {v}"
    );

    // Thin buildSessionContext: message entries project, others drop.
    assert_eq!(
        v["contextRoles"],
        json!(["user", "assistant", "toolResult"]),
        "{v}"
    );
    assert_eq!(
        v["contextFirst"],
        json!({ "role": "user", "content": "hello" }),
        "{v}"
    );
    assert_eq!(v["contextSecond"], "world", "{v}");
    assert_eq!(v["contextEmpty"], 0, "missing entries project to []: {v}");
    assert_eq!(v["contextIsArray"], true, "{v}");

    // SessionManager: host-backed read face + standard-error write face.
    assert_eq!(v["managerConstructed"], true, "{v}");
    assert_eq!(v["openedLeafId"], json!(null), "{v}");
    assert_eq!(v["openedEntries"], 0, "{v}");
    assert_eq!(v["openedIsPersisted"], true, "{v}");
    assert_eq!(v["openedFile"], "string", "{v}");
    assert_eq!(v["openedBranch"], true, "{v}");
    assert_eq!(v["openedTree"], true, "{v}");
    assert_eq!(v["createdFile"], "string", "{v}");
    assert_eq!(v["createdHasConversation"], false, "{v}");
    assert!(
        v["branchMissing"]
            .as_str()
            .unwrap_or_default()
            .contains("Entry nope not found"),
        "{v}"
    );
    let static_errors = v["staticErrors"].as_array().expect("staticErrors array");
    assert!(
        static_errors[0]
            .as_str()
            .unwrap_or_default()
            .contains("Session file is not a valid pi session"),
        "{v}"
    );
    assert!(
        static_errors[1]
            .as_str()
            .unwrap_or_default()
            .contains("Cannot fork"),
        "{v}"
    );
    assert_eq!(v["asyncStatics"], json!(["resolved", "resolved"]), "{v}");
    let expected_instance =
        "SessionManager.appendMessage is not available in the pir extension runtime";
    for (index, error) in v["instanceErrors"]
        .as_array()
        .expect("instanceErrors array")
        .iter()
        .enumerate()
    {
        // Every probe rides a handle-less instance (upstream's constructor is
        // private): each face throws the standard unavailable error.
        let message = error.as_str().unwrap_or_default();
        assert!(
            message.ends_with("is not available in the pir extension runtime"),
            "{v}"
        );
        if index == 5 {
            assert_eq!(message, expected_instance, "{v}");
        }
    }
    assert_eq!(v["agentDir"], true, "{v}");
}

/// Phase-4e acceptance: pi-web-access (dist bundle) and pi-phone (index.ts)
/// load in-process — the last two user extensions beside context-mode. The
/// full error chain is part of the assertion message when a load fails.
#[tokio::test(flavor = "current_thread")]
async fn phase4e_pi_web_access_and_pi_phone_load() {
    let Some(home) = home_dir() else {
        eprintln!("[pi_shims] no home directory; skipping phase-4e load matrix");
        // pi-web-access + pi-phone.
        finish_installed_matrix("phase-4e load matrix", 0, 2);
        return;
    };
    let mut executed = 0usize;
    let mut skipped = 0usize;

    let web_access = home.join(".pi/agent/npm/node_modules/pi-web-access/dist/index.js");
    if web_access.is_file() {
        executed += 1;
        let (loaded, failed) = load_one(&web_access).await;
        assert_eq!(
            (loaded, failed.as_str()),
            (1, ""),
            "pi-web-access must load (full error chain above): {failed}"
        );
    } else {
        skipped += 1;
        eprintln!("[pi_shims] pi-web-access: skipped (not installed)");
    }

    // pi-phone is a working checkout, not an npm-installed package.
    let pi_phone = home.join("Desktop/github/pi-phone/index.ts");
    if pi_phone.is_file() {
        executed += 1;
        let (loaded, failed) = load_one(&pi_phone).await;
        assert_eq!(
            (loaded, failed.as_str()),
            (1, ""),
            "pi-phone must load (full error chain above): {failed}"
        );
    } else {
        skipped += 1;
        eprintln!(
            "[pi_shims] pi-phone: skipped (no checkout at {})",
            pi_phone.display()
        );
    }
    finish_installed_matrix("phase-4e load matrix", executed, skipped);
}

/// The upstream pi v1.2.0 (`7d75a0e3e`) **value**-export surface of every
/// package pir mirrors, one list per package: `Object.keys` of the module
/// namespace (`packages/*/src/index.ts`, `export *` chains resolved, types
/// excluded — they have no runtime key). Generated and re-verified with
/// `scripts/gen_pi_surface.mjs` (name-by-name diff against the running shim);
/// adding an upstream name means updating the list here and the shim together.
const PI_SURFACE_TS: &str = r##"
import * as TuiModule from "@earendil-works/pi-tui";
import TuiDefault from "@earendil-works/pi-tui";
import * as CodingAgentModule from "@earendil-works/pi-coding-agent";
import CodingAgentDefault from "@earendil-works/pi-coding-agent";
import * as AgentCoreModule from "@earendil-works/pi-agent-core";
import AgentCoreDefault from "@earendil-works/pi-agent-core";
import * as AiModule from "@earendil-works/pi-ai";
import AiDefault from "@earendil-works/pi-ai";
import * as CodemodeModule from "@earendil-works/pi-codemode";
import CodemodeDefault from "@earendil-works/pi-codemode";

const UPSTREAM_NAMES: Record<string, string[]> = {
  "pi-tui": [
    "Box", "CURSOR_MARKER", "CancellableLoader", "CombinedAutocompleteProvider", "Container",
    "Editor", "HStack", "Image", "Input", "Key", "KeybindingsManager", "Loader", "Markdown",
    "Marked", "MouseRegion", "ProcessTerminal", "ScrollView", "SelectList", "SettingsList",
    "Spacer", "StdinBuffer", "TUI_KEYBINDINGS", "Text", "TruncatedText", "TuiAltScreen",
    "TuiMainScreen", "VStack", "allocateImageId", "backgroundAnsi", "calculateImageRows",
    "colorToHex", "colorToOkhsl", "colorToOklch", "colorToRgb", "compositeTuiLine",
    "decodeKittyPrintable", "deleteAllKittyImages", "deleteKittyImage", "detectCapabilities",
    "encodeITerm2", "encodeKitty", "foregroundAnsi", "formatProgramStatus", "fuzzyFilter",
    "fuzzyMatch", "getCapabilities", "getCellDimensions", "getGifDimensions",
    "getImageDimensions", "getJpegDimensions", "getKeybindings", "getNativeClipboard",
    "getOsc8LinkAtColumn", "getPngDimensions", "getTerminalColorMode", "getWebpDimensions",
    "hyperlink", "imageFallback", "indexedColor", "isAppleTerminalSession", "isFocusable",
    "isKeyRelease", "isKeyRepeat", "isKittyProtocolActive", "isViewportTUI", "matchesKey",
    "mixColors", "okhslColor", "oklabToOkhslLightness", "oklchColor", "parseColor", "parseKey",
    "parseTerminalColorSchemeReport", "renderFakeCursor", "renderImage", "renderLatex",
    "resetCapabilitiesCache", "rgbColor", "setCapabilities", "setCapabilityOverrides",
    "setCellDimensions", "setImageTranscoder", "setKeybindings", "setKittyProtocolActive",
    "sliceByColumn", "stripTerminalSequences", "styleText", "styleTextWithAnsi",
    "truncateToWidth", "visibleWidth", "wrapTextWithAnsi",
  ],
  "pi-coding-agent": [
    "AgentSession", "AgentSessionRuntime", "ArminComponent", "AssistantMessageComponent",
    "BashExecutionComponent", "BorderedLoader", "BranchSummaryMessageComponent",
    "CONFIG_DIR_NAME", "CURRENT_SESSION_VERSION", "CompactionSummaryMessageComponent",
    "CredentialSynchronizationError", "CustomEditor", "CustomMessageComponent",
    "DEFAULT_COMPACTION_SETTINGS", "DEFAULT_MAX_BYTES", "DEFAULT_MAX_LINES",
    "DefaultPackageManager", "DefaultResourceLoader", "DynamicBorder",
    "ExtensionEditorComponent", "ExtensionInputComponent", "ExtensionRunner",
    "ExtensionSelectorComponent", "FooterComponent", "InteractiveMode", "LoginDialogComponent",
    "ModelRegistry", "ModelRuntime", "ModelSelectorComponent", "OAuthSelectorComponent",
    "ProjectTrustStore", "RpcClient", "SessionManager", "SessionSelectorComponent",
    "SettingsManager", "SettingsSelectorComponent", "ShowImagesSelectorComponent",
    "SkillInvocationMessageComponent", "Theme", "ThemeSelectorComponent",
    "ThinkingSelectorComponent", "ToolExecutionComponent", "TreeSelectorComponent",
    "UserMessageComponent", "UserMessageSelectorComponent", "VERSION",
    "VIRTUAL_MODEL_STATE_ENTRY", "buildContextEntries", "buildSessionContext",
    "buildSessionProjection", "calculateContextTokens", "collectEntriesForBranchSummary",
    "compact", "convertToLlm", "convertToPng", "copyToClipboard", "createAgentSession",
    "createAgentSessionFromServices", "createAgentSessionRuntime", "createAgentSessionServices",
    "createBashTool", "createBashToolDefinition", "createCodemodeExtension",
    "createCodingTools", "createEditTool", "createEditToolDefinition", "createEventBus",
    "createExtensionRuntime", "createFindTool", "createFindToolDefinition", "createGrepTool",
    "createGrepToolDefinition", "createLocalBashOperations", "createLocalPowerShellOperations",
    "createLsTool", "createLsToolDefinition", "createMcpExtension", "createPowerShellTool",
    "createPowerShellToolDefinition", "createReadOnlyTools", "createReadTool",
    "createReadToolDefinition", "createSyntheticSourceInfo", "createToolSearchExtension",
    "createWriteTool", "createWriteToolDefinition", "defineTool",
    "detectSupportedImageMimeTypeFromFile", "discoverAndLoadExtensions", "estimateTokens",
    "findCutPoint", "findTurnStartIndex", "formatDimensionNote", "formatSize",
    "formatSkillsForPrompt", "generateBranchSummary", "generateDiffString", "generateSummary",
    "generateSummaryWithUsage", "generateUnifiedPatch", "getAgentDir", "getDocsPath",
    "getExamplesPath", "getLanguageFromPath", "getLastAssistantUsage",
    "getLatestCompactionEntry", "getMarkdownTheme", "getPackageDir", "getPowerShellConfig",
    "getReadmePath", "getSelectListTheme", "getSettingsListTheme", "getShellConfig",
    "hasTrustRequiringProjectResources", "highlightCode", "initTheme", "isBashToolResult",
    "isEditToolResult", "isFindToolResult", "isGrepToolResult", "isLsToolResult",
    "isPowerShellToolResult", "isReadToolResult", "isToolCallEventType", "isWriteToolResult",
    "keyHint", "keyText", "loadProjectContextFiles", "loadSkills", "loadSkillsFromDir", "main",
    "migrateSessionEntries", "parseArgs", "parseFrontmatter", "parseSessionEntries",
    "parseSkillBlock", "prepareBranchEntries", "rawKeyHint", "readStoredCredential",
    "renderDiff", "resizeImage", "resolveCliModel", "resolveModelScopeWithDiagnostics",
    "runPrintMode", "runRpcMode", "serializeConversation", "sessionEntryToContextMessages",
    "shouldCompact", "stripFrontmatter", "truncateHead", "truncateLine", "truncateTail",
    "truncateToVisualLines", "withFileMutationQueue", "wrapRegisteredTool",
    "wrapRegisteredTools",
  ],
  "pi-agent-core": [
    "Agent", "agentLoop", "agentLoopContinue", "runAgentLoop", "runAgentLoopContinue",
    "runToolCall", "setDefaultStreamFn", "streamProxy",
  ],
  "pi-ai": [
    "AssistantMessageEventStream", "AssistantMessageFrameEncoder",
    "DEFAULT_MAX_AGENT_RETRY_DELAY_MS", "EventStream", "InMemoryCredentialStore",
    "InMemoryModelsStore", "ModelsError", "StringEnum", "Type",
    "appendAssistantMessageDiagnostic", "calculateCost", "clampThinkingLevel",
    "cleanupSessionResources", "collapseSystemMessages", "contentText",
    "createAssistantMessageDiagnostic", "createAssistantMessageEventStream", "createFauxCore",
    "createInitialSystemMessage", "createModels", "createProvider", "declarationsEqual",
    "defaultProviderAuthContext", "envApiKeyAuth", "extractDiagnosticError",
    "fauxAssistantMessage", "fauxProvider", "fauxText", "fauxThinking", "fauxToolCall",
    "formatThrownValue", "getCurrentSystemMessage", "getCurrentSystemPrompt", "getCurrentTools",
    "getDeclaredTools", "getInitialSystemMessage", "getModelType", "getOverflowPatterns",
    "getSupportedThinkingLevels", "getSystemMessageText", "getToolStateChanges", "hasApi",
    "hasNonAdditiveToolChanges", "hasToolRedefinitions", "isContextOverflow", "isModelType",
    "isRecoverableLength", "isRetryableAssistantError", "lazyApi", "lazyOAuth", "lazyStream",
    "modelsAreEqual", "normalizeContext", "parseJsonWithRepair", "parseStreamingJson",
    "reduceAssistantMessageFrames", "registerSessionResourceCleanup",
    "renderSystemMessageUpdate", "repairJson", "resolveTranscript", "resolveTranscriptTools",
    "retryAssistantCall", "retryDelayMs", "toToolDeclaration", "uuidv7",
    "validateToolArguments", "validateToolCall", "withoutInitialSystemMessage",
  ],
  "pi-codemode": [
    "CODEMODE_OPTIONS_PREFIX", "CODEMODE_SOURCE_GRAMMAR", "CodemodeSandbox",
    "CodemodeSandboxBase", "CodemodeSourceError", "DEFAULT_INPUT_SCHEMA_MAX_CHARS",
    "DEFAULT_INTERRUPT_BUDGET", "InlineCodemodeSandbox", "MAX_OUTPUT_CHARS", "MAX_OUTPUT_ITEMS",
    "MAX_STORE_TOTAL_CHARS", "MAX_STORE_VALUE_CHARS", "RemoteCodemodeSandbox",
    "loadQuickJSWasm", "parseCodemodeSource", "renderDeclarations", "renderToolOutputType",
    "renderToolSample", "renderToolSignature", "schemaToType", "serveCodemodeRemote",
    "toCodemodeIdentifier",
  ],
};

const MODULES: Record<string, [any, any]> = {
  "pi-tui": [TuiModule, TuiDefault],
  "pi-coding-agent": [CodingAgentModule, CodingAgentDefault],
  "pi-agent-core": [AgentCoreModule, AgentCoreDefault],
  "pi-ai": [AiModule, AiDefault],
  "pi-codemode": [CodemodeModule, CodemodeDefault],
};

export default (pi: any) => {
  pi.on("session_start", async () => {
    const result: any = {};
    for (const pkg of Object.keys(UPSTREAM_NAMES)) {
      const [namespace, defaultExport] = MODULES[pkg];
      const missingNamed: string[] = [];
      const missingDefault: string[] = [];
      for (const name of UPSTREAM_NAMES[pkg]) {
        if ((namespace as any)[name] === undefined) missingNamed.push(name);
        if ((defaultExport as any)[name] === undefined) missingDefault.push(name);
      }
      result[pkg] = {
        count: UPSTREAM_NAMES[pkg].length,
        missingNamed,
        missingDefault,
      };
    }
    globalThis.__piSdkSurface = result;
  });
};
"##;

/// Every upstream value export links in both the namespace and the default
/// object of the matching pir virtual shim (superset allowed; a missing name is
/// an extension-breaker at link time).
#[tokio::test(flavor = "current_thread")]
async fn pi_sdk_value_export_surface_lock() {
    let dir = write_temp_fixture("sdk-surface", &[]);
    let rt = new_runtime_at(&dir).await;
    rt.seed_module("pir-ext:pi-sdk-surface.ts", PI_SURFACE_TS)
        .expect("seed pi SDK surface fixture");
    let loaded = rt.seeded_load("pir-ext:pi-sdk-surface.ts").await;
    assert_eq!(loaded.expect("seeded load"), 1);

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt
        .emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    assert!(
        outcome.errors.is_empty(),
        "handler errors in the SDK surface fixture: {:?}",
        outcome.errors
    );

    let v: Value = rt
        .eval_json("JSON.stringify(globalThis.__piSdkSurface)")
        .await
        .expect("read SDK surface results");

    for pkg in [
        "pi-tui",
        "pi-coding-agent",
        "pi-agent-core",
        "pi-ai",
        "pi-codemode",
    ] {
        let entry = &v[pkg];
        assert!(
            entry["count"].as_u64().unwrap_or(0) > 0,
            "{pkg} must be audited: {v}"
        );
        assert_eq!(
            entry["missingNamed"],
            json!([]),
            "{pkg} shim lost upstream value exports (namespace): {v}"
        );
        assert_eq!(
            entry["missingDefault"],
            json!([]),
            "{pkg} shim lost upstream value exports (default object): {v}"
        );
    }
    assert_eq!(v["pi-agent-core"]["count"], 8, "{v}");
    assert_eq!(v["pi-tui"]["count"], 91, "{v}");
    assert_eq!(v["pi-coding-agent"]["count"], 156, "{v}");
    assert_eq!(v["pi-ai"]["count"], 68, "{v}");
    assert_eq!(v["pi-codemode"]["count"], 22, "{v}");

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// pi-codemode shim: DRIFT D14 (constraint tags) + D25 (MCP decoupling) and the
// runtime-face tiers, including the `/portable` and `/cloudflare` subpaths.
// ---------------------------------------------------------------------------

/// The declaration-rendering probes are the upstream `declarations.test.ts`
/// expectations (pi `8a9c051f8` for the tags, `24382bedd`/`c661bc574` for the
/// MCP decoupling), so a port drift shows up as a byte-level mismatch.
const CODEMODE_SHIM_TS: &str = r##"
import * as Codemode from "@earendil-works/pi-codemode";
import CodemodeDefault from "@earendil-works/pi-codemode";
import * as Portable from "@earendil-works/pi-codemode/portable";
import * as Cloudflare from "@earendil-works/pi-codemode/cloudflare";
import * as WorkerEntry from "@earendil-works/pi-codemode/worker";
import { renderToolSignature as DeclaredSignature } from "@earendil-works/pi-codemode/declarations";
import { parseCodemodeSource as SourcedParse } from "@earendil-works/pi-codemode/source";
import {
  CodemodeSandbox, CodemodeSandboxBase, InlineCodemodeSandbox, RemoteCodemodeSandbox,
  CodemodeSandboxDurableObject, serveCodemodeRemote, loadQuickJSWasm,
  createInlineCodemodeSandbox, cloudflareQuickJSWasm,
  DEFAULT_INTERRUPT_BUDGET, DEFAULT_REMOTE_MEMORY_LIMIT_BYTES,
  schemaToType, renderToolSignature, renderToolSample, renderDeclarations,
  renderToolOutputType, parseCodemodeSource, CodemodeSourceError, toCodemodeIdentifier,
  CODEMODE_OPTIONS_PREFIX, CODEMODE_SOURCE_GRAMMAR, DEFAULT_INPUT_SCHEMA_MAX_CHARS,
  MAX_OUTPUT_ITEMS,
} from "@earendil-works/pi-codemode";

function thrown(call: () => any): any {
  try {
    call();
    return "no-throw";
  } catch (error: any) {
    return { message: String(error && error.message ? error.message : error), code: error && error.code };
  }
}

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};

    // --- D14: constraints the type cannot express become `@` tags ---
    out.constraints = schemaToType({
      type: "object",
      properties: {
        timeoutMs: { type: "integer", minimum: 0, maximum: 30000, default: 0, description: "Wait in milliseconds." },
        id: { type: "string", pattern: "^[a-z]+$", maxLength: 8 },
        tags: { type: "array", items: { type: "string" }, maxItems: 5, uniqueItems: true, deprecated: true },
        name: { type: "string" },
      },
      required: ["id"],
    });
    out.nullableAnyOf = schemaToType({
      type: "object",
      properties: { limit: { anyOf: [{ type: "integer", minimum: 1, maximum: 100 }, { type: "null" }], default: null, title: "Limit" } },
    });
    out.bothBranches = schemaToType({
      type: "object",
      properties: { n: { anyOf: [{ type: "integer", minimum: 10, maximum: 50, format: "int32" }, { type: "null" }], minimum: 0, maximum: 20, format: "port" } },
    });
    out.draft04Bounds = schemaToType({
      type: "object",
      properties: {
        n: { type: "integer", minimum: 0, exclusiveMinimum: true, maximum: 10, exclusiveMaximum: false },
        m: { anyOf: [{ type: "number", maximum: 5, exclusiveMaximum: true }, { type: "null" }], maximum: 8 },
      },
    });
    out.longTagDropped = schemaToType({
      type: "object",
      properties: { config: { type: "object", default: { text: "x".repeat(100) } } },
    });
    out.lineTerminators = schemaToType({
      type: "object",
      properties: { sep: { type: "string", pattern: "^[\r\n\u2028\u2029]$", default: "\u2028", description: "a\u2028b" } },
    });
    const budgeted = { type: "object", properties: { n: { type: "integer", minimum: 1 } } };
    out.budget = {
      plain: schemaToType(budgeted),
      untagged: schemaToType(budgeted, { maxChars: 20 }),
      unknown: schemaToType(budgeted, { maxChars: 10 }),
    };

    // --- D25: output schemas render as their own types (no MCP shapes) ---
    out.signature = renderToolSignature({
      name: "hidden-dynamic-tool",
      inputSchema: { type: "object", properties: { city: { type: "string" } }, required: ["city"], additionalProperties: false },
      outputSchema: { type: "object", properties: { ok: { type: "boolean" } }, required: ["ok"] },
    });
    out.freeSignature = renderToolSignature({ name: "free" });
    out.mcpShapedOutput = renderToolSignature({
      name: "plain",
      inputSchema: { type: "object", properties: {} },
      outputSchema: {
        type: "object",
        properties: {
          content: { type: "array", items: { type: "object" } },
          isError: { type: "boolean" },
          _meta: { type: "object" },
        },
        required: ["content"],
      },
    });
    out.mcpHelpersGone = [
      typeof (Codemode as any).MCP_TYPESCRIPT_PREAMBLE,
      typeof (Codemode as any).mcpStructuredContentSchema,
      typeof (CodemodeDefault as any).MCP_TYPESCRIPT_PREAMBLE,
    ];
    out.sample = renderToolSample({ name: "foo", description: "bar", inputSchema: { type: "string" } });
    out.outputTypeMissing = renderToolOutputType(undefined);
    out.declarations = renderDeclarations({
      tools: [{ name: "t", description: "d", inputSchema: { type: "string" } }],
      globals: [{ name: "ns.g", description: "g", inputSchema: { type: "string" } }],
    });

    // --- pure-JS source/identifier surface still links through every subpath ---
    out.parsed = parseCodemodeSource('// @options: {"timeout_ms": 500}\nreturn 1;');
    out.identifier = toCodemodeIdentifier("hidden-dynamic-tool");
    out.constants = [CODEMODE_OPTIONS_PREFIX, DEFAULT_INPUT_SCHEMA_MAX_CHARS, MAX_OUTPUT_ITEMS, typeof CODEMODE_SOURCE_GRAMMAR];
    out.sourceError = thrown(() => parseCodemodeSource("   "));
    // DRIFT D13 (upstream `b6d505cd1`): an options line past the first line is
    // an error instead of being silently ignored.
    out.misplacedOptions = [
      thrown(() => parseCodemodeSource('const a = 1;\n// @options: {"timeout_ms": 500}\nreturn a;')),
      thrown(() => parseCodemodeSource('a\n\n  // @options: {"timeout_ms": 1}\nb')),
    ];
    out.optionsPrefixVariant = parseCodemodeSource("// @optionsx: {}\nreturn 1;");
    out.sourceErrorIsType = (() => {
      try {
        parseCodemodeSource("");
        return false;
      } catch (error: any) {
        return error instanceof CodemodeSourceError;
      }
    })();

    // --- runtime tiers: registries real, the sandbox runs on the host ---
    // DRIFT §2.3 item 2 / D28: `execute()`/`start()` route to the host
    // codemode executor; the remote/wasm faces below keep throwing.
    const base = new CodemodeSandboxBase({
      timeoutMs: 1234,
      tools: [{ name: "alpha" }, { name: "beta" }],
      globals: [{ name: "ns.x" }],
    });
    const baseRun = await base.execute("return 1 + 1;");
    out.base = {
      tools: base.tools.map((tool: any) => tool.name),
      globals: base.globals.map((global_: any) => global_.name),
      timeoutMs: base.timeoutMs,
      unregistered: base.unregisterTool("alpha"),
      afterUnregister: base.tools.map((tool: any) => tool.name),
      duplicate: thrown(() => base.registerTool({ name: "beta" })),
      run: { ok: baseRun.ok, value: baseRun.value, output: baseRun.output, calls: baseRun.calls },
      startValidation: thrown(() => base.start("return 1;", {})),
    };
    const closed = new CodemodeSandboxBase({});
    await closed.close();
    out.closeFlipsFlag = closed.closed === true;

    const host = new CodemodeSandbox({ timeoutMs: 5000 });
    out.hostIsBase = host instanceof CodemodeSandboxBase;
    const hostRun = await host.execute("return 1;");
    out.hostExecute = { ok: hostRun.ok, value: hostRun.value };

    const inline = new InlineCodemodeSandbox({ wasm: {} });
    out.inlineIsBase = inline instanceof CodemodeSandboxBase;
    const inlineRun = await inline.execute("return 1;");
    out.inlineExecute = { ok: inlineRun.ok, value: inlineRun.value };

    const remote = new RemoteCodemodeSandbox({ remote: async () => {} });
    out.remoteIsBase = remote instanceof CodemodeSandboxBase;
    out.remoteExecute = thrown(() => remote.execute("return 1;"));
    out.remoteStart = thrown(() => remote.start("return 1;", {}));

    out.serve = thrown(() => serveCodemodeRemote({}, async () => null, {}));
    out.loadWasm = await loadQuickJSWasm().then(() => "resolved", (error: any) => error.message);
    out.durableObject = thrown(() => new CodemodeSandboxDurableObject());
    const fromWrangler = createInlineCodemodeSandbox({ timeoutMs: 5000 });
    const wranglerRun = await fromWrangler.execute("return 1;");
    out.wrangler = {
      isInline: fromWrangler instanceof InlineCodemodeSandbox,
      run: { ok: wranglerRun.ok, value: wranglerRun.value },
      wasmAccess: thrown(() => (cloudflareQuickJSWasm as any).module),
    };
    out.constantsRuntime = [DEFAULT_INTERRUPT_BUDGET, DEFAULT_REMOTE_MEMORY_LIMIT_BYTES];

    // --- one module serves every subpath entry ---
    out.subpaths = {
      portableInline: Portable.InlineCodemodeSandbox === InlineCodemodeSandbox,
      portableRemote: Portable.RemoteCodemodeSandbox === RemoteCodemodeSandbox,
      portableBase: Portable.CodemodeSandboxBase === CodemodeSandboxBase,
      portableServe: typeof Portable.serveCodemodeRemote === "function",
      portableBudget: Portable.DEFAULT_INTERRUPT_BUDGET,
      cloudflareDurable: typeof Cloudflare.CodemodeSandboxDurableObject === "function",
      cloudflareWasm: Cloudflare.cloudflareQuickJSWasm === cloudflareQuickJSWasm,
      cloudflareInline: Cloudflare.createInlineCodemodeSandbox === createInlineCodemodeSandbox,
      cloudflareMemory: Cloudflare.DEFAULT_REMOTE_MEMORY_LIMIT_BYTES,
      declarationsSignature: DeclaredSignature === renderToolSignature,
      sourceParse: SourcedParse === parseCodemodeSource,
      workerEntryKeys: Object.keys(WorkerEntry).length > 0,
      defaultMirrorsNamed: Object.keys(Codemode).every(
        (name: any) => name === "default" || (CodemodeDefault as any)[name] !== undefined,
      ),
    };

    globalThis.__codemode = out;
  });
};
"##;

#[tokio::test(flavor = "current_thread")]
async fn codemode_shim_declarations_and_runtime_tiers() {
    let dir = write_temp_fixture("codemode-shim", &[]);
    let rt = new_runtime_at(&dir).await;
    rt.seed_module("pir-ext:codemode-shim.ts", CODEMODE_SHIM_TS)
        .expect("seed codemode shim fixture");
    let loaded = rt.seeded_load("pir-ext:codemode-shim.ts").await;
    assert_eq!(loaded.expect("seeded load"), 1);

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt
        .emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    assert!(
        outcome.errors.is_empty(),
        "handler errors in the codemode shim fixture: {:?}",
        outcome.errors
    );

    let v: Value = rt
        .eval_json("JSON.stringify(globalThis.__codemode)")
        .await
        .expect("read codemode results");

    // D14: upstream declarations.test.ts expectations, verbatim.
    assert_eq!(
        v["constraints"],
        json!([
            "{",
            "  // @maxLength 8 @pattern ^[a-z]+$",
            "  id: string;",
            "  name?: string;",
            "  // @maxItems 5 @uniqueItems @deprecated",
            "  tags?: Array<string>;",
            "  // Wait in milliseconds.",
            "  // @integer @minimum 0 @maximum 30000 @default 0",
            "  timeoutMs?: number;",
            "}",
        ]
        .join("\n")),
        "{v}"
    );
    assert_eq!(
        v["nullableAnyOf"], "{\n  // @integer @minimum 1 @maximum 100\n  limit?: number | null;\n}",
        "{v}"
    );
    assert_eq!(
        v["bothBranches"],
        "{\n  // @integer @minimum 10 @maximum 20 @format port @format int32\n  n?: number | null;\n}",
        "{v}"
    );
    assert_eq!(
        v["draft04Bounds"],
        json!([
            "{",
            "  // @maximum 8 @exclusiveMaximum 5",
            "  m?: number | null;",
            "  // @integer @exclusiveMinimum 0 @maximum 10",
            "  n?: number;",
            "}",
        ]
        .join("\n")),
        "{v}"
    );
    assert_eq!(
        v["longTagDropped"], "{ config?: { [key: string]: unknown; }; }",
        "tags longer than 80 chars are left out: {v}"
    );
    assert_eq!(
        v["lineTerminators"],
        json!([
            "{",
            "  // a",
            "  // b",
            "  // @pattern ^[\\u000d\\u000a\\u2028\\u2029]$ @default \"\\u2028\"",
            "  sep?: string;",
            "}",
        ]
        .join("\n")),
        "{v}"
    );
    assert_eq!(
        v["budget"]["plain"], "{\n  // @integer @minimum 1\n  n?: number;\n}",
        "{v}"
    );
    assert_eq!(
        v["budget"]["untagged"], "{ n?: number; }",
        "over budget renders again without tags: {v}"
    );
    assert_eq!(
        v["budget"]["unknown"], "unknown",
        "still over budget renders as unknown: {v}"
    );

    // D25: MCP shapes are gone; output schemas render as their own types.
    assert_eq!(
        v["signature"], "hidden_dynamic_tool(args: { city: string; }): Promise<{ ok: boolean; }>;",
        "{v}"
    );
    assert_eq!(
        v["freeSignature"], "free(args: unknown): Promise<unknown>;",
        "{v}"
    );
    assert_eq!(
        v["mcpShapedOutput"],
        "plain(args: { [key: string]: unknown; }): Promise<{ _meta?: { [key: string]: unknown; }; content: Array<{ [key: string]: unknown; }>; isError?: boolean; }>;",
        "{v}"
    );
    assert_eq!(
        v["mcpHelpersGone"],
        json!(["undefined", "undefined", "undefined"]),
        "MCP_TYPESCRIPT_PREAMBLE/mcpStructuredContentSchema are upstream-removed: {v}"
    );
    assert_eq!(
        v["sample"],
        "bar\n\ncodemode tool declaration:\n```ts\ndeclare const tools: { foo(args: string): Promise<unknown>; };\n```",
        "{v}"
    );
    assert_eq!(v["outputTypeMissing"], "unknown", "{v}");
    assert_eq!(
        v["declarations"],
        "declare const tools: {\n  /** d */\n  t(args: string): Promise<unknown>;\n};\n\ndeclare const ns: {\n  /** g */\n  g(args: string): Promise<unknown>;\n};",
        "{v}"
    );

    // Pure-JS surfaces and constants.
    assert_eq!(v["identifier"], "hidden_dynamic_tool", "{v}");
    assert_eq!(v["parsed"]["options"]["timeoutMs"], 500, "{v}");
    assert_eq!(v["parsed"]["code"], "\nreturn 1;", "{v}");
    assert_eq!(v["constants"][0], "// @options:", "{v}");
    assert_eq!(v["constants"][1], 16000, "{v}");
    assert_eq!(v["constants"][2], 100000, "{v}");
    assert_eq!(v["constants"][3], "string", "{v}");
    assert_eq!(v["sourceErrorIsType"], true, "{v}");
    assert!(
        v["sourceError"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("Expected JavaScript source text"),
        "{v}"
    );
    assert_eq!(
        v["misplacedOptions"][0]["message"],
        "The // @options: line must be the first line of the input; found one on line 2",
        "DRIFT D13: options past the first line must throw: {v}"
    );
    assert_eq!(
        v["misplacedOptions"][1]["message"],
        "The // @options: line must be the first line of the input; found one on line 3",
        "{v}"
    );
    assert_eq!(
        v["optionsPrefixVariant"]["options"],
        json!({}),
        "`// @optionsx:` is not the directive: {v}"
    );

    // Runtime tiers: real registries, and the sandbox actually runs on the
    // host engine (DRIFT §2.3 item 2 / D28).
    assert_eq!(v["base"]["tools"], json!(["alpha", "beta"]), "{v}");
    assert_eq!(v["base"]["globals"], json!(["ns.x"]), "{v}");
    assert_eq!(v["base"]["timeoutMs"], 1234, "{v}");
    assert_eq!(v["base"]["unregistered"], true, "{v}");
    assert_eq!(v["base"]["afterUnregister"], json!(["beta"]), "{v}");
    assert_eq!(
        v["base"]["duplicate"]["message"], "Tool \"beta\" is already registered",
        "{v}"
    );
    assert_eq!(
        v["base"]["run"],
        json!({ "ok": true, "value": 2, "output": [], "calls": [] }),
        "CodemodeSandboxBase.execute runs on the host engine: {v}"
    );
    assert_eq!(
        v["base"]["startValidation"]["message"],
        "startedAt must be a non-negative number of milliseconds since the epoch",
        "start() keeps upstream's synchronous validation: {v}"
    );
    assert_eq!(v["closeFlipsFlag"], true, "{v}");
    assert_eq!(v["hostIsBase"], true, "{v}");
    assert_eq!(v["inlineIsBase"], true, "{v}");
    assert_eq!(v["remoteIsBase"], true, "{v}");
    assert_eq!(v["hostExecute"], json!({ "ok": true, "value": 1 }), "{v}");
    assert_eq!(v["inlineExecute"], json!({ "ok": true, "value": 1 }), "{v}");
    // The remote faces keep the unified not-supported error (DRIFT §2.3).
    for (face, key) in [
        ("RemoteCodemodeSandbox.execute", "remoteExecute"),
        ("RemoteCodemodeSandbox.start", "remoteStart"),
    ] {
        let error = &v[key];
        assert_eq!(
            error["message"],
            format!("{face} is not supported by the pir extension runtime (codemode runs as a builtin tool on the host engine)"),
            "{v}"
        );
        assert_eq!(error["code"], "ERR_NOT_SUPPORTED", "{v}");
    }
    assert_eq!(
        v["serve"]["message"],
        "serveCodemodeRemote is not supported by the pir extension runtime (codemode runs as a builtin tool on the host engine)",
        "{v}"
    );
    assert_eq!(
        v["loadWasm"],
        "loadQuickJSWasm is not supported by the pir extension runtime (codemode runs as a builtin tool on the host engine)",
        "{v}"
    );
    assert!(
        v["durableObject"]["message"]
            .as_str()
            .unwrap_or_default()
            .starts_with("CodemodeSandboxDurableObject is not supported"),
        "{v}"
    );
    assert_eq!(v["wrangler"]["isInline"], true, "{v}");
    // `createInlineCodemodeSandbox` is real now (the host engine needs no wasm
    // module); only the `cloudflareQuickJSWasm` face keeps throwing.
    assert_eq!(
        v["wrangler"]["run"],
        json!({ "ok": true, "value": 1 }),
        "{v}"
    );
    assert!(
        v["wrangler"]["wasmAccess"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("cloudflareQuickJSWasm is not supported"),
        "{v}"
    );
    assert_eq!(v["constantsRuntime"], json!([100000, 33554432]), "{v}");

    // Every published subpath resolves to the same module.
    let subpaths = &v["subpaths"];
    assert_eq!(subpaths["portableInline"], true, "{v}");
    assert_eq!(subpaths["portableRemote"], true, "{v}");
    assert_eq!(subpaths["portableBase"], true, "{v}");
    assert_eq!(subpaths["portableServe"], true, "{v}");
    assert_eq!(subpaths["portableBudget"], 100000, "{v}");
    assert_eq!(subpaths["cloudflareDurable"], true, "{v}");
    assert_eq!(subpaths["cloudflareWasm"], true, "{v}");
    assert_eq!(subpaths["cloudflareInline"], true, "{v}");
    assert_eq!(subpaths["cloudflareMemory"], 33554432, "{v}");
    assert_eq!(subpaths["declarationsSignature"], true, "{v}");
    assert_eq!(subpaths["sourceParse"], true, "{v}");
    assert_eq!(subpaths["workerEntryKeys"], true, "{v}");
    assert_eq!(subpaths["defaultMirrorsNamed"], true, "{v}");

    let _ = std::fs::remove_dir_all(&dir);
}
