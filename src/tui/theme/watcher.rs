//! Theme file hot-reload — the Rust port of upstream `theme.ts`'s
//! `startThemeWatcher` / `stopThemeWatcher` (theme.ts:734-818) and the
//! `onThemeChange` callback (theme.ts:730-732).
//!
//! Upstream watches the custom themes directory while the active theme is a
//! custom one (`initTheme(name, true)` / `setTheme(name, true)` pass
//! `enableWatcher`; the interactive controller always does). A change to
//! `<name>.json` debounces for [`THEME_RELOAD_DEBOUNCE`] (100 ms, so an
//! editor's write/rename burst reloads once), then the file is re-read and
//! re-parsed: the registry entry is replaced, the global theme swaps to the
//! reloaded one, and the change callback fires so the UI repaints. A stale
//! timer (the active theme changed meanwhile) and a temporarily missing file
//! (the editor is mid-save) are ignored, and a parse error leaves the last
//! good theme in place.
//!
//! The decision logic is pure ([`ThemeWatcherPlan`]) so tests can drive it
//! with synthetic events and instants; [`start_theme_watcher`] is the thin
//! `notify`-based runtime around it.

use notify::Watcher as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::registry::{custom_themes_cell, register_custom_theme_path, try_register_custom_theme};
use super::state::{current_theme_name, set_theme};
use super::theme_json::parse_theme_json;
use super::SYSTEM_THEME_NAME;

/// How long after the last filesystem event a theme file is re-read
/// (upstream's `setTimeout(..., 100)`, theme.ts:790).
pub const THEME_RELOAD_DEBOUNCE: Duration = Duration::from_millis(100);

/// Longest the runtime thread blocks waiting for a filesystem event. While a
/// reload is armed it waits exactly until the debounce deadline (which is at
/// most [`THEME_RELOAD_DEBOUNCE`]); with nothing armed it still wakes
/// periodically to observe the stop flag, instead of the former fixed 25 ms
/// tick (40 idle wakeups/s for a file that changes a handful of times a
/// session). [`ThemeWatcherHandle::stop`] additionally sends a stop event
/// through the watcher channel, so this interval only bounds how often the
/// thread re-checks its flag — it does not bound shutdown latency.
const STOP_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// How long the watcher thread may block before it must re-check its state:
/// the time left until the armed debounce deadline, capped at
/// [`STOP_CHECK_INTERVAL`] so `stop()` stays prompt. With nothing armed this
/// is the stop-check interval itself — there is no debounce to wait for, so
/// a fixed short tick would only burn wakeups.
#[doc(hidden)]
pub fn theme_watch_timeout(plan: &ThemeWatcherPlan, now: Instant) -> Duration {
    match plan.due_at() {
        Some(due) => due.saturating_duration_since(now).min(STOP_CHECK_INTERVAL),
        None => STOP_CHECK_INTERVAL,
    }
}

/// The theme file a watcher follows.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WatchedTheme {
    name: String,
    path: PathBuf,
}

/// The pure decision core of the watcher: which file is watched, whether a
/// filesystem event scheduled a reload, and when that reload is due.
#[derive(Debug, Default, Clone)]
pub struct ThemeWatcherPlan {
    watched: Option<WatchedTheme>,
    /// When the debounce window started (upstream `themeReloadTimer`).
    pending_since: Option<Instant>,
}

impl ThemeWatcherPlan {
    /// Start watching `name`'s file in `custom_dir` (upstream
    /// `startThemeWatcher`, theme.ts:734-758): only a custom theme has a
    /// file, and only an existing file is watched:
    ///
    /// - `dark`, `light` and `system` are built in / generated, so they are
    ///   never watched;
    /// - a name whose `<name>.json` does not exist (e.g. a theme registered
    ///   by an extension) is not watched either.
    pub fn start(name: &str, custom_dir: &Path, file_exists: &dyn Fn(&Path) -> bool) -> Self {
        let watching = name != "dark" && name != "light" && name != SYSTEM_THEME_NAME;
        if !watching {
            return Self::default();
        }
        let path = custom_dir.join(format!("{name}.json"));
        if !file_exists(&path) {
            return Self::default();
        }
        Self {
            watched: Some(WatchedTheme {
                name: name.to_string(),
                path,
            }),
            pending_since: None,
        }
    }

    /// Stop watching and drop any armed reload (upstream `stopThemeWatcher`,
    /// theme.ts:812-818).
    pub fn stop(&mut self) {
        self.watched = None;
        self.pending_since = None;
    }

    /// Whether a file is being watched.
    pub fn is_watching(&self) -> bool {
        self.watched.is_some()
    }

    /// The watched file.
    pub fn path(&self) -> Option<&Path> {
        self.watched.as_ref().map(|watched| watched.path.as_path())
    }

    /// The watched theme's name.
    pub fn name(&self) -> Option<&str> {
        self.watched.as_ref().map(|watched| watched.name.as_str())
    }

    /// Whether a reload is armed (waiting out the debounce).
    pub fn is_pending(&self) -> bool {
        self.pending_since.is_some()
    }

    /// One filesystem event: arm (or re-arm) the debounce when it concerns the
    /// watched file and the active theme is still the watched one (upstream's
    /// listener, theme.ts:793-808). `filename` is the event's file name, or
    /// `None` when the backend did not report one — upstream reloads for an
    /// unnamed event on the watched directory.
    pub fn on_fs_event(&mut self, current_name: &str, filename: Option<&str>, now: Instant) {
        let Some(watched) = &self.watched else {
            return;
        };
        if current_name != watched.name {
            return;
        }
        if let Some(filename) = filename {
            if filename != format!("{}.json", watched.name) {
                return;
            }
        }
        // Upstream clears the previous timer and starts a new one: the last
        // event of a burst decides when the reload runs.
        self.pending_since = Some(now);
    }

    /// The debounce deadline of the armed reload, if any.
    pub fn due_at(&self) -> Option<Instant> {
        self.pending_since
            .map(|since| since + THEME_RELOAD_DEBOUNCE)
    }

    /// Whether the armed reload is due at `now`.
    pub fn is_due(&self, now: Instant) -> bool {
        self.due_at().is_some_and(|due| now >= due)
    }

    /// Take the reload that is due at `now`, if any (upstream's timer body,
    /// theme.ts:780-791): the timer is cleared first, a stale one (the active
    /// theme changed) is ignored, and a temporarily missing file keeps the
    /// last successfully loaded theme active.
    pub fn take_due(
        &mut self,
        current_name: &str,
        now: Instant,
        file_exists: &dyn Fn(&Path) -> bool,
    ) -> Option<PathBuf> {
        if !self.is_due(now) {
            return None;
        }
        self.pending_since = None;
        let watched = self.watched.as_ref()?;
        if current_name != watched.name {
            return None;
        }
        if !file_exists(&watched.path) {
            return None;
        }
        Some(watched.path.clone())
    }
}

/// A per-installation listener for "the theme changed" (upstream
/// `onThemeChange`, theme.ts:730-732): the interactive UI re-renders.
type ThemeChangeListener = Arc<dyn Fn() + Send + Sync>;

fn theme_change_listener_cell() -> &'static Mutex<Option<ThemeChangeListener>> {
    static CELL: OnceLock<Mutex<Option<ThemeChangeListener>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

/// Register the callback that fires after the theme was reloaded or swapped
/// (upstream `onThemeChange`). Replaces any previous listener.
pub fn set_theme_change_listener(listener: Arc<dyn Fn() + Send + Sync>) {
    *theme_change_listener_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(listener);
}

/// Fire the registered change callback (a no-op without one).
pub fn notify_theme_change() {
    let listener = theme_change_listener_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(listener) = listener {
        listener();
    }
}

/// Re-read one theme file and make it the active theme (upstream's watcher
/// timer body, theme.ts:781-790): parse the file (an invalid state while the
/// editor writes is ignored, keeping the last good theme), refresh the
/// registry entry, swap the global theme, and notify the UI.
///
/// Returns the registered theme's name on success.
pub fn reload_theme_file(path: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let theme = parse_theme_json(&text)?;
    let name = theme.name.clone();
    // Upstream `registeredThemes.set(watchedThemeName, reloadedTheme)`: the
    // reload REPLACES the registry entry (unlike the first-wins startup
    // loaders), so the edit takes effect; the file it came from is remembered
    // for the extension API.
    remove_registered_theme(&name);
    register_custom_theme_path(&name, path.to_path_buf());
    try_register_custom_theme(theme);
    // Upstream `setGlobalTheme(reloadedTheme)`: the watched theme is the
    // active one (the stale-timer guard checked that), so swap it in.
    if current_theme_name() == name {
        set_theme(&name);
        notify_theme_change();
    }
    Ok(name)
}

/// Drop a registered custom theme (the reload path's "set" semantics
/// upstream gets from `Map.set`).
fn remove_registered_theme(name: &str) {
    custom_themes_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(name);
}

/// One event delivered to the watcher thread (a `notify` backend event, or
/// the synthetic error [`ThemeWatcherHandle::shutdown`] sends to wake it).
type WatchEvent = notify::Result<notify::Event>;

/// A running theme watcher: stops when dropped or [`ThemeWatcherHandle::stop`]ed.
pub struct ThemeWatcherHandle {
    stop: Arc<AtomicBool>,
    /// Wakes the thread out of `recv_timeout` immediately on shutdown, so the
    /// stop-check interval can stay long (fewer idle wakeups) without adding
    /// shutdown latency. The synthetic `Err` makes the loop exit.
    stop_tx: Option<std::sync::mpsc::Sender<WatchEvent>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ThemeWatcherHandle {
    /// Stop watching (upstream `stopThemeWatcher`).
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(sender) = self.stop_tx.take() {
            let _ = sender.send(Err(notify::Error::generic("theme watcher stopped")));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ThemeWatcherHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Watch the active theme's file and reload it on change (upstream
/// `startThemeWatcher`, called with `enableWatcher: true`). Returns the
/// handle, or `None` when the active theme has no watchable file (built-in,
/// system, or missing custom file).
///
/// The `notify` backend watches the custom themes directory — upstream does
/// the same, because editors replace the file via rename — and events for
/// other files are filtered out by name.
pub fn start_theme_watcher(name: &str, custom_dir: &Path) -> Option<ThemeWatcherHandle> {
    let plan = ThemeWatcherPlan::start(name, custom_dir, &|path| path.exists());
    if !plan.is_watching() {
        return None;
    }
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let dir = custom_dir.to_path_buf();
    let (sender, receiver) = std::sync::mpsc::channel::<WatchEvent>();
    let stop_tx = sender.clone();
    let thread = std::thread::Builder::new()
        .name("pir-theme-watch".to_owned())
        .spawn(move || {
            let mut native = match notify::RecommendedWatcher::new(
                move |event| {
                    let _ = sender.send(event);
                },
                notify::Config::default(),
            ) {
                Ok(native) => native,
                Err(_) => return,
            };
            if native
                .watch(&dir, notify::RecursiveMode::NonRecursive)
                .is_err()
            {
                return;
            }
            let mut plan = plan;
            while !thread_stop.load(Ordering::SeqCst) {
                // Block until an event arrives, the armed debounce is due, or
                // the stop-check interval elapses (see
                // `theme_watch_timeout`). No fixed 25 ms tick.
                let wait = theme_watch_timeout(&plan, Instant::now());
                match receiver.recv_timeout(wait) {
                    Ok(Ok(event)) => {
                        let filename = event
                            .paths
                            .first()
                            .and_then(|path| path.file_name())
                            .map(|file| file.to_string_lossy().into_owned());
                        let current = current_theme_name();
                        plan.on_fs_event(&current, filename.as_deref(), Instant::now());
                    }
                    Ok(Err(_)) => {
                        // A backend error (e.g. the directory vanished) keeps
                        // the last good theme; upstream closes the watcher.
                        break;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
                // The stale-timer guard uses the live active theme name: a
                // timer that fires after the user switched themes is ignored.
                let current = current_theme_name();
                if let Some(path) = plan.take_due(&current, Instant::now(), &|path| path.exists()) {
                    // Upstream ignores a parse error (the file may be in an
                    // invalid state while being edited).
                    let _ = reload_theme_file(&path);
                }
            }
        });
    match thread {
        Ok(thread) => Some(ThemeWatcherHandle {
            stop,
            stop_tx: Some(stop_tx),
            thread: Some(thread),
        }),
        Err(_) => None,
    }
}
