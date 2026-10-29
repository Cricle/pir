//! Clipboard access for the TUI (paste support + `/copy`).
//!
//! Reading the clipboard is best-effort: any failure (no clipboard, no
//! display, locked by another process) degrades to `None` — the caller
//! inserts nothing and shows a status note instead. Never panics.
//!
//! Writing follows upstream `copyToClipboard` (coding-agent
//! `utils/clipboard.ts`): the local clipboard first, then — for a remote or
//! intermediated session — the same text as OSC 52 so the *client* terminal
//! receives it too (`PI_CLIPBOARD_OSC52=1` covers remote clients that leave
//! no SSH markers, such as multiplexer attaches or remote IDE daemons;
//! upstream `3e477eab5`).

use std::io::Write;
use std::time::{Duration, Instant};

/// The largest base64 payload that may be sent with OSC 52 (upstream
/// `MAX_OSC52_ENCODED_LENGTH`); anything longer is refused with a failure
/// instead of spraying megabytes at the terminal.
pub const MAX_OSC52_ENCODED_LENGTH: usize = 100_000;

/// Read plain text from the system clipboard, normalizing Windows line
/// endings. Returns `None` when the clipboard is unavailable or holds no
/// text.
pub fn read_text() -> Option<String> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    let text = clipboard.get_text().ok()?;
    let text = normalize_newlines(&text);
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Guarantees the delivery callback runs exactly once: the normal path takes
/// it, and `Drop` invokes it with `None` while the reader thread unwinds from
/// a panic. That is what lets the UI retire its pending-read slot on every
/// exit path instead of only when a successful `ClipboardText` event arrives.
struct DeliverGuard<F: FnOnce(Option<String>)> {
    deliver: Option<F>,
}

impl<F: FnOnce(Option<String>)> DeliverGuard<F> {
    fn new(deliver: F) -> Self {
        Self {
            deliver: Some(deliver),
        }
    }

    fn deliver(mut self, text: Option<String>) {
        if let Some(deliver) = self.deliver.take() {
            deliver(text);
        }
    }
}

impl<F: FnOnce(Option<String>)> Drop for DeliverGuard<F> {
    fn drop(&mut self) {
        if let Some(deliver) = self.deliver.take() {
            deliver(None);
        }
    }
}

/// Runs `on_exit` when the reader thread exits for any reason: normal return,
/// panic, or an early drop while unwinding.
#[doc(hidden)]
pub struct ReaderExitGuard<E: FnOnce()> {
    on_exit: Option<E>,
}

impl<E: FnOnce()> ReaderExitGuard<E> {
    #[doc(hidden)]
    pub fn new(on_exit: E) -> Self {
        Self {
            on_exit: Some(on_exit),
        }
    }
}

impl<E: FnOnce()> Drop for ReaderExitGuard<E> {
    fn drop(&mut self) {
        if let Some(on_exit) = self.on_exit.take() {
            on_exit();
        }
    }
}

/// The reader-thread body, kept separate so tests can drive a panicking
/// reader without touching the platform clipboard.
#[doc(hidden)]
pub fn run_read<R, F>(read: R, deliver: F)
where
    R: FnOnce() -> Option<String>,
    F: FnOnce(Option<String>),
{
    let deliver = DeliverGuard::new(deliver);
    let text = read();
    deliver.deliver(text);
}

/// Run [`read_text`] OFF the caller's thread and hand the result to
/// `deliver` there.
///
/// `arboard::Clipboard::new()` / `get_text()` talk to the platform clipboard
/// and block while another process holds it open (clipboard managers, remote
/// clipboard bridges, a terminal doing its own copy-on-select). On the TUI
/// event-loop thread that froze input and painting, so the read moves to a
/// short-lived named thread; the caller forwards the outcome through its own
/// event channel.
///
/// The callback is guaranteed to run exactly once: a panicking `read_text`
/// delivers `None` while the thread unwinds, so a caller that retires a
/// pending counter from the callback/event cannot leak it on that path.
///
/// Returns the spawn error (`deliver` is dropped in that case) so the caller
/// can report the same "unavailable" outcome without reading the clipboard
/// from its own thread.
pub fn spawn_read_text<F>(deliver: F) -> std::io::Result<()>
where
    F: FnOnce(Option<String>) + Send + 'static,
{
    spawn_read_text_with_exit(deliver, || {})
}

/// [`spawn_read_text`] plus an `on_exit` callback run by the reader thread on
/// every exit path, including a panic. Use this when the pending-read slot is
/// owned by the worker rather than retired by the UI event handler; the
/// callback also covers a failed event send, where the handler never runs.
///
/// Callers that already decrement in their `ClipboardText` handler should use
/// [`spawn_read_text`] so the slot is not retired twice.
pub fn spawn_read_text_with_exit<F, E>(deliver: F, on_exit: E) -> std::io::Result<()>
where
    F: FnOnce(Option<String>) + Send + 'static,
    E: FnOnce() + Send + 'static,
{
    std::thread::Builder::new()
        .name("pir-clipboard-read".to_owned())
        .spawn(move || {
            let _exit = ReaderExitGuard::new(on_exit);
            run_read(read_text, deliver);
        })
        .map(|_| ())
}

/// Process environment lookup used by the real [`write_text`].
fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Copy plain text to the system clipboard (the `/copy` slash command and the
/// fullscreen copy-on-select gesture).
///
/// Route order mirrors upstream `copyToClipboard` (`utils/clipboard.js:83-160`):
/// the native clipboard first, then — when that failed and no OSC 52 route
/// applied — the platform clipboard COMMAND, and OSC 52 alongside for remote
/// and headless sessions. Returns `true` when any route succeeded, so a
/// transient native-write failure (another process holding the Windows
/// clipboard open during the gesture: clipboard managers, IMEs, a terminal
/// doing its own copy-on-select) no longer reports "Copy failed" while the
/// text still reaches the clipboard through the command route. Never panics.
pub fn write_text(text: &str) -> bool {
    let mut out = std::io::stdout();
    if write_text_with(
        text,
        &write_local_text,
        &process_env,
        cfg!(target_os = "linux"),
        &mut out,
    ) {
        return true;
    }
    // Stage 2 (upstream `copyToClipboard`: `pbcopy` / `clip` / `wl-copy` /
    // `xclip` / `xsel`). Upstream runs it whenever the native write failed;
    // pir keeps the same order (native → OSC 52 → command).
    platform_clipboard_command(text)
}

/// [`write_text`] with the local writer, environment lookup, platform and
/// output sink injected, so tests can observe the OSC 52 emission without
/// touching the real clipboard or the terminal. `linux` mirrors upstream's
/// `platform() === "linux"` check (only Linux has the headless fallback).
///
/// This is the native + OSC 52 halves of [`write_text`]; the platform-command
/// stage lives in [`platform_clipboard_command`] and is not part of the
/// injected seam.
pub fn write_text_with(
    text: &str,
    local: &dyn Fn(&str) -> bool,
    env: &dyn Fn(&str) -> Option<String>,
    linux: bool,
    out: &mut dyn Write,
) -> bool {
    // Direct writes precede OSC 52 so the terminal cannot race the native
    // writer (upstream clipboard.ts:146-147).
    let copied = local(text);
    // OSC 52 cannot be verified, so a desktop session with a display reports
    // the failure instead; without a display the terminal is the only
    // clipboard route, and a remote session always emits it to reach the
    // client clipboard (upstream clipboard.ts:176-183).
    if (is_remote_session(env) || (!copied && headless_session(env, linux)))
        && emit_osc52(out, text)
    {
        return true;
    }
    copied
}

/// The platform-command stage of [`write_text`] (upstream
/// `utils/clipboard.js:96-121`). Windows takes the UTF-8-safe temp-file route
/// instead of `clip.exe`: `clip.exe` (and PowerShell reading piped stdin)
/// decode bytes with the console code page and mangle non-ASCII UTF-8 — the
/// exact reason upstream's WSL path writes a file and reads it back with
/// `Set-Clipboard` (`clipboard.js:28-59`).
fn platform_clipboard_command(text: &str) -> bool {
    match std::env::consts::OS {
        "windows" => windows_clipboard_temp_file(text),
        platform => stdin_clipboard_commands(platform, &process_env)
            .iter()
            .any(|(program, args)| run_clipboard_command(program, args, text)),
    }
}

/// The stage-2 commands that accept the text on STDIN, in upstream's order
/// (`copyToClipboard`: macOS `pbcopy`; Linux `wl-copy` / `xclip` / `xsel` —
/// gated on the same env markers upstream uses, so a headless or X11/Wayland
/// session only runs the tool that can work). Windows is intentionally absent;
/// see [`platform_clipboard_command`].
pub fn stdin_clipboard_commands(
    platform: &str,
    env: &dyn Fn(&str) -> Option<String>,
) -> Vec<(&'static str, Vec<&'static str>)> {
    let present = |name: &str| env(name).is_some_and(|value| !value.is_empty());
    let mut commands: Vec<(&'static str, Vec<&'static str>)> = Vec::new();
    match platform {
        "macos" => commands.push(("pbcopy", Vec::new())),
        "linux" => {
            if present("TERMUX_VERSION") {
                commands.push(("termux-clipboard-set", Vec::new()));
            }
            if present("WAYLAND_DISPLAY") {
                commands.push(("wl-copy", Vec::new()));
            }
            if present("DISPLAY") {
                commands.push(("xclip", vec!["-selection", "clipboard"]));
                commands.push(("xsel", vec!["--clipboard", "--input"]));
            }
        }
        _ => {}
    }
    commands
}

/// The PowerShell argument list for the Windows route: `Set-Clipboard` fed
/// from a UTF-8 file (upstream's WSL-safe mechanism, `clipboard.js:44-49`).
/// Pure so the script text is pinned by tests.
pub fn windows_set_clipboard_args(path: &str) -> Vec<String> {
    // PowerShell single-quoted strings escape `'` by doubling it (upstream
    // does the same for the WSL path it hands to PowerShell).
    let escaped = path.replace('\'', "''");
    vec![
        "-NoProfile".to_string(),
        "-Command".to_string(),
        format!("Set-Clipboard -Value ([System.IO.File]::ReadAllText('{escaped}', [System.Text.Encoding]::UTF8))"),
    ]
}

/// Windows: write the text to a UTF-8 temp file and let PowerShell's
/// `Set-Clipboard` copy it back (byte-exact for non-ASCII; the file is
/// removed either way).
#[cfg(windows)]
fn windows_clipboard_temp_file(text: &str) -> bool {
    use std::io::Write as _;
    let path = std::env::temp_dir().join(format!(
        "pir-clip-{}-{:x}.txt",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or(0)
    ));
    let written = std::fs::File::create(&path)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .is_ok();
    let copied = written
        && run_clipboard_command(
            "powershell.exe",
            &windows_set_clipboard_args(&path.to_string_lossy()),
            // No stdin text: the script reads the file itself.
            "",
        );
    let _ = std::fs::remove_file(&path);
    copied
}

/// Non-Windows build of the Windows-only route (never reached: the caller
/// switches on `std::env::consts::OS`).
#[cfg(not(windows))]
fn windows_clipboard_temp_file(_text: &str) -> bool {
    false
}

/// Run one clipboard command with `text` on its STDIN; success is the
/// process's zero exit status (upstream `runClipboardCommand` treats a
/// `undefined` result as failure, `clipboard-command.js`). Never panics.
fn run_clipboard_command(program: &str, args: &[impl AsRef<std::ffi::OsStr>], text: &str) -> bool {
    use std::process::Stdio;
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // CREATE_NO_WINDOW (0x08000000): the helper must not flash a console.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(0x0800_0000);
    }
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
        // Dropping stdin closes the pipe so the child sees EOF and exits.
    }
    child.wait().is_ok_and(|status| status.success())
}

/// The one local clipboard write (native platform clipboard).
fn write_local_text(text: &str) -> bool {
    match arboard::Clipboard::new() {
        Ok(mut clipboard) => clipboard.set_text(text.to_string()).is_ok(),
        Err(_) => false,
    }
}

/// Whether this session cannot reach the *user's* clipboard directly:
/// SSH/Mosh sessions where the terminal forwards the copy to the client, and
/// — forced with `PI_CLIPBOARD_OSC52=1` — remote clients that leave no SSH
/// markers, such as a multiplexer attached from another machine (#10114,
/// #10835) or an IDE daemon (#10826) (upstream `isRemoteSession`, verbatim
/// including the empty-string-is-unset rule and the exact `"1"` opt-in).
pub fn is_remote_session(env: &dyn Fn(&str) -> Option<String>) -> bool {
    let present = |name: &str| env(name).is_some_and(|value| !value.is_empty());
    present("SSH_CONNECTION")
        || present("SSH_CLIENT")
        || present("MOSH_CONNECTION")
        || env("PI_CLIPBOARD_OSC52").as_deref() == Some("1")
}

/// A Linux session with no display and no Termux clipboard app: the terminal
/// is the only clipboard route (containers, WSL without WSLg) — upstream
/// `headless` (clipboard.ts:178).
pub fn headless_session(env: &dyn Fn(&str) -> Option<String>, linux: bool) -> bool {
    let present = |name: &str| env(name).is_some_and(|value| !value.is_empty());
    linux && !present("DISPLAY") && !present("WAYLAND_DISPLAY") && !present("TERMUX_VERSION")
}

/// Emit `text` to the terminal with OSC 52 (upstream `emitOsc52`): the
/// base64 payload between the sequence header and a BEL terminator, byte for
/// byte upstream's `\x1b]52;c;<base64>\x07`. Returns `false` — writing
/// nothing — when the payload exceeds [`MAX_OSC52_ENCODED_LENGTH`] or the
/// terminal write fails.
pub fn emit_osc52(out: &mut dyn Write, text: &str) -> bool {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    if encoded.len() > MAX_OSC52_ENCODED_LENGTH {
        return false;
    }
    out.write_all(format!("\x1b]52;c;{encoded}\x07").as_bytes())
        .is_ok()
        && out.flush().is_ok()
}

/// Normalize `\r\n` and lone `\r` to `\n` so pasted text matches the
/// editor's line model.
pub fn normalize_newlines(text: &str) -> String {
    if !text.contains('\r') {
        return text.to_string();
    }
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// The exact error upstream raises when a copied file path carries a Unicode
/// control character (`\p{Cc}`): the paste must stop there instead of falling
/// through to an image or text read.
pub const CLIPBOARD_PATH_CONTROL_ERROR: &str = "Clipboard file path contains control characters";

/// The MIME types upstream considers natively supported (in preference order)
/// — `SUPPORTED_IMAGE_MIME_TYPES` in `utils/clipboard-image.ts`.
const SUPPORTED_IMAGE_MIME_TYPES: [&str; 4] =
    ["image/png", "image/jpeg", "image/webp", "image/gif"];

/// 1 s for type-list probes (`DEFAULT_LIST_TIMEOUT_MS`).
const CLIPBOARD_LIST_TIMEOUT: Duration = Duration::from_secs(1);
/// 3 s for the actual clipboard read command (`runClipboardCommand` default).
const CLIPBOARD_COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
/// 5 s for PowerShell/WSL (`DEFAULT_POWERSHELL_TIMEOUT_MS`).
const CLIPBOARD_POWERSHELL_TIMEOUT: Duration = Duration::from_secs(5);
/// 50 MiB stdout cap (`maxBufferBytes` in `clipboard-command.ts`).
pub const CLIPBOARD_STDOUT_CAP: usize = 50 * 1024 * 1024;

/// One successfully read clipboard image: encoded bytes plus its MIME type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardImage {
    pub bytes: Vec<u8>,
    pub mime_type: String,
}

/// A successful clipboard paste, in upstream `handleClipboardPaste`
/// precedence order: native file list, then image (as a temp-file path), then
/// plain text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardPaste {
    /// Native file-list paths (Finder file copies); inserted raw.
    FilePaths(Vec<String>),
    /// An image already written to a temp file; the path itself is inserted.
    ImagePath(String),
    /// Plain text — the existing fallback path.
    Text(String),
}

/// The lower-cased base MIME type with parameters stripped
/// (`image/PNG; charset=x` → `image/png`).
pub fn base_mime_type(mime_type: &str) -> String {
    mime_type
        .split(';')
        .next()
        .unwrap_or(mime_type)
        .trim()
        .to_lowercase()
}

/// File extension for a supported image MIME type (upstream
/// `extensionForImageMimeType`); `None` for anything else, whose caller falls
/// back to `png`.
pub fn extension_for_image_mime_type(mime_type: &str) -> Option<&'static str> {
    match base_mime_type(mime_type).as_str() {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/webp" => Some("webp"),
        "image/gif" => Some("gif"),
        _ => None,
    }
}

/// Whether the MIME type is one of the four formats that need no conversion.
pub fn is_supported_image_mime_type(mime_type: &str) -> bool {
    let base = base_mime_type(mime_type);
    SUPPORTED_IMAGE_MIME_TYPES.contains(&base.as_str())
}

/// Pick the preferred offered image type (upstream
/// `selectPreferredImageMimeType`): png, jpeg, webp, gif in that order, else
/// the first `image/*` candidate. The returned string is the trimmed RAW type
/// (parameters preserved), exactly what the clipboard tool must be asked for.
pub fn select_preferred_image_mime_type(mime_types: &[String]) -> Option<String> {
    let normalized: Vec<(String, String)> = mime_types
        .iter()
        .map(|mime_type| mime_type.trim())
        .filter(|mime_type| !mime_type.is_empty())
        .map(|mime_type| (mime_type.to_owned(), base_mime_type(mime_type)))
        .collect();
    for preferred in SUPPORTED_IMAGE_MIME_TYPES {
        if let Some((raw, _)) = normalized.iter().find(|(_, base)| base == preferred) {
            return Some(raw.clone());
        }
    }
    normalized
        .iter()
        .find(|(_, base)| base.starts_with("image/"))
        .map(|(raw, _)| raw.clone())
}

/// Shell-quote one path for bash mode (upstream `quoteIfNeeded`): values made
/// only of `[A-Za-z0-9_\-./~:@]` pass through, everything else — including
/// the empty string — is POSIX single-quoted with `'\''` escaping.
pub fn quote_if_needed(value: &str) -> String {
    let safe = !value.is_empty()
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, '_' | '-' | '.' | '/' | '~' | ':' | '@')
        });
    if safe {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

/// Join clipboard file paths for insertion: newline-separated in normal mode,
/// space-separated with bash quoting in `!` bash mode (upstream
/// `handleClipboardPaste`).
pub fn format_paths_for_insert(paths: &[String], bash_mode: bool) -> String {
    if bash_mode {
        paths
            .iter()
            .map(|path| quote_if_needed(path))
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        paths.join("\n")
    }
}

/// Add the upstream cursor-spacing around a raw path insertion: one leading
/// space when the character before the cursor is non-whitespace, one trailing
/// space when the character after the cursor is non-whitespace.
pub fn insert_with_cursor_spacing(text: &str, before: Option<char>, after: Option<char>) -> String {
    let leading = before.is_some_and(|character| !character.is_whitespace());
    let trailing = after.is_some_and(|character| !character.is_whitespace());
    let mut insert =
        String::with_capacity(text.len() + usize::from(leading) + usize::from(trailing));
    if leading {
        insert.push(' ');
    }
    insert.push_str(text);
    if trailing {
        insert.push(' ');
    }
    insert
}

/// Whether `text` contains a Unicode control character (general category
/// `Cc`, upstream `/\p{Cc}/u`).
pub fn contains_control_characters(text: &str) -> bool {
    text.chars().any(char::is_control)
}

/// Validate the native file list before any insertion: upstream throws on the
/// first path with a control character and never attempts image/text reads.
pub fn validate_clipboard_file_paths(paths: &[String]) -> Result<(), String> {
    if paths.iter().any(|path| contains_control_characters(path)) {
        Err(CLIPBOARD_PATH_CONTROL_ERROR.to_owned())
    } else {
        Ok(())
    }
}

/// Write clipboard image bytes to `std::env::temp_dir()/pi-clipboard-<uuid
/// v4>.<ext>` and return the path. The file is intentionally never deleted:
/// the user may keep referring to it in the prompt (upstream writes and
/// forgets the same way).
pub fn write_clipboard_temp_file(
    bytes: &[u8],
    extension: &str,
) -> std::io::Result<std::path::PathBuf> {
    let path = std::env::temp_dir().join(format!(
        "pi-clipboard-{}.{}",
        uuid::Uuid::new_v4(),
        extension
    ));
    let mut file = std::fs::File::create(&path)?;
    file.write_all(bytes)?;
    file.flush()?;
    Ok(path)
}

/// The paste pipeline with its three readers injected, so tests can pin the
/// precedence (file paths → image → text) and the no-fall-through rule for
/// control characters without touching the real clipboard.
pub fn read_paste_with<F, I, T>(
    read_files: F,
    read_image: I,
    read_text: T,
) -> Result<Option<ClipboardPaste>, String>
where
    F: FnOnce() -> Option<Vec<String>>,
    I: FnOnce() -> Option<ClipboardImage>,
    T: FnOnce() -> Option<String>,
{
    if let Some(paths) = read_files().filter(|paths| !paths.is_empty()) {
        validate_clipboard_file_paths(&paths)?;
        return Ok(Some(ClipboardPaste::FilePaths(paths)));
    }
    if let Some(image) = read_image() {
        let extension = extension_for_image_mime_type(&image.mime_type).unwrap_or("png");
        let path = write_clipboard_temp_file(&image.bytes, extension)
            .map_err(|error| format!("cannot write clipboard image: {error}"))?;
        return Ok(Some(ClipboardPaste::ImagePath(
            path.to_string_lossy().into_owned(),
        )));
    }
    Ok(read_text().map(ClipboardPaste::Text))
}

/// Read one clipboard paste with upstream `handleClipboardPaste` semantics:
/// native file list first, then an image (written to a temp file and returned
/// as its path), then plain text. An error aborts the whole paste.
pub fn read_paste() -> Result<Option<ClipboardPaste>, String> {
    read_paste_with(read_file_paths, read_image, read_text)
}

/// Native file-list read: Explorer/Finder file copies surface through
/// arboard's platform pasteboard file list (upstream `readClipboardFilePaths`).
/// This is not macOS-only: Windows exposes CF_HDROP and the Unix backends
/// expose text/uri-list, so a copied image FILE is inserted as its path
/// exactly like pi instead of falling through to the bitmap/text readers.
fn read_file_paths() -> Option<Vec<String>> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    let paths = clipboard.get().file_list().ok()?;
    if paths.is_empty() {
        return None;
    }
    Some(
        paths
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
    )
}

/// Environment inputs that select the image-reader route. Kept as a plain
/// struct so the precedence is unit-testable on any host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageRouteEnv {
    pub linux: bool,
    pub wayland: bool,
    pub wsl: bool,
    pub termux: bool,
    pub display: bool,
}

impl ImageRouteEnv {
    /// Detect the route inputs from the process environment (upstream
    /// `isWaylandSession`, `isWSL`, and the Termux `TERMUX_VERSION` check).
    pub fn detect() -> Self {
        let present = |name: &str| std::env::var(name).is_ok_and(|value| !value.is_empty());
        let linux = cfg!(target_os = "linux");
        Self {
            linux,
            wayland: present("WAYLAND_DISPLAY")
                || std::env::var("XDG_SESSION_TYPE")
                    .is_ok_and(|value| value.eq_ignore_ascii_case("wayland")),
            wsl: linux && detect_wsl(),
            termux: present("TERMUX_VERSION"),
            display: present("DISPLAY"),
        }
    }
}

/// Tri-state result of one image backend: `Unavailable` is upstream's
/// `undefined` (command missing/failed, keep falling back), `Empty` is `null`
/// (the backend answered and has no image, stop falling back), `Data` is a
/// decoded image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageAttempt {
    Unavailable,
    Empty,
    Data(ClipboardImage),
}

/// Run the image-reader route with every backend injected (upstream
/// `readClipboardImage`).
///
/// The tri-state matters: an empty Wayland clipboard must NOT fall through to
/// stale X11 contents, while a missing/failed `wl-paste` must.
pub fn read_image_with<W, X, P, N>(
    env: &ImageRouteEnv,
    wl_paste: W,
    xclip: X,
    powershell: P,
    native: N,
) -> Option<ClipboardImage>
where
    W: FnOnce() -> ImageAttempt,
    X: FnOnce() -> ImageAttempt,
    P: FnOnce() -> Option<ClipboardImage>,
    N: FnOnce() -> ImageAttempt,
{
    if env.termux {
        return None;
    }

    let mut attempt: Option<ImageAttempt> = None;
    if env.linux {
        if env.wayland || env.wsl {
            attempt = available_attempt(wl_paste());
        }
        // Only an unavailable backend (`undefined`) falls through; `Empty`
        // (a real answer with no image) stops the chain here.
        if attempt.is_none() {
            attempt = available_attempt(xclip());
        }
        // WSL images land on the Windows clipboard (Win+Shift+S), so ask
        // PowerShell whenever the Linux side did not produce data; a null
        // PowerShell answer keeps an existing `Empty` result.
        if env.wsl && !matches!(attempt, Some(ImageAttempt::Data(_))) {
            if let Some(image) = powershell() {
                attempt = Some(ImageAttempt::Data(image));
            }
        }
        if attempt.is_none() && env.display {
            attempt = available_attempt(native());
        }
    } else {
        attempt = available_attempt(native());
    }

    match attempt {
        Some(ImageAttempt::Data(image)) => Some(image),
        _ => None,
    }
}

/// `Unavailable` is upstream's `undefined` and must keep the chain moving, so
/// it maps to "no answer yet"; every real answer (including `Empty`) is kept.
fn available_attempt(attempt: ImageAttempt) -> Option<ImageAttempt> {
    match attempt {
        ImageAttempt::Unavailable => None,
        answer => Some(answer),
    }
}

/// Read one image from the platform clipboard (upstream `readClipboardImage`),
/// converting unsupported formats (e.g. Windows DIB-as-BMP) to PNG.
pub fn read_image() -> Option<ClipboardImage> {
    let env = ImageRouteEnv::detect();
    let mut image = read_image_with(
        &env,
        wl_paste_image,
        xclip_image,
        powershell_wsl_image,
        native_image,
    )?;
    if !is_supported_image_mime_type(&image.mime_type) {
        image.bytes = convert_image_to_png(&image.bytes)?;
        image.mime_type = "image/png".to_owned();
    }
    Some(image)
}

/// `wl-paste --list-types` then `wl-paste --type <selected> --no-newline`.
fn wl_paste_image() -> ImageAttempt {
    let Some(list) = run_clipboard_capture("wl-paste", &["--list-types"], CLIPBOARD_LIST_TIMEOUT)
    else {
        return ImageAttempt::Unavailable;
    };
    let types = parse_mime_type_list(&list);
    let Some(selected) = select_preferred_image_mime_type(&types) else {
        return ImageAttempt::Empty;
    };
    let Some(data) = run_clipboard_capture(
        "wl-paste",
        &["--type", selected.as_str(), "--no-newline"],
        CLIPBOARD_COMMAND_TIMEOUT,
    ) else {
        return ImageAttempt::Unavailable;
    };
    if data.is_empty() {
        return ImageAttempt::Empty;
    }
    ImageAttempt::Data(ClipboardImage {
        bytes: data,
        mime_type: base_mime_type(&selected),
    })
}

/// `xclip -selection clipboard -t TARGETS -o` then the selected type.
fn xclip_image() -> ImageAttempt {
    let Some(targets) = run_clipboard_capture(
        "xclip",
        &["-selection", "clipboard", "-t", "TARGETS", "-o"],
        CLIPBOARD_LIST_TIMEOUT,
    ) else {
        return ImageAttempt::Unavailable;
    };
    let types = parse_mime_type_list(&targets);
    let Some(selected) = select_preferred_image_mime_type(&types) else {
        return ImageAttempt::Empty;
    };
    let Some(data) = run_clipboard_capture(
        "xclip",
        &["-selection", "clipboard", "-t", selected.as_str(), "-o"],
        CLIPBOARD_COMMAND_TIMEOUT,
    ) else {
        return ImageAttempt::Unavailable;
    };
    if data.is_empty() {
        return ImageAttempt::Empty;
    }
    ImageAttempt::Data(ClipboardImage {
        bytes: data,
        mime_type: base_mime_type(&selected),
    })
}

/// WSL: read the Windows clipboard through PowerShell's `GetImage`, saving a
/// PNG into the Linux temp directory via its `\\wsl$` path (upstream
/// `readClipboardImageViaPowerShell`).
fn powershell_wsl_image() -> Option<ClipboardImage> {
    let temp_path = std::env::temp_dir().join(format!("pi-wsl-clip-{}.png", uuid::Uuid::new_v4()));
    let temp_path_text = temp_path.to_string_lossy().into_owned();
    let result = (|| {
        let windows_path =
            run_clipboard_capture("wslpath", &["-w", &temp_path_text], CLIPBOARD_LIST_TIMEOUT)?;
        let windows_path = String::from_utf8_lossy(&windows_path).trim().to_owned();
        if windows_path.is_empty() {
            return None;
        }
        let script = windows_clipboard_image_script(&windows_path);
        let output = run_clipboard_capture(
            "powershell.exe",
            &["-NoProfile", "-Command", &script],
            CLIPBOARD_POWERSHELL_TIMEOUT,
        )?;
        if String::from_utf8_lossy(&output).trim() != "ok" {
            return None;
        }
        let length = std::fs::metadata(&temp_path).ok()?.len();
        if length == 0 || length > CLIPBOARD_STDOUT_CAP as u64 {
            return None;
        }
        let bytes = std::fs::read(&temp_path).ok()?;
        Some(ClipboardImage {
            bytes,
            mime_type: "image/png".to_owned(),
        })
    })();
    let _ = std::fs::remove_file(&temp_path);
    result
}

/// The PowerShell script that copies the Windows clipboard image to `path`;
/// the path is single-quoted with `''` escaping (upstream
/// `readClipboardImageViaPowerShell`). Pure so tests can pin the quoting.
pub fn windows_clipboard_image_script(path: &str) -> String {
    let escaped = path.replace('\'', "''");
    [
        "Add-Type -AssemblyName System.Windows.Forms".to_owned(),
        "Add-Type -AssemblyName System.Drawing".to_owned(),
        format!("$path = '{escaped}'"),
        "$img = [System.Windows.Forms.Clipboard]::GetImage()".to_owned(),
        "if ($img) { $img.Save($path, [System.Drawing.Imaging.ImageFormat]::Png); Write-Output 'ok' } else { Write-Output 'empty' }".to_owned(),
    ]
    .join("; ")
}

/// Native arboard read for Windows/macOS (and Linux X11 as the last resort):
/// RGBA pixels re-encoded as PNG.
fn native_image() -> ImageAttempt {
    let Ok(mut clipboard) = arboard::Clipboard::new() else {
        return ImageAttempt::Unavailable;
    };
    let Ok(image) = clipboard.get_image() else {
        return ImageAttempt::Empty;
    };
    match encode_rgba_png(image.width, image.height, image.bytes.as_ref()) {
        Some(bytes) => ImageAttempt::Data(ClipboardImage {
            bytes,
            mime_type: "image/png".to_owned(),
        }),
        None => ImageAttempt::Unavailable,
    }
}

/// Encode raw RGBA pixels as a PNG (upstream's native clipboard returns RGBA
/// and the temp file must be a real image format).
fn encode_rgba_png(width: usize, height: usize, rgba: &[u8]) -> Option<Vec<u8>> {
    let width = u32::try_from(width).ok()?;
    let height = u32::try_from(height).ok()?;
    let image = image::RgbaImage::from_raw(width, height, rgba.to_vec())?;
    let mut output = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut output, image::ImageFormat::Png)
        .ok()?;
    Some(output.into_inner())
}

/// Convert an unsupported clipboard image to PNG (upstream Photon fallback);
/// `None` when the bytes cannot be decoded.
fn convert_image_to_png(bytes: &[u8]) -> Option<Vec<u8>> {
    let image = image::load_from_memory(bytes).ok()?;
    let mut output = std::io::Cursor::new(Vec::new());
    image.write_to(&mut output, image::ImageFormat::Png).ok()?;
    Some(output.into_inner())
}

/// Parse a clipboard type list (`wl-paste --list-types` / xclip `TARGETS`):
/// newline-separated, trimmed, empty lines dropped.
fn parse_mime_type_list(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|line| line.trim().to_owned())
        .filter(|line| !line.is_empty())
        .collect()
}

/// Run a read-only clipboard command with a hard timeout and a stdout cap
/// (upstream `runClipboardCommand`: 50 MiB, kill on timeout, `undefined` on
/// any failure). Stdin is /dev/null so no writer can wait on us.
fn run_clipboard_capture(program: &str, args: &[&str], timeout: Duration) -> Option<Vec<u8>> {
    use std::process::Stdio;
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // CREATE_NO_WINDOW (0x08000000): no console flash from helper processes.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn().ok()?;
    let stdout = child.stdout.take()?;
    let reader = std::thread::Builder::new()
        .name("pir-clipboard-cmd".to_owned())
        .spawn(move || read_capped(stdout, CLIPBOARD_STDOUT_CAP))
        .ok()?;

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return None;
            }
        }
    };
    let bytes = reader.join().ok()??;
    status.success().then_some(bytes)
}

/// Read at most `cap` bytes; `None` aborts the command when the cap is
/// exceeded (the caller treats it as a failed backend).
fn read_capped(mut reader: impl std::io::Read, cap: usize) -> Option<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Some(output),
            Ok(read) => {
                if output.len() + read > cap {
                    return None;
                }
                output.extend_from_slice(&buffer[..read]);
            }
            Err(_) => return None,
        }
    }
}

/// WSL detection (upstream `isWSL`): interop markers first, then the kernel
/// version string.
fn detect_wsl() -> bool {
    let present = |name: &str| std::env::var(name).is_ok_and(|value| !value.is_empty());
    if present("WSL_DISTRO_NAME") || present("WSLENV") {
        return true;
    }
    std::fs::read_to_string("/proc/version")
        .map(|version| {
            let version = version.to_lowercase();
            version.contains("microsoft") || version.contains("wsl")
        })
        .unwrap_or(false)
}

/// Delivery guard for [`spawn_read_paste_with_exit`]: the normal path takes
/// the callback, a panic unwinds through `Drop` and reports a reader error,
/// so the caller still retires its pending slot and shows a notice.
struct PasteDeliverGuard<F: FnOnce(Result<Option<ClipboardPaste>, String>)> {
    deliver: Option<F>,
}

impl<F: FnOnce(Result<Option<ClipboardPaste>, String>)> PasteDeliverGuard<F> {
    fn new(deliver: F) -> Self {
        Self {
            deliver: Some(deliver),
        }
    }

    fn deliver(mut self, result: Result<Option<ClipboardPaste>, String>) {
        if let Some(deliver) = self.deliver.take() {
            deliver(result);
        }
    }
}

impl<F: FnOnce(Result<Option<ClipboardPaste>, String>)> Drop for PasteDeliverGuard<F> {
    fn drop(&mut self) {
        if let Some(deliver) = self.deliver.take() {
            deliver(Err("clipboard reader panicked".to_owned()));
        }
    }
}

/// The paste reader-thread body, kept separate so tests can pin the panic
/// delivery without touching the platform clipboard.
fn run_read_paste<R, F>(read: R, deliver: F)
where
    R: FnOnce() -> Result<Option<ClipboardPaste>, String>,
    F: FnOnce(Result<Option<ClipboardPaste>, String>),
{
    let deliver = PasteDeliverGuard::new(deliver);
    let result = read();
    deliver.deliver(result);
}

/// Run [`read_paste`] OFF the caller's thread. Same exactly-once contract as
/// [`spawn_read_text_with_exit`]: `on_exit` runs on every exit path, including
/// a panicking reader (which delivers an error through `deliver`).
pub fn spawn_read_paste_with_exit<F, E>(deliver: F, on_exit: E) -> std::io::Result<()>
where
    F: FnOnce(Result<Option<ClipboardPaste>, String>) + Send + 'static,
    E: FnOnce() + Send + 'static,
{
    std::thread::Builder::new()
        .name("pir-clipboard-paste".to_owned())
        .spawn(move || {
            let _exit = ReaderExitGuard::new(on_exit);
            run_read_paste(read_paste, deliver);
        })
        .map(|_| ())
}
