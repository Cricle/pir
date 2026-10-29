//! Phase 4f: `@earendil-works/pi-tui` full value-export surface mirror.
//!
//! Fixture extensions are seeded from source strings at runtime (the repo
//! hygiene gate forbids tracked `.ts` files), mirroring tests/pi_shims.rs.

use std::path::{Path, PathBuf};

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

/// See tests/extensions_runtime.rs: fixtures must be written at runtime.
fn write_temp_fixture(test_name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("pir-tui-shims-{}-{test_name}", std::process::id()));
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

async fn run_seeded(name: &str, fixture: &str) -> (PathBuf, ExtensionRuntime) {
    let dir = write_temp_fixture(name, &[]);
    let rt = new_runtime_at(&dir).await;
    let module = format!("pir-ext:{name}.ts");
    rt.seed_module(&module, fixture).expect("seed fixture");
    let loaded = rt.seeded_load(&module).await;
    assert_eq!(loaded.expect("seeded load"), 1, "fixture must load");

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt
        .emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    assert!(
        outcome.errors.is_empty(),
        "handler errors in {name}: {:?}",
        outcome.errors
    );
    (dir, rt)
}

// ---------------------------------------------------------------------------
// 1. Coverage: every missing name from the 74-name surface regression links,
//    in both the named exports and the default export object.
// ---------------------------------------------------------------------------

const TUI_SURFACE_TS: &str = r##"
import * as TuiModule from "@earendil-works/pi-tui";
import TuiDefault from "@earendil-works/pi-tui";

// The pi-gaps-tui.txt missing list (74 names), embedded as the regression const.
const EXPECTED_NAMES: string[] = [
  "CURSOR_MARKER", "CancellableLoader", "CombinedAutocompleteProvider", "Editor",
  "HStack", "Image", "KeybindingsManager", "Loader", "Marked", "MouseRegion",
  "ProcessTerminal", "ScrollView", "SettingsList", "StdinBuffer", "TUI_KEYBINDINGS",
  "TruncatedText", "TuiAltScreen", "TuiMainScreen", "VStack",
  "allocateImageId", "backgroundAnsi", "calculateImageRows", "colorToHex",
  "colorToOkhsl", "colorToOklch", "colorToRgb", "compositeTuiLine",
  "decodeKittyPrintable", "deleteAllKittyImages", "deleteKittyImage",
  "detectCapabilities", "encodeITerm2", "encodeKitty", "foregroundAnsi",
  "formatProgramStatus", "getCapabilities", "getCellDimensions", "getGifDimensions",
  "getImageDimensions", "getJpegDimensions", "getKeybindings", "getNativeClipboard",
  "getOsc8LinkAtColumn", "getPngDimensions", "getTerminalColorMode",
  "getWebpDimensions", "hyperlink", "imageFallback", "indexedColor",
  "isAppleTerminalSession", "isFocusable", "isKeyRepeat", "isViewportTUI",
  "mixColors", "okhslColor", "oklabToOkhslLightness", "oklchColor", "parseColor",
  "parseKey", "parseTerminalColorSchemeReport", "renderFakeCursor", "renderImage",
  "renderLatex", "resetCapabilitiesCache", "rgbColor", "setCapabilities",
  "setCapabilityOverrides", "setCellDimensions", "setImageTranscoder",
  "setKeybindings", "sliceByColumn", "stripTerminalSequences", "styleText",
  "styleTextWithAnsi",
];

export default (pi: any) => {
  pi.on("session_start", async () => {
    const missing: string[] = [];
    const notInDefault: string[] = [];
    for (const name of EXPECTED_NAMES) {
      if ((TuiModule as any)[name] === undefined) missing.push(name);
      if ((TuiDefault as any)[name] === undefined) notInDefault.push(name);
    }
    globalThis.__tuiCoverage = {
      count: EXPECTED_NAMES.length,
      missing,
      notInDefault,
      defaultIsObject: typeof TuiDefault === "object" && TuiDefault !== null,
      // The pre-existing phase-4c face must keep linking too.
      legacyFace: [
        typeof TuiModule.Key === "object",
        typeof TuiModule.matchesKey === "function",
        typeof TuiModule.Text === "function",
        typeof TuiModule.Markdown === "function",
      ],
      tuiKeybindingsCount: Object.keys(TuiModule.TUI_KEYBINDINGS).length,
    };
  });
};
"##;

#[tokio::test(flavor = "current_thread")]
async fn tui_shims_surface_coverage() {
    let (dir, rt) = run_seeded("tui-coverage", TUI_SURFACE_TS).await;
    let v: Value = rt
        .eval_json("JSON.stringify(globalThis.__tuiCoverage)")
        .await
        .expect("read coverage results");

    assert_eq!(
        v["count"], 74,
        "the embedded gap list must have 74 names: {v}"
    );
    assert_eq!(v["missing"], json!([]), "missing named exports: {v}");
    assert_eq!(
        v["notInDefault"],
        json!([]),
        "names absent from default export: {v}"
    );
    assert_eq!(v["defaultIsObject"], true, "{v}");
    assert_eq!(v["legacyFace"], json!([true, true, true, true]), "{v}");
    assert_eq!(v["tuiKeybindingsCount"], 47, "default key table size: {v}");

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 2. Behaviour probes: real helpers must behave like the upstream ports.
// ---------------------------------------------------------------------------

const TUI_BEHAVIOUR_TS: &str = r##"
import {
  CURSOR_MARKER, FAKE_CURSOR_START, FAKE_CURSOR_END, renderFakeCursor, isFocusable, isViewportTUI,
  visibleWidth, truncateToWidth, stripTerminalSequences, sliceByColumn, getOsc8LinkAtColumn,
  compositeTuiLine,
  indexedColor, rgbColor, colorToRgb, colorToHex, colorToOkhsl, colorToOklch, parseColor,
  mixColors, foregroundAnsi, styleTextWithAnsi, oklabToOkhslLightness,
  getPngDimensions, getGifDimensions, getImageDimensions, getJpegDimensions, getWebpDimensions,
  calculateImageRows, getCellDimensions, setCellDimensions, allocateImageId,
  encodeKitty, encodeITerm2, deleteKittyImage, deleteAllKittyImages,
  detectCapabilities, getCapabilities, setCapabilities, resetCapabilitiesCache,
  getTerminalColorMode, renderImage, hyperlink, imageFallback,
  TUI_KEYBINDINGS, KeybindingsManager, getKeybindings,
  parseKey, decodeKittyPrintable, isKeyRepeat,
  formatProgramStatus, parseTerminalColorSchemeReport,
  StdinBuffer, VStack, HStack, Text, TruncatedText, Loader, CancellableLoader, ScrollView,
  Editor, SettingsList, Marked, renderLatex, CombinedAutocompleteProvider,
  ProcessTerminal, TuiAltScreen, TuiMainScreen, Image, MouseRegion, setImageTranscoder,
} from "@earendil-works/pi-tui";

// 1x1 red PNG and 1x1 GIF payloads (well-known fixtures).
const PNG_1X1 =
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
const GIF_1X1 = "R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7";

class Piece {
  text: string;
  constructor(text: string) {
    this.text = text;
  }
  render(width: number): string[] {
    return this.text.split("\n").map((line: string) => line.slice(0, width));
  }
  invalidate(): void {}
}

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};

    // --- P1 colors ---
    out.colors = {
      indexedRed: colorToHex(indexedColor(196)),
      parsedShortHex: colorToHex(parseColor("#abc")),
      rgbRoundtrip: colorToRgb(rgbColor(18, 52, 86)),
      srgbMix: colorToHex(mixColors(rgbColor(0, 0, 0), rgbColor(255, 255, 255), 0.5, "srgb")),
      okhslRed: colorToOkhsl(rgbColor(255, 0, 0)),
      oklchRedL: colorToOklch(rgbColor(255, 0, 0)).l,
      oklabLightness: oklabToOkhslLightness(0.628),
      fgIndexed: foregroundAnsi(indexedColor(196), "truecolor"),
      fgTruecolor: foregroundAnsi(rgbColor(255, 0, 0), "truecolor"),
      styled: styleTextWithAnsi("x", "\u001b[31m", undefined, { bold: true }),
    };
    let parseColorThrew = false;
    try {
      parseColor("not-a-color");
    } catch {
      parseColorThrew = true;
    }
    out.colors.parseColorThrew = parseColorThrew;

    // --- P2 image header parsing ---
    out.headers = {
      png: getPngDimensions(PNG_1X1),
      gif: getGifDimensions(GIF_1X1),
      pngViaMime: getImageDimensions(PNG_1X1, "image/png"),
      jpegOnPng: getJpegDimensions(PNG_1X1),
      webpOnJunk: getWebpDimensions("QUJD"),
      unknownMime: getImageDimensions(PNG_1X1, "image/tiff"),
    };

    // --- P3 width/truncate/slice ---
    const styledText = "\u001b[31mhello world\u001b[0m";
    const truncated = truncateToWidth(styledText, 8);
    out.width = {
      truncatedWidth: visibleWidth(truncated),
      truncatedVisible: stripTerminalSequences(truncated),
      truncatedKeepsStyle: truncated.startsWith("\u001b[31m"),
      sliceAnsi: sliceByColumn("\u001b[31mhello\u001b[0m world", 1, 3),
      cursorMarkerWidth: visibleWidth(CURSOR_MARKER),
      fakeCursor: renderFakeCursor("x") === FAKE_CURSOR_START + "x" + FAKE_CURSOR_END,
    };

    // --- P4 compositing and OSC 8 ---
    const osc8Line = "\u001b]8;;https://x.dev\u001b\\see\u001b]8;;\u001b\\ here";
    out.compose = {
      left: stripTerminalSequences(compositeTuiLine("abcdefghij", "XY", 3, 2, 10)),
      wide: stripTerminalSequences(compositeTuiLine("ab", "WXYZ", 1, 4, 8)),
      stripped: stripTerminalSequences("a\u001b[31mb\u001b[0mc"),
      link: getOsc8LinkAtColumn(osc8Line, 2),
      noLink: getOsc8LinkAtColumn("plain", 1) === undefined,
      linkOpens: hyperlink("t", "https://x.dev"),
    };

    // --- P5 VStack / HStack ---
    const vstack = new VStack([new Piece("aaa"), new Piece("bbb")]);
    const vgap = new VStack([new Piece("aaa"), new Piece("bbb")], { gap: 1 });
    const hstack = new HStack([new Piece("a\nb"), new Piece("c")], { gap: 1 });
    const hLines = hstack.render(8);
    out.stacks = {
      v: vstack.render(10),
      vgap: vgap.render(5),
      h0: stripTerminalSequences(hLines[0]),
      h1: stripTerminalSequences(hLines[1]),
      hCount: hLines.length,
    };

    // --- P6 keybindings ---
    const kb = new KeybindingsManager(TUI_KEYBINDINGS);
    const kbOverride = new KeybindingsManager(TUI_KEYBINDINGS, { "tui.select.confirm": "space" });
    out.keybindings = {
      enterConfirm: kb.matches("\r", "tui.select.confirm"),
      escCancel: kb.matches("\u001b", "tui.select.cancel"),
      upSelect: kb.matches("\u001b[A", "tui.select.up"),
      shiftTabNotCancel: kb.matches("\u001b[Z", "tui.select.cancel"),
      cancelKeys: kb.getKeys("tui.select.cancel"),
      overrideSpace: kbOverride.matches(" ", "tui.select.confirm"),
      overrideEnter: kbOverride.matches("\r", "tui.select.confirm"),
      globalGet: getKeybindings().matches("\r", "tui.input.submit"),
      cursorUpDefault: TUI_KEYBINDINGS["tui.editor.cursorUp"].defaultKeys,
      definition: kb.getDefinition("tui.select.confirm"),
    };

    // --- P7 image rows / cell math ---
    const idsOk = Array.from({ length: 20 }, () => allocateImageId()).every(
      (id: number) => Number.isInteger(id) && id >= 1 && id <= 0xffffffff,
    );
    const defaultCells = getCellDimensions();
    setCellDimensions({ widthPx: 10, heightPx: 20 });
    const customCells = getCellDimensions();
    setCellDimensions({ widthPx: 9, heightPx: 18 });
    out.imageMath = {
      rows100: calculateImageRows({ widthPx: 100, heightPx: 100 }, 20),
      rows1x1: calculateImageRows({ widthPx: 1, heightPx: 1 }, 10),
      defaultCells,
      customCellWidth: customCells.widthPx,
      allocateOk: idsOk,
    };

    // --- P8 kitty/iterm2 encoding ---
    const big = "A".repeat(5000);
    const chunked = encodeKitty(big, {});
    out.encoding = {
      small: encodeKitty("AAAA", { columns: 2, rows: 1 }),
      chunkedHasM1: chunked.includes(",m=1;"),
      chunkedHasM0: chunked.includes("m=0;"),
      chunkStartsKitty: chunked.startsWith("\u001b_G"),
      lf: encodeITerm2("AAAA", { width: 2 }),
      del: deleteKittyImage(7),
      delAll: deleteAllKittyImages(),
    };

    // --- P9 StdinBuffer ---
    const sb = new StdinBuffer();
    const events: string[] = [];
    sb.on("data", (d: string) => events.push(d));
    sb.process("h");
    sb.process("\u001b[");
    sb.process("A");
    const sbPaste = new StdinBuffer();
    const pasteEvents: string[] = [];
    sbPaste.on("paste", (p: string) => pasteEvents.push(p));
    sbPaste.process("\u001b[200~hello\r\nworld\u001b[201~");
    const sbFlush = new StdinBuffer();
    sbFlush.process("\u001b[");
    const flushed = sbFlush.flush();
    out.stdinBuffer = { events, pasteEvents, flushed, bufferAfter: sbFlush.getBuffer() };

    // --- P10 keys / program status / terminal colors ---
    out.keys = {
      legacyUp: parseKey("\u001b[A"),
      rawCtrlA: parseKey("\x01"),
      kittyCtrlA: parseKey("\u001b[97;5u"),
      kittyShiftA: parseKey("\u001b[97:65;2u"),
      printable: decodeKittyPrintable("\u001b[97u"),
      printableShift: decodeKittyPrintable("\u001b[97:65;2u"),
      printableCtrl: decodeKittyPrintable("\u001b[97;5u") === undefined,
      repeat: isKeyRepeat("\u001b[97;5:2u"),
      repeatLegacy: isKeyRepeat("\u001b[A"),
    };
    out.programStatus = {
      blocked: formatProgramStatus({ state: "blocked", kind: "permission", message: "Allow?" }),
      working: formatProgramStatus({ state: "working", app: "pir", message: "hi" }),
      invalidApp: formatProgramStatus({ state: "working", app: "bad app!" }),
      schemeLight: parseTerminalColorSchemeReport("\u001b[?997;2n"),
      schemeDark: parseTerminalColorSchemeReport("\u001b[?997;1n"),
      schemeNone: parseTerminalColorSchemeReport("plain") === undefined,
      marker: CURSOR_MARKER,
    };

    // --- P11 capabilities / renderImage ---
    resetCapabilitiesCache();
    const detected = detectCapabilities(() => false);
    setCapabilities({ images: "kitty", trueColor: true, hyperlinks: true });
    const kittyRender = renderImage("AAAA", { widthPx: 1, heightPx: 1 }, { maxWidthCells: 4 });
    const modeAfterSet = getTerminalColorMode();
    setCapabilities({ images: null, trueColor: false, hyperlinks: false });
    const noImageRender = renderImage("AAAA", { widthPx: 1, heightPx: 1 }, {});
    const fallback = imageFallback("image/png", { widthPx: 1, heightPx: 1 });
    resetCapabilitiesCache();
    out.capabilities = {
      detectedShape:
        typeof detected.images !== "undefined" &&
        typeof detected.trueColor === "boolean" &&
        typeof detected.hyperlinks === "boolean",
      kittyPrefix: kittyRender !== null && kittyRender!.sequence.startsWith("\u001b_Ga=T,f=100,q=2"),
      kittyColumns: kittyRender !== null ? kittyRender!.columns : -1,
      modeAfterSet,
      noImageRenderIsNull: noImageRender === null,
      fallback,
    };

    // --- P12 components ---
    const fakeTui = { terminal: { rows: 24, columns: 80 }, requestRender() {} } as any;

    const editor = new Editor(fakeTui, { borderColor: (s: string) => s, selectList: {} });
    editor.setText("hello");
    let editorChanges = 0;
    let submitted: string | null = null;
    editor.onChange = () => { editorChanges++; };
    editor.onSubmit = (value: string) => { submitted = value; };
    const editorLines = editor.render(10);
    editor.handleInput("!");
    const afterBang = editor.getText();
    editor.handleInput("\r");
    const afterSubmit = editor.getText();
    const editorChangesAfterSubmit = editorChanges;
    editor.setText("x");
    editor.handleInput("\n");
    editor.handleInput("y");
    const multiLine = editor.getText();
    editor.setText("");
    editor.addToHistory("saved");
    editor.handleInput("\u001b[A");
    const historyText = editor.getText();

    const loader = new Loader(fakeTui, (s: string) => s, (s: string) => s, "Loading...");
    loader.start();
    const loaderLines = loader.render(30).map((line: string) => stripTerminalSequences(line));
    loader.stop();

    const cancellable = new CancellableLoader(fakeTui, (s: string) => s, (s: string) => s, "Work...");
    let aborted = false;
    cancellable.onAbort = () => { aborted = true; };
    cancellable.handleInput("\u001b");
    const signalAborted = cancellable.signal.aborted;
    cancellable.stop();

    const settingsTheme = {
      label: (t: string) => t,
      value: (t: string) => t,
      description: (t: string) => t,
      cursor: "> ",
      hint: (t: string) => t,
    };
    const settings = new SettingsList(
      [
        { id: "a", label: "Alpha", currentValue: "on", values: ["on", "off"] },
        { id: "b", label: "Beta", currentValue: "x" },
      ],
      5,
      settingsTheme,
      () => {},
      () => {},
    );
    const settingsLines = settings.render(30).map((line: string) => stripTerminalSequences(line));

    const scrollView = new ScrollView(new Piece("content"));
    scrollView.updateLayout(50, 10, () => {});
    scrollView.scrollTo(30);
    scrollView.scrollToEnd();

    out.components = {
      editorLineCount: editorLines.length,
      editorTopBorder: editorLines[0],
      editorVisible: stripTerminalSequences(editorLines[1]).trimEnd(),
      afterBang,
      afterSubmit,
      submitted,
      editorChanges: editorChangesAfterSubmit,
      multiLine,
      historyText,
      loaderLines,
      loaderStops: true,
      cancellableAborted: aborted,
      cancellableSignal: signalAborted,
      settingsLines,
      truncated: new TruncatedText("hello world", 1, 1)
        .render(8)
        .map((line: string) => stripTerminalSequences(line)),
      markedText: new Marked().parse("# hi"),
      latexFrac: renderLatex("\\frac{1}{2}"),
      latexPi: renderLatex("\\pi"),
      latexSymbols: renderLatex("\\alpha + \\beta"),
      latexSuperscript: renderLatex("a^2"),
      latexPlain: renderLatex("x + 1"),
      latexEmpty: renderLatex(""),
      latexUnsupported: renderLatex("\\unknowncommand") === undefined,
      scrollTopAfterEnd: scrollView.scrollTop,
      editorFocusable: isFocusable(editor),
      textNotFocusable: isFocusable(new Text("x")),
      altViewport: isViewportTUI(new TuiAltScreen({})),
      mainNotViewport: isViewportTUI(new TuiMainScreen({})),
      altMode: new TuiAltScreen({}).mode,
      mainMode: new TuiMainScreen({}).mode,
    };

    // --- P13 autocomplete / image component / mouse region ---
    const provider = new CombinedAutocompleteProvider(
      [
        { name: "help", description: "Show help" },
        { name: "model", description: "Pick model" },
      ],
      "/tmp",
    );
    const suggestions = await provider.getSuggestions(["/he"], 0, 3, {});
    setCapabilities({ images: "kitty", trueColor: true, hyperlinks: true });
    setImageTranscoder((data: string) => data);
    const image = new Image(PNG_1X1, "image/png", { fallbackColor: (s: string) => s });
    const imageLines = image.render(40);
    setCapabilities({ images: null, trueColor: false, hyperlinks: false });
    image.invalidate();
    const imageFallbackLines = image.render(40);
    resetCapabilitiesCache();
    setImageTranscoder(undefined);

    const moused = new MouseRegion(new Piece("click"), (event: any) => ({ handled: true, x: event.x }));
    const mouseResult = moused.handleMouse({ x: 5 });

    out.misc = {
      suggestionNames: suggestions ? suggestions.items.map((item: any) => item.value) : null,
      suggestionPrefix: suggestions ? suggestions.prefix : null,
      applied: suggestions
        ? provider.applyCompletion(["/he"], 0, 3, suggestions.items[0], "/he")
        : null,
      imageLineCount: imageLines.length,
      imageUsesKitty: imageLines[0].startsWith("\u001b_G"),
      imageFallbackText: stripTerminalSequences(imageFallbackLines[0]),
      mouseResult,
      processTerminalDims: (() => {
        const terminal = new ProcessTerminal();
        return [terminal.kittyProtocolActive, typeof terminal.columns, typeof terminal.rows];
      })(),
      appleTerminal: (await import("@earendil-works/pi-tui")).isAppleTerminalSession(),
    };

    globalThis.__tuiProbes = out;
  });
};
"##;

#[tokio::test(flavor = "current_thread")]
async fn tui_shims_behaviour_probes() {
    let (dir, rt) = run_seeded("tui-behaviour", TUI_BEHAVIOUR_TS).await;
    let v: Value = rt
        .eval_json("JSON.stringify(globalThis.__tuiProbes)")
        .await
        .expect("read behaviour probes");

    // P1 colors (values verified against the upstream build).
    assert_eq!(v["colors"]["indexedRed"], "#ff0000", "{v}");
    assert_eq!(v["colors"]["parsedShortHex"], "#aabbcc", "{v}");
    assert_eq!(
        v["colors"]["rgbRoundtrip"],
        json!({ "r": 18, "g": 52, "b": 86 }),
        "{v}"
    );
    assert_eq!(v["colors"]["srgbMix"], "#808080", "{v}");
    assert_eq!(v["colors"]["fgIndexed"], "\u{1b}[38;5;196m", "{v}");
    assert_eq!(v["colors"]["fgTruecolor"], "\u{1b}[38;2;255;0;0m", "{v}");
    assert_eq!(
        v["colors"]["styled"], "\u{1b}[31m\u{1b}[1mx\u{1b}[22m\u{1b}[39m",
        "{v}"
    );
    assert!(
        (v["colors"]["okhslRed"]["l"].as_f64().unwrap() - 0.5680846563197032).abs() < 1e-6,
        "OKHSL red lightness: {v}"
    );
    assert!(
        (v["colors"]["okhslRed"]["h"].as_f64().unwrap() - 29.23388027962784).abs() < 1e-6,
        "OKHSL red hue: {v}"
    );
    assert!(
        (v["colors"]["oklabLightness"].as_f64().unwrap() - 0.5681361593754493).abs() < 1e-6,
        "oklabToOkhslLightness: {v}"
    );
    assert_eq!(v["colors"]["parseColorThrew"], true, "{v}");

    // P2 image headers.
    assert_eq!(
        v["headers"]["png"],
        json!({ "widthPx": 1, "heightPx": 1 }),
        "{v}"
    );
    assert_eq!(
        v["headers"]["gif"],
        json!({ "widthPx": 1, "heightPx": 1 }),
        "{v}"
    );
    assert_eq!(
        v["headers"]["pngViaMime"],
        json!({ "widthPx": 1, "heightPx": 1 }),
        "{v}"
    );
    assert_eq!(v["headers"]["jpegOnPng"], Value::Null, "{v}");
    assert_eq!(v["headers"]["webpOnJunk"], Value::Null, "{v}");
    assert_eq!(v["headers"]["unknownMime"], Value::Null, "{v}");

    // P3 width/truncate/slice.
    assert_eq!(v["width"]["truncatedWidth"], 8, "{v}");
    assert_eq!(v["width"]["truncatedVisible"], "hello...", "{v}");
    assert_eq!(v["width"]["truncatedKeepsStyle"], true, "{v}");
    assert_eq!(v["width"]["sliceAnsi"], "\u{1b}[31mell", "{v}");
    assert_eq!(v["width"]["cursorMarkerWidth"], 0, "{v}");
    assert_eq!(v["width"]["fakeCursor"], true, "{v}");

    // P4 compositing and OSC 8.
    assert_eq!(v["compose"]["left"], "abcXYfghij", "{v}");
    assert_eq!(v["compose"]["wide"], "aWXYZ   ", "{v}");
    assert_eq!(v["compose"]["stripped"], "abc", "{v}");
    assert_eq!(v["compose"]["link"], "https://x.dev", "{v}");
    assert_eq!(v["compose"]["noLink"], true, "{v}");
    assert_eq!(
        v["compose"]["linkOpens"], "\u{1b}]8;;https://x.dev\u{1b}\\t\u{1b}]8;;\u{1b}\\",
        "{v}"
    );

    // P5 stacks (composited rows compared by visible content).
    assert_eq!(v["stacks"]["v"], json!(["aaa", "bbb"]), "{v}");
    assert_eq!(v["stacks"]["vgap"], json!(["aaa", "", "bbb"]), "{v}");
    assert_eq!(v["stacks"]["h0"], "a c     ", "{v}");
    assert_eq!(v["stacks"]["h1"], "b       ", "{v}");
    assert_eq!(v["stacks"]["hCount"], 2, "{v}");

    // P6 keybindings.
    assert_eq!(v["keybindings"]["enterConfirm"], true, "{v}");
    assert_eq!(v["keybindings"]["escCancel"], true, "{v}");
    assert_eq!(v["keybindings"]["upSelect"], true, "{v}");
    assert_eq!(v["keybindings"]["shiftTabNotCancel"], false, "{v}");
    assert_eq!(
        v["keybindings"]["cancelKeys"],
        json!(["escape", "ctrl+c"]),
        "{v}"
    );
    assert_eq!(v["keybindings"]["overrideSpace"], true, "{v}");
    assert_eq!(v["keybindings"]["overrideEnter"], false, "{v}");
    assert_eq!(v["keybindings"]["globalGet"], true, "{v}");
    assert_eq!(v["keybindings"]["cursorUpDefault"], "up", "{v}");
    assert_eq!(
        v["keybindings"]["definition"],
        json!({ "defaultKeys": "enter", "description": "Confirm selection" }),
        "{v}"
    );

    // P7 image math.
    assert_eq!(v["imageMath"]["rows100"], 10, "{v}");
    assert_eq!(v["imageMath"]["rows1x1"], 5, "{v}");
    assert_eq!(
        v["imageMath"]["defaultCells"],
        json!({ "widthPx": 9, "heightPx": 18 }),
        "{v}"
    );
    assert_eq!(v["imageMath"]["customCellWidth"], 10, "{v}");
    assert_eq!(v["imageMath"]["allocateOk"], true, "{v}");

    // P8 encoding.
    assert_eq!(
        v["encoding"]["small"], "\u{1b}_Ga=T,f=100,q=2,c=2,r=1;AAAA\u{1b}\\",
        "{v}"
    );
    assert_eq!(v["encoding"]["chunkedHasM1"], true, "{v}");
    assert_eq!(v["encoding"]["chunkedHasM0"], true, "{v}");
    assert_eq!(v["encoding"]["chunkStartsKitty"], true, "{v}");
    assert_eq!(
        v["encoding"]["lf"], "\u{1b}]1337;File=inline=1;size=3;width=2:AAAA\u{7}",
        "{v}"
    );
    assert_eq!(
        v["encoding"]["del"], "\u{1b}_Ga=d,d=I,i=7,q=2\u{1b}\\",
        "{v}"
    );
    assert_eq!(
        v["encoding"]["delAll"], "\u{1b}_Ga=d,d=A,q=2\u{1b}\\",
        "{v}"
    );

    // P9 StdinBuffer: complete sequences split across chunks re-assemble.
    assert_eq!(v["stdinBuffer"]["events"], json!(["h", "\u{1b}[A"]), "{v}");
    assert_eq!(
        v["stdinBuffer"]["pasteEvents"],
        json!(["hello\r\nworld"]),
        "{v}"
    );
    assert_eq!(v["stdinBuffer"]["flushed"], json!(["\u{1b}["]), "{v}");
    assert_eq!(v["stdinBuffer"]["bufferAfter"], "", "{v}");

    // P10 keys / program status / terminal colors.
    assert_eq!(v["keys"]["legacyUp"], "up", "{v}");
    assert_eq!(v["keys"]["rawCtrlA"], "ctrl+a", "{v}");
    assert_eq!(v["keys"]["kittyCtrlA"], "ctrl+a", "{v}");
    assert_eq!(v["keys"]["kittyShiftA"], "shift+a", "{v}");
    assert_eq!(v["keys"]["printable"], "a", "{v}");
    assert_eq!(v["keys"]["printableShift"], "A", "{v}");
    assert_eq!(v["keys"]["printableCtrl"], true, "{v}");
    assert_eq!(v["keys"]["repeat"], true, "{v}");
    assert_eq!(v["keys"]["repeatLegacy"], false, "{v}");
    assert_eq!(
        v["programStatus"]["blocked"],
        "\u{1b}]7501;state=blocked:kind=permission:msg=QWxsb3c/\u{1b}\\",
        "{v}"
    );
    assert_eq!(
        v["programStatus"]["working"], "\u{1b}]7501;state=working:app=pir:msg=aGk=\u{1b}\\",
        "{v}"
    );
    assert_eq!(
        v["programStatus"]["invalidApp"], "\u{1b}]7501;state=working\u{1b}\\",
        "{v}"
    );
    assert_eq!(v["programStatus"]["schemeLight"], "light", "{v}");
    assert_eq!(v["programStatus"]["schemeDark"], "dark", "{v}");
    assert_eq!(v["programStatus"]["schemeNone"], true, "{v}");
    assert_eq!(v["programStatus"]["marker"], "\u{1b}_pi:c\u{7}", "{v}");

    // P11 capabilities / renderImage.
    assert_eq!(v["capabilities"]["detectedShape"], true, "{v}");
    assert_eq!(v["capabilities"]["kittyPrefix"], true, "{v}");
    assert_eq!(v["capabilities"]["kittyColumns"], 4, "{v}");
    assert_eq!(v["capabilities"]["modeAfterSet"], "truecolor", "{v}");
    assert_eq!(v["capabilities"]["noImageRenderIsNull"], true, "{v}");
    assert_eq!(
        v["capabilities"]["fallback"], "[Image: [image/png] 1x1]",
        "{v}"
    );

    // P12 components.
    assert_eq!(
        v["components"]["editorLineCount"], 3,
        "top border + line + bottom border: {v}"
    );
    assert_eq!(v["components"]["editorTopBorder"], "──────────", "{v}");
    assert_eq!(v["components"]["editorVisible"], "hello", "{v}");
    assert_eq!(v["components"]["afterBang"], "hello!", "{v}");
    assert_eq!(v["components"]["afterSubmit"], "", "{v}");
    assert_eq!(v["components"]["submitted"], "hello!", "{v}");
    assert_eq!(
        v["components"]["editorChanges"], 2,
        "typing + submit reset: {v}"
    );
    assert_eq!(v["components"]["multiLine"], "x\ny", "{v}");
    assert_eq!(v["components"]["historyText"], "saved", "{v}");
    assert_eq!(
        v["components"]["loaderLines"],
        json!(["", " ⠋ Loading...                 "]),
        "{v}"
    );
    assert_eq!(v["components"]["cancellableAborted"], true, "{v}");
    assert_eq!(v["components"]["cancellableSignal"], true, "{v}");
    assert_eq!(
        v["components"]["settingsLines"],
        json!([
            "> Alpha  on",
            "  Beta   x",
            "",
            "  Enter/Space to change · E..."
        ]),
        "{v}"
    );
    assert_eq!(
        v["components"]["truncated"],
        json!(["        ", " hel... ", "        "]),
        "{v}"
    );
    let marked = v["components"]["markedText"].as_str().unwrap_or_default();
    assert!(
        marked.contains("hi"),
        "Marked.parse must render the heading text: {v}"
    );
    assert_eq!(v["components"]["latexFrac"], "1/2", "{v}");
    assert_eq!(v["components"]["latexPi"], "π", "{v}");
    assert_eq!(v["components"]["latexSymbols"], "α + β", "{v}");
    assert_eq!(v["components"]["latexSuperscript"], "a²", "{v}");
    assert_eq!(v["components"]["latexPlain"], "x + 1", "{v}");
    assert_eq!(v["components"]["latexEmpty"], "", "{v}");
    assert_eq!(v["components"]["latexUnsupported"], true, "{v}");
    assert_eq!(
        v["components"]["scrollTopAfterEnd"], 40,
        "50 content - 10 viewport: {v}"
    );
    assert_eq!(v["components"]["editorFocusable"], true, "{v}");
    assert_eq!(v["components"]["textNotFocusable"], false, "{v}");
    assert_eq!(v["components"]["altViewport"], true, "{v}");
    assert_eq!(v["components"]["mainNotViewport"], false, "{v}");
    assert_eq!(v["components"]["altMode"], "fullscreen", "{v}");
    assert_eq!(v["components"]["mainMode"], "regular", "{v}");

    // P13 autocomplete / image component / mouse region.
    assert_eq!(v["misc"]["suggestionNames"], json!(["help"]), "{v}");
    assert_eq!(v["misc"]["suggestionPrefix"], "/he", "{v}");
    assert_eq!(
        v["misc"]["applied"],
        json!({ "lines": ["/help "], "cursorLine": 0, "cursorCol": 6 }),
        "{v}"
    );
    assert!(
        v["misc"]["imageLineCount"].as_u64().unwrap() >= 2,
        "kitty image reserves its rows: {v}"
    );
    assert_eq!(v["misc"]["imageUsesKitty"], true, "{v}");
    assert_eq!(
        v["misc"]["imageFallbackText"], "[Image: [image/png] 1x1]",
        "{v}"
    );
    assert_eq!(
        v["misc"]["mouseResult"],
        json!({ "handled": true, "x": 5 }),
        "{v}"
    );
    assert_eq!(
        v["misc"]["processTerminalDims"],
        json!([false, "number", "number"]),
        "{v}"
    );
    assert_eq!(
        v["misc"]["appleTerminal"], false,
        "test host is not Apple Terminal: {v}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 3a. Environment detection (DRIFT D21): a Herdr session reports the Kitty
//     graphics protocol, matching the Rust probe in src/tui/terminal_image.rs.
// ---------------------------------------------------------------------------

const TUI_HERDR_TS: &str = r##"
import { detectCapabilities } from "@earendil-works/pi-tui";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const env = process.env;
    // Isolate the probe from any ambient terminal on the test host (the
    // runtime's process.env is a snapshot, so this never touches the OS env).
    for (const name of [
      "TMUX", "KITTY_WINDOW_ID", "GHOSTTY_RESOURCES_DIR", "WEZTERM_PANE",
      "WARP_SESSION_ID", "WARP_TERMINAL_SESSION_UUID", "ITERM_SESSION_ID",
      "WT_SESSION", "TERMINAL_EMULATOR", "PI_IMAGE_PROTOCOL", "PI_TRUE_COLOR",
      "PI_HYPERLINKS",
    ]) {
      delete (env as any)[name];
    }
    env.TERM = "xterm-256color";
    env.COLORTERM = "truecolor";
    env.TERM_PROGRAM = "herdr";
    globalThis.__tuiHerdr = detectCapabilities(() => false);

    // COLORTERM drives only trueColor; the image protocol and hyperlinks are
    // fixed by the Herdr branch.
    delete (env as any).COLORTERM;
    globalThis.__tuiHerdrNoColorTerm = detectCapabilities(() => false);
  });
};
"##;

#[tokio::test(flavor = "current_thread")]
async fn tui_shims_herdr_reports_kitty_images() {
    let (dir, rt) = run_seeded("tui-herdr", TUI_HERDR_TS).await;
    let v: Value = rt
        .eval_json("JSON.stringify(globalThis.__tuiHerdr)")
        .await
        .expect("read herdr capabilities");

    assert_eq!(
        v,
        json!({ "images": "kitty", "trueColor": true, "hyperlinks": true }),
        "a TERM_PROGRAM=herdr session reports the Kitty graphics protocol: {v}"
    );

    let no_color_term: Value = rt
        .eval_json("JSON.stringify(globalThis.__tuiHerdrNoColorTerm)")
        .await
        .expect("read herdr capabilities without COLORTERM");
    assert_eq!(
        no_color_term,
        json!({ "images": "kitty", "trueColor": false, "hyperlinks": true }),
        "herdr keeps the trueColor hint while always reporting kitty images: {no_color_term}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 3. Throw probes: the documented-unavailable faces fail loudly.
// ---------------------------------------------------------------------------

const TUI_THROW_TS: &str = r##"
import { getNativeClipboard, ScrollView, Text } from "@earendil-works/pi-tui";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const errors: string[] = [];
    try {
      getNativeClipboard();
      errors.push("no-throw");
    } catch (error: any) {
      errors.push(String(error && error.message ? error.message : error));
    }
    const child = new Text("x", 0, 0);
    try {
      const scroll = new ScrollView(child);
      scroll.addChild(new Text("y", 0, 0));
      errors.push("no-throw");
    } catch (error: any) {
      errors.push(String(error && error.message ? error.message : error));
    }
    try {
      new ScrollView(child, { axis: "horizontal" as any });
      errors.push("no-throw");
    } catch (error: any) {
      errors.push(String(error && error.message ? error.message : error));
    }
    globalThis.__tuiThrows = errors;
  });
};
"##;

#[tokio::test(flavor = "current_thread")]
async fn tui_shims_throw_probes() {
    let (dir, rt) = run_seeded("tui-throws", TUI_THROW_TS).await;
    let v: Value = rt
        .eval_json("JSON.stringify(globalThis.__tuiThrows)")
        .await
        .expect("read throw probes");

    assert_eq!(
        v,
        json!([
            "getNativeClipboard is not available in the pir extension runtime (no native clipboard helper)",
            "ScrollView has exactly one child",
            "Unsupported ScrollView axis: horizontal"
        ]),
        "unavailable faces must throw the documented errors: {v}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
