"""Final SDK gating pass: the env daemon's watch op, the no-engine
`ExtensionHandle::start`, and three small cleanups.

- env_daemon is core (its frame/decode protocol is shared with the remote-env
  client); only the `watch` op needs the platform watcher (`notify`), so it sits
  behind the `env` feature and fails honestly without it.
- Without the `extensions` feature there is no JS runtime to start: `start`
  compiles to an honest error, so hosts keep their existing call sites and
  surface the real reason instead of silently running without extensions.
"""

import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def read(rel: str) -> str:
    with open(os.path.join(ROOT, rel), encoding="utf-8", newline="") as fh:
        return fh.read()


def write(rel: str, text: str) -> None:
    with open(os.path.join(ROOT, rel), "w", encoding="utf-8", newline="") as fh:
        fh.write(text)


def patch(rel: str, pairs) -> None:
    text = read(rel)
    nl = "\r\n" if "\r\n" in text else "\n"
    for old, new in pairs:
        old_n = old if nl == "\n" else old.replace("\n", "\r\n")
        new_n = new if nl == "\n" else new.replace("\n", "\r\n")
        if new_n in text:
            continue
        if old_n not in text:
            print(f"MISSING anchor in {rel}:\n{old[:220]}", file=sys.stderr)
            sys.exit(1)
        text = text.replace(old_n, new_n, 1)
    write(rel, text)


START_STUB = '''    /// A build without the `extensions` feature has no embedded JS engine, so
    /// there is no runtime to start: the call fails with a clear reason (the
    /// host logs it) instead of running silently without extensions.
    #[cfg(not(feature = "extensions"))]
    pub fn start(config: ExtensionConfig) -> anyhow::Result<Self> {
        let _ = config;
        anyhow::bail!(
            "this build has no JS extension runtime (the `extensions` feature is off)"
        )
    }
'''

WATCH_FN = '''/// One raw-mode read (upstream waits for the first stdin `data` event);
/// silently skipped when raw mode is unavailable.
fn wait_for_any_keypress() {
    // Raw mode is a TUI facility: without the TUI the wait is compiled out
    // (the same path this takes when raw mode is unavailable).
    #[cfg(feature = "tui")]
    {
        use crossterm::event::read;
        if !std::io::stdin().is_terminal() {
            return;
        }
        if crossterm::terminal::enable_raw_mode().is_err() {
            return;
        }
        let _ = read();
        let _ = crossterm::terminal::disable_raw_mode();
    }
}
'''


def main() -> int:
    # 1. ExtensionHandle::start without the engine.
    mailbox = read("src/extensions/handle/mailbox.rs")
    if "no JS extension runtime" not in mailbox:
        anchor = '    #[cfg(feature = "extensions")]\n    pub fn start(config: ExtensionConfig) -> anyhow::Result<Self> {'
        if "\r\n" in mailbox:
            anchor = anchor.replace("\n", "\r\n")
        if anchor not in mailbox:
            print("start() anchor missing", file=sys.stderr)
            return 1
        stub = START_STUB if "\r\n" not in mailbox else START_STUB.replace("\n", "\r\n")
        mailbox = mailbox.replace(anchor, stub + anchor, 1)
        write("src/extensions/handle/mailbox.rs", mailbox)

    # 2. wait_for_any_keypress: the whole body belongs to the TUI.
    mig = read("src/config/migrations.rs")
    start = mig.index("/// One raw-mode read (upstream waits for the first stdin `data` event);")
    end = mig.index("\n}\n", start) + len("\n}\n")
    write("src/config/migrations.rs", mig[:start] + WATCH_FN + mig[end:])

    # 3. export_html: `variables` is only mutated with the TUI's theme colors.
    patch(
        "src/coding_agent/export_html.rs",
        [
            (
                "    let mut variables = String::new();\n    // The theme's export colors are a TUI surface",
                "    #[cfg(feature = \"tui\")]\n    let mut variables = String::new();\n    #[cfg(not(feature = \"tui\"))]\n    let variables = String::new();\n    // The theme's export colors are a TUI surface",
            )
        ],
    )

    # 4. the diff-preview helper is a TUI seam.
    patch(
        "src/coding_agent/tools/mod.rs",
        [
            (
                "pub(crate) use edit::compute_edits_diff;\n",
                "// The diff preview is read by the TUI's edit renderer only.\n#[cfg(feature = \"tui\")]\npub(crate) use edit::compute_edits_diff;\n",
            )
        ],
    )

    # 5. the daemon's watch op needs the platform watcher.
    patch(
        "src/env_daemon/mod.rs",
        [
            ("mod watch;\n", "#[cfg(feature = \"env\")]\nmod watch;\n"),
            (
                '            "watch" => plain(watch::run(id, json, &self.output, control)),\n',
                '            #[cfg(feature = "env")]\n            "watch" => plain(watch::run(id, json, &self.output, control)),\n'
                "            // No platform watcher without the `env` feature: answer with the\n"
                "            // wire's error frame rather than a silent no-op.\n"
                "            #[cfg(not(feature = \"env\"))]\n"
                '            "watch" => plain(Err(Failure::new(\n'
                '                "unsupported",\n'
                '                "this build has no env-daemon `watch` op (the `env` feature is off)",\n'
                '            ))),\n',
            ),
        ],
    )

    # 6. env_daemon is core now (lib.rs is CRLF).
    patch(
        "src/lib.rs",
        [
            (
                '#[cfg(feature = "env")]\npub mod env_daemon;\n',
                "// The daemon (and its frame/decode protocol, shared with the\n"
                "// remote-env client) is core; only its `watch` op needs the `env`\n"
                "// feature (the platform watcher, `notify`).\n"
                "pub mod env_daemon;\n",
            )
        ],
    )

    print("final gates applied")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
