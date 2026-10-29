//! Small command-overlay components shared by the slash-command handlers —
//! ports of upstream `components/trust-selector.ts` (`/trust`), the
//! `showExtensionConfirm` Yes/No dialog shape used by `handleImportCommand`
//! (`/import`), and the OAuth provider selector used by `/login`/`/logout`
//! (`OAuthSelectorComponent` + `getLoginProviderOptions`/
//! `getLogoutProviderOptions`).
//!
//! These are pure state machines: the TUI maps real key events in and
//! executes the returned outcomes against the trust store, the worker, and
//! the auth storage. Keys are fixed (upstream binds semantic actions through
//! its keybinding registry; pir's selector overlays use the same fixed-key
//! convention as the scoped-models selector).

use std::sync::Mutex;

use super::core::{Component, Container, DynamicBorder, Spacer, Text};
use super::theme::{with_theme, ThemeColor};

// ---------------------------------------------------------------------------
// Confirm dialog (upstream showExtensionConfirm → Yes/No selector)
// ---------------------------------------------------------------------------

/// Confirm-dialog keys, decoupled from crossterm for direct tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmKey {
    Left,
    Right,
    Enter,
    Yes,
    No,
    Esc,
}

/// A Yes/No confirm dialog (upstream `showExtensionConfirm` renders the same
/// two options; pir keeps the pir dialog's y/n + arrow-key interaction).
pub struct ConfirmDialog {
    state: Mutex<ConfirmState>,
}

struct ConfirmState {
    title: String,
    message: String,
    yes: bool,
}

/// What a confirm-dialog key press produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmOutcome {
    /// The user answered (Enter / y / n); `true` = Yes.
    Answered(bool),
    /// The user cancelled (Esc).
    Cancelled,
}

impl ConfirmDialog {
    pub fn new(title: String, message: String) -> Self {
        Self {
            state: Mutex::new(ConfirmState {
                title,
                message,
                // Upstream showExtensionConfirm renders ["Yes", "No"] with
                // `selectedIndex = 0` (extension-selector.ts): the dialog
                // opens with Yes highlighted.
                yes: true,
            }),
        }
    }

    /// Handle one key: `Some` = the dialog closed (answered or cancelled),
    /// `None` = not consumed (arrow keys just move the highlight).
    pub fn handle_key(&self, key: ConfirmKey) -> Option<ConfirmOutcome> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match key {
            ConfirmKey::Left | ConfirmKey::Right => {
                state.yes = !state.yes;
                None
            }
            ConfirmKey::Yes => Some(ConfirmOutcome::Answered(true)),
            ConfirmKey::No => Some(ConfirmOutcome::Answered(false)),
            ConfirmKey::Enter => Some(ConfirmOutcome::Answered(state.yes)),
            ConfirmKey::Esc => Some(ConfirmOutcome::Cancelled),
        }
    }

    /// The highlighted answer (test seam).
    pub fn is_yes(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .yes
    }
}

impl Component for ConfirmDialog {
    fn render(&self, width: usize) -> Vec<String> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut frame = Container::new();
        frame.add_child(DynamicBorder::new());
        frame.add_child(Spacer::new(1));
        frame.add_child(Text::with_padding(
            with_theme(|theme| theme.fg(ThemeColor::Accent, &theme.bold(&state.title))),
            1,
            0,
        ));
        frame.add_child(Text::with_padding(
            with_theme(|theme| theme.fg(ThemeColor::Text, &state.message)),
            1,
            0,
        ));
        frame.add_child(Spacer::new(1));
        frame.add_child(Text::with_padding(selector_option("Yes", state.yes), 1, 0));
        frame.add_child(Text::with_padding(selector_option("No", !state.yes), 1, 0));
        frame.add_child(Spacer::new(1));
        frame.add_child(Text::with_padding(
            with_theme(|theme| {
                theme.fg(
                    ThemeColor::Dim,
                    "  ←/→ select · enter confirm · y/n · esc cancel",
                )
            }),
            1,
            0,
        ));
        frame.add_child(DynamicBorder::new());
        frame.render(width)
    }

    fn invalidate(&self) {}
}

/// One selector row (upstream `extension-selector.ts:87-92` via dialogs.rs).
fn selector_option(label: &str, selected: bool) -> String {
    if selected {
        with_theme(|theme| {
            format!(
                "{}{}",
                theme.fg(ThemeColor::Accent, "→ "),
                theme.fg(ThemeColor::Accent, label)
            )
        })
    } else {
        with_theme(|theme| format!("  {}", theme.fg(ThemeColor::Text, label)))
    }
}

// ---------------------------------------------------------------------------
// Trust selector (upstream components/trust-selector.ts)
// ---------------------------------------------------------------------------

/// One selectable trust option (upstream `ProjectTrustOption`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustOption {
    /// The display label for the selector row.
    pub label: String,
    /// Whether choosing this option marks the project trusted.
    pub trusted: bool,
    /// Whether this option matches the currently saved decision (the `✓`
    /// marker; upstream `isSavedOption`).
    pub is_saved: bool,
}

impl From<&crate::config::trust::ProjectTrustOption> for TrustOption {
    fn from(option: &crate::config::trust::ProjectTrustOption) -> Self {
        Self {
            label: option.label.clone(),
            trusted: option.trusted,
            is_saved: false,
        }
    }
}

/// Trust-selector keys, decoupled from crossterm for direct tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustKey {
    Up,
    Down,
    Enter,
    Esc,
}

/// What a trust-selector key press produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustOutcome {
    None,
    /// Save the highlighted option (index into the option list).
    Save(usize),
    Cancel,
}

/// The `/trust` selector: navigate the options, Enter saves the decision to
/// trust.json (upstream `TrustSelectorComponent.handleInput` →
/// `trustStore.setMany(selection.updates)`).
pub struct TrustSelector {
    state: Mutex<TrustState>,
}

struct TrustState {
    cwd_text: String,
    saved_text: String,
    current_text: String,
    options: Vec<TrustOption>,
    selected: usize,
}

impl TrustSelector {
    /// `options`: the [`crate::config::trust::get_project_trust_options`]
    /// list; `saved_index`: the option matching the saved decision
    /// (`None` = no saved decision).
    pub fn new(
        cwd_text: String,
        saved_text: String,
        current_trusted: bool,
        options: Vec<TrustOption>,
        saved_index: Option<usize>,
    ) -> Self {
        let selected = saved_index
            .unwrap_or(0)
            .min(options.len().saturating_sub(1));
        Self {
            state: Mutex::new(TrustState {
                cwd_text,
                saved_text,
                current_text: if current_trusted {
                    "trusted"
                } else {
                    "untrusted"
                }
                .to_string(),
                options,
                selected,
            }),
        }
    }

    pub fn options(&self) -> Vec<TrustOption> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .options
            .clone()
    }

    pub fn handle_key(&self, key: TrustKey) -> TrustOutcome {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match key {
            TrustKey::Up => {
                state.selected = state.selected.saturating_sub(1);
                TrustOutcome::None
            }
            TrustKey::Down => {
                let last = state.options.len().saturating_sub(1);
                state.selected = (state.selected + 1).min(last);
                TrustOutcome::None
            }
            TrustKey::Enter => TrustOutcome::Save(state.selected),
            TrustKey::Esc => TrustOutcome::Cancel,
        }
    }
}

impl Component for TrustSelector {
    fn render(&self, width: usize) -> Vec<String> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut frame = Container::new();
        frame.add_child(DynamicBorder::new());
        frame.add_child(Spacer::new(1));
        frame.add_child(Text::with_padding(
            with_theme(|theme| theme.fg(ThemeColor::Accent, &theme.bold("Project trust"))),
            1,
            0,
        ));
        frame.add_child(Text::with_padding(
            with_theme(|theme| theme.fg(ThemeColor::Muted, &state.cwd_text)),
            1,
            0,
        ));
        frame.add_child(Spacer::new(1));
        frame.add_child(Text::with_padding(
            with_theme(|theme| {
                theme.fg(
                    ThemeColor::Muted,
                    &format!("Saved decision: {}", state.saved_text),
                )
            }),
            1,
            0,
        ));
        frame.add_child(Text::with_padding(
            with_theme(|theme| {
                theme.fg(
                    ThemeColor::Muted,
                    &format!("Current session: {}", state.current_text),
                )
            }),
            1,
            0,
        ));
        frame.add_child(Spacer::new(1));
        for (index, option) in state.options.iter().enumerate() {
            let is_selected = index == state.selected;
            let marker = if option.is_saved {
                with_theme(|theme| theme.fg(ThemeColor::Accent, "✓ "))
            } else {
                "  ".to_string()
            };
            let prefix = if is_selected {
                with_theme(|theme| theme.fg(ThemeColor::Accent, "→ "))
            } else {
                "  ".to_string()
            };
            let label = format!("{prefix}{marker}{}", option.label);
            frame.add_child(Text::with_padding(
                if is_selected {
                    with_theme(|theme| theme.fg(ThemeColor::Accent, &label))
                } else {
                    label
                },
                1,
                0,
            ));
        }
        frame.add_child(Spacer::new(1));
        frame.add_child(Text::with_padding(
            with_theme(|theme| {
                theme.fg(ThemeColor::Dim, "  ↑/↓ navigate · enter save · esc cancel")
            }),
            1,
            0,
        ));
        frame.add_child(DynamicBorder::new());
        frame.render(width)
    }

    fn invalidate(&self) {}
}

// ---------------------------------------------------------------------------
// OAuth provider selector (upstream OAuthSelectorComponent, login + logout)
// ---------------------------------------------------------------------------

/// The login method a provider row starts (upstream
/// `AuthSelectorProvider.authType`: each provider lists one OAuth entry and
/// one API-key entry when both auth methods are declared,
/// interactive-mode.ts:5788-5821).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthProviderKind {
    /// Browser/device OAuth flow (upstream `authType: "oauth"`).
    OAuth,
    /// Paste-an-API-key dialog (upstream `authType: "api_key"`).
    ApiKey,
}

impl AuthProviderKind {
    /// The upstream tag (`"oauth"` / `"api_key"`).
    pub fn as_str(self) -> &'static str {
        match self {
            AuthProviderKind::OAuth => "oauth",
            AuthProviderKind::ApiKey => "api_key",
        }
    }
}

/// The `/login` menu's Radius row description (upstream
/// `RADIUS_LOGIN_INTRO`, interactive-mode.ts:371; rendered on the Radius
/// provider row, interactive-mode.ts:6269).
pub const RADIUS_LOGIN_INTRO: &str =
    "Radius is a service crafted for Pi by the builders of Pi, Earendil Works";

/// The auth-type label for one row (upstream
/// `formatAuthSelectorProviderType`, oauth-selector.ts:27-33).
pub fn auth_type_label(auth_type: AuthProviderKind, subscription: bool) -> &'static str {
    match auth_type {
        AuthProviderKind::ApiKey => "API key",
        AuthProviderKind::OAuth if subscription => "subscription",
        AuthProviderKind::OAuth => "account",
    }
}

/// The themed status suffix for one selector row (upstream
/// `formatAuthSelectorProviderStatus`, oauth-selector.ts:36-53): not
/// configured → ` • not configured`; a configured credential of a DIFFERENT
/// type → ` • {type} configured` (warning); a matching stored credential →
/// ` ✓ configured`; an ALL-CAPS source (or comma-separated list) →
/// ` ✓ env: SRC`; anything else renders the source verbatim.
pub fn auth_status_marker(
    row_kind: AuthProviderKind,
    subscription: bool,
    source: &str,
    status_kind: Option<AuthProviderKind>,
) -> String {
    let Some(configured) = status_kind else {
        return with_theme(|theme| theme.fg(ThemeColor::Muted, " • not configured"));
    };
    if configured != row_kind {
        let label = format!("{} configured", auth_type_label(configured, subscription));
        return with_theme(|theme| {
            format!(
                "{}{}",
                theme.fg(ThemeColor::Muted, " • "),
                theme.fg(ThemeColor::Warning, &label)
            )
        });
    }
    if source.is_empty() || source == "OAuth" || source == "stored credential" {
        return with_theme(|theme| theme.fg(ThemeColor::Success, " ✓ configured"));
    }
    let source = if source
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase())
        && source.split(", ").all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        }) {
        format!("env: {source}")
    } else {
        source.to_string()
    };
    with_theme(|theme| theme.fg(ThemeColor::Success, &format!(" ✓ {source}")))
}

/// One provider row (upstream `AuthSelectorProvider` reduced to pir's
/// surface): provider id, display label, login method, the
/// stored-credential status, and whether the provider bills against a
/// subscription (upstream `subscription`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthProviderOption {
    /// The provider id the auth storage keys on.
    pub id: String,
    /// The display label for the selector row.
    pub label: String,
    /// The configured credential's source label (upstream
    /// `authStatus.label ?? authStatus.source`): empty = not configured;
    /// "stored credential"/"OAuth" = a stored credential; an ALL-CAPS name
    /// renders as `env: SRC`; anything else renders verbatim.
    pub status: String,
    /// The kind of the CONFIGURED credential (`None` = not configured) —
    /// drives the `• {type} configured` mismatch warning
    /// (oauth-selector.ts:36-53).
    pub status_kind: Option<AuthProviderKind>,
    /// The login method this row starts.
    pub kind: AuthProviderKind,
    /// The provider bills a subscription (upstream
    /// `provider.auth.oauth.isSubscription === true`); pir marks the Radius
    /// subscription provider.
    pub subscription: bool,
    /// Optional row description (upstream renders the Radius login intro on
    /// its provider row, interactive-mode.ts:6269).
    pub description: Option<String>,
}

/// Auth-selector keys, decoupled from crossterm for direct tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKey {
    Up,
    Down,
    Enter,
    Esc,
}

/// What an auth-selector key press produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthOutcome {
    None,
    /// Start (login) or remove (logout) the highlighted provider; the index
    /// addresses the provider list.
    Select(usize),
    Cancel,
}

/// Shared login progress the background login thread publishes (the UI loop
/// renders it through [`AuthSelector::status`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoginProgress {
    /// Latest progress/status line.
    pub status: String,
    /// Set when the login attempt finished (success or failure).
    pub finished: bool,
}

/// The `/login`/`/logout` provider selector (upstream
/// `showLoginProviderSelector` / `showOAuthSelector("logout")` reduced to
/// pir's provider surface: OAuth-capable providers for login, stored
/// credentials for logout).
pub struct AuthSelector {
    state: Mutex<AuthState>,
}

/// The selector mode (upstream `showLoginProviderSelector` /
/// `showOAuthSelector("logout")`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    Login,
    Logout,
}

impl AuthMode {
    /// The upstream mode name (`"login"` / `"logout"`).
    pub fn as_str(self) -> &'static str {
        match self {
            AuthMode::Login => "login",
            AuthMode::Logout => "logout",
        }
    }
}

struct AuthState {
    mode: AuthMode,
    title: String,
    providers: Vec<AuthProviderOption>,
    selected: usize,
    /// Live login progress (login mode only; updated by the login thread).
    progress: LoginProgress,
}

impl AuthSelector {
    /// Login mode: `providers` are the OAuth-capable providers with their
    /// stored-credential status.
    pub fn login(providers: Vec<AuthProviderOption>) -> Self {
        Self::new(AuthMode::Login, "Select a provider to sign in:", providers)
    }

    /// Logout mode: `providers` are the stored credentials to remove.
    pub fn logout(providers: Vec<AuthProviderOption>) -> Self {
        Self::new(
            AuthMode::Logout,
            "Select a stored credential to remove:",
            providers,
        )
    }

    fn new(mode: AuthMode, title: &str, providers: Vec<AuthProviderOption>) -> Self {
        Self {
            state: Mutex::new(AuthState {
                mode,
                title: title.to_string(),
                providers,
                selected: 0,
                progress: LoginProgress::default(),
            }),
        }
    }

    /// The selector mode.
    pub fn mode(&self) -> AuthMode {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .mode
    }

    /// The provider list (test/render seam).
    pub fn providers(&self) -> Vec<AuthProviderOption> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .providers
            .clone()
    }

    /// The live login progress line.
    pub fn progress(&self) -> LoginProgress {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .progress
            .clone()
    }

    /// Publish a progress line from the login thread.
    pub fn set_progress(&self, status: String, finished: bool) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.progress = LoginProgress { status, finished };
    }

    pub fn handle_key(&self, key: AuthKey) -> AuthOutcome {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.progress.finished {
            // A completed login keeps the result on screen until Esc/Enter.
            return match key {
                AuthKey::Esc | AuthKey::Enter => AuthOutcome::Cancel,
                _ => AuthOutcome::None,
            };
        }
        match key {
            AuthKey::Up => {
                state.selected = state.selected.saturating_sub(1);
                AuthOutcome::None
            }
            AuthKey::Down => {
                let last = state.providers.len().saturating_sub(1);
                state.selected = (state.selected + 1).min(last);
                AuthOutcome::None
            }
            AuthKey::Enter => AuthOutcome::Select(state.selected),
            AuthKey::Esc => AuthOutcome::Cancel,
        }
    }
}

impl Component for AuthSelector {
    fn render(&self, width: usize) -> Vec<String> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut frame = Container::new();
        frame.add_child(DynamicBorder::new());
        frame.add_child(Spacer::new(1));
        frame.add_child(Text::with_padding(
            with_theme(|theme| theme.fg(ThemeColor::Accent, &theme.bold(&state.title))),
            1,
            0,
        ));
        frame.add_child(Spacer::new(1));
        if state.providers.is_empty() {
            frame.add_child(Text::with_padding(
                with_theme(|theme| theme.fg(ThemeColor::Muted, "  No providers available.")),
                1,
                0,
            ));
        }
        for (index, provider) in state.providers.iter().enumerate() {
            let is_selected = index == state.selected;
            let prefix = if is_selected {
                with_theme(|theme| theme.fg(ThemeColor::Accent, "→ "))
            } else {
                "  ".to_string()
            };
            // Upstream row shape (oauth-selector.ts:157-167):
            // `{name}[ ({type})]{status}` with the auth-type tag muted.
            let type_tag = with_theme(|theme| {
                theme.fg(
                    ThemeColor::Muted,
                    &format!(
                        " [{}]",
                        auth_type_label(provider.kind, provider.subscription)
                    ),
                )
            });
            let marker = auth_status_marker(
                provider.kind,
                provider.subscription,
                &provider.status,
                provider.status_kind,
            );
            let name_text = if is_selected {
                with_theme(|theme| theme.fg(ThemeColor::Accent, &provider.label))
            } else {
                provider.label.clone()
            };
            let label = format!("{prefix}{name_text}{type_tag}{marker}");
            frame.add_child(Text::with_padding(label, 1, 0));
            if let Some(description) = &provider.description {
                frame.add_child(Text::with_padding(
                    with_theme(|theme| theme.fg(ThemeColor::Muted, &format!("  {description}"))),
                    0,
                    0,
                ));
            }
        }
        if !state.progress.status.is_empty() {
            frame.add_child(Spacer::new(1));
            frame.add_child(Text::with_padding(
                with_theme(|theme| {
                    if state.progress.finished {
                        theme.fg(ThemeColor::Success, &state.progress.status)
                    } else {
                        theme.fg(ThemeColor::Muted, &state.progress.status)
                    }
                }),
                1,
                0,
            ));
        }
        frame.add_child(Spacer::new(1));
        frame.add_child(Text::with_padding(
            with_theme(|theme| {
                theme.fg(
                    ThemeColor::Dim,
                    "  ↑/↓ navigate · enter select · esc cancel",
                )
            }),
            1,
            0,
        ));
        frame.add_child(DynamicBorder::new());
        frame.render(width)
    }

    fn invalidate(&self) {}
}

/// The top-level `/login` auth-type selector (upstream
/// `showLoginAuthTypeSelector`, interactive-mode.ts:5885-5933): the provider
/// login options "Sign in with an account" → "Sign in with an API key", with
/// the dedicated "Sign in with Radius" entry LAST. Radius selection carries
/// the RADIUS_LOGIN_INTRO description; the upstream shimmer animation is a
/// recorded cosmetic deviation.
pub struct AuthTypeSelector {
    state: Mutex<AuthTypeState>,
}

struct AuthTypeState {
    /// The selector title (upstream: "Select authentication method:" for the
    /// top-level menu, "Select authentication method for {name}:" scoped to
    /// one provider).
    title: String,
    /// The row labels ("Sign in with an account", …); the Radius intro rides
    /// `descriptions` at the same index when present.
    labels: Vec<String>,
    descriptions: Vec<Option<String>>,
    /// The auth type each row starts (parallel to `labels`).
    choices: Vec<AuthTypeChoice>,
    selected: usize,
}

/// Which auth type one selector row starts (upstream routes
/// `option === subscriptionLabel ? "oauth" : "api_key"`, plus the Radius
/// row).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthTypeChoice {
    OAuth,
    ApiKey,
    Radius,
}

impl AuthTypeSelector {
    /// The top-level selector: both auth types plus the dedicated Radius
    /// entry last (upstream interactive-mode.ts:5899-5908).
    pub fn login() -> Self {
        Self::with_options(vec![
            (
                "Sign in with an account".to_string(),
                None,
                AuthTypeChoice::OAuth,
            ),
            (
                "Sign in with an API key".to_string(),
                None,
                AuthTypeChoice::ApiKey,
            ),
            (
                "Sign in with Radius".to_string(),
                Some(RADIUS_LOGIN_INTRO.to_string()),
                AuthTypeChoice::Radius,
            ),
        ])
    }

    /// A provider-scoped selector (upstream
    /// `showLoginAuthTypeSelector(providerOptions)`): only the auth types the
    /// provider declares, in upstream order, without the Radius row.
    pub fn for_provider(
        provider_name: &str,
        oauth: bool,
        api_key: bool,
        subscription_label: Option<String>,
    ) -> Self {
        Self::for_provider_inner(
            format!("Select authentication method for {provider_name}:"),
            oauth,
            api_key,
            subscription_label,
        )
    }

    fn for_provider_inner(
        title: String,
        oauth: bool,
        api_key: bool,
        subscription_label: Option<String>,
    ) -> Self {
        let mut options = Vec::new();
        if oauth {
            options.push((
                subscription_label.unwrap_or_else(|| "Sign in with an account".to_string()),
                None,
                AuthTypeChoice::OAuth,
            ));
        }
        if api_key {
            options.push((
                "Sign in with an API key".to_string(),
                None,
                AuthTypeChoice::ApiKey,
            ));
        }
        Self::with_options_inner(title, options)
    }

    fn with_options(options: Vec<(String, Option<String>, AuthTypeChoice)>) -> Self {
        Self::with_options_inner("Select authentication method:".to_string(), options)
    }

    fn with_options_inner(
        title: String,
        options: Vec<(String, Option<String>, AuthTypeChoice)>,
    ) -> Self {
        Self {
            state: Mutex::new(AuthTypeState {
                title,
                labels: options.iter().map(|(label, _, _)| label.clone()).collect(),
                descriptions: options
                    .iter()
                    .map(|(_, description, _)| description.clone())
                    .collect(),
                choices: options.iter().map(|(_, _, choice)| *choice).collect(),
                selected: 0,
            }),
        }
    }

    /// The row labels (test/render seam).
    pub fn labels(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .labels
            .clone()
    }

    /// The selected row's auth type (`None` for an empty selector).
    pub fn selected(&self) -> Option<AuthTypeChoice> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.choices.get(state.selected).copied()
    }

    pub fn handle_key(&self, key: AuthKey) -> AuthOutcome {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match key {
            AuthKey::Up => {
                state.selected = state.selected.saturating_sub(1);
                AuthOutcome::None
            }
            AuthKey::Down => {
                let last = state.labels.len().saturating_sub(1);
                state.selected = (state.selected + 1).min(last);
                AuthOutcome::None
            }
            AuthKey::Enter => AuthOutcome::Select(state.selected),
            AuthKey::Esc => AuthOutcome::Cancel,
        }
    }
}

impl Component for AuthTypeSelector {
    fn render(&self, width: usize) -> Vec<String> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut frame = Container::new();
        frame.add_child(DynamicBorder::new());
        frame.add_child(Spacer::new(1));
        frame.add_child(Text::with_padding(
            with_theme(|theme| theme.fg(ThemeColor::Accent, &theme.bold(&state.title))),
            1,
            0,
        ));
        frame.add_child(Spacer::new(1));
        for (index, label) in state.labels.iter().enumerate() {
            let is_selected = index == state.selected;
            let prefix = if is_selected {
                with_theme(|theme| theme.fg(ThemeColor::Accent, "→ "))
            } else {
                "  ".to_string()
            };
            let row = if is_selected {
                with_theme(|theme| theme.fg(ThemeColor::Accent, label))
            } else {
                label.clone()
            };
            frame.add_child(Text::with_padding(format!("{prefix}{row}"), 1, 0));
            if let Some(Some(description)) = state.descriptions.get(index) {
                frame.add_child(Text::with_padding(
                    with_theme(|theme| theme.fg(ThemeColor::Muted, &format!("  {description}"))),
                    0,
                    0,
                ));
            }
        }
        frame.add_child(Spacer::new(1));
        frame.add_child(Text::with_padding(
            with_theme(|theme| {
                theme.fg(
                    ThemeColor::Dim,
                    "  ↑/↓ navigate · enter select · esc cancel",
                )
            }),
            1,
            0,
        ));
        frame.add_child(DynamicBorder::new());
        frame.render(width)
    }

    fn invalidate(&self) {}
}
