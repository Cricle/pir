//! Mode and phase domain types.
//!
//! These mirror the engine's `Mode`/`Phase` wire labels exactly. The host
//! persists labels (not the enum discriminants), so [`Mode::from_label`] and
//! [`Phase::from_label`] must stay in sync with [`Mode::label`] /
//! [`Phase::label`].

use serde::{Deserialize, Serialize};

/// The user-triggered session mode.
///
/// ```
/// use pir_builtin_mode::Mode;
///
/// assert_eq!(Mode::default(), Mode::Normal);
/// assert!(Mode::Plan.is_read_only());
/// assert!(!Mode::Normal.is_read_only());
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Normal,
    Foreman,
    Plan,
}

impl Mode {
    /// Every mode, in menu order.
    pub const ALL: [Self; 3] = [Self::Normal, Self::Foreman, Self::Plan];

    /// The mode for a persisted discriminant; unknown values fall back to
    /// [`Mode::Normal`].
    ///
    /// ```
    /// use pir_builtin_mode::Mode;
    ///
    /// assert_eq!(Mode::from_u8(1), Mode::Foreman);
    /// assert_eq!(Mode::from_u8(99), Mode::Normal);
    /// ```
    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Foreman,
            2 => Self::Plan,
            _ => Self::Normal,
        }
    }

    /// The wire label, as stored in settings and the journal.
    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Foreman => "foreman",
            Self::Plan => "plan",
        }
    }

    /// Parse a [`Mode::label`]; `None` for an unknown label.
    ///
    /// ```
    /// use pir_builtin_mode::Mode;
    ///
    /// assert_eq!(Mode::from_label("foreman"), Some(Mode::Foreman));
    /// assert_eq!(Mode::from_label("nope"), None);
    /// ```
    pub fn from_label(value: &str) -> Option<Self> {
        match value {
            "normal" => Some(Self::Normal),
            "foreman" => Some(Self::Foreman),
            "plan" => Some(Self::Plan),
            _ => None,
        }
    }

    /// Whether this mode is read-only for direct tools.
    pub fn is_read_only(self) -> bool {
        matches!(self, Self::Foreman | Self::Plan)
    }
}

/// The advisory phase rendered under the mode badge.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    #[default]
    Idle,
    Planning,
    Delegating,
    Review,
    Rework,
    Verified,
    Unverified,
}

impl Phase {
    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Planning,
            2 => Self::Delegating,
            3 => Self::Review,
            4 => Self::Rework,
            5 => Self::Verified,
            6 => Self::Unverified,
            _ => Self::Idle,
        }
    }

    /// The persisted/journal label (upper-case, as used by the engine).
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "IDLE",
            Self::Planning => "PLANNING",
            Self::Delegating => "DELEGATING",
            Self::Review => "REVIEW",
            Self::Rework => "REWORK",
            Self::Verified => "VERIFIED",
            Self::Unverified => "UNVERIFIED",
        }
    }

    pub fn from_label(value: &str) -> Option<Self> {
        match value {
            "IDLE" => Some(Self::Idle),
            "PLANNING" => Some(Self::Planning),
            "DELEGATING" => Some(Self::Delegating),
            "REVIEW" => Some(Self::Review),
            "REWORK" => Some(Self::Rework),
            "VERIFIED" => Some(Self::Verified),
            "UNVERIFIED" => Some(Self::Unverified),
            _ => None,
        }
    }

    /// Phase shown immediately after entering `mode`.
    pub fn initial_for(mode: Mode) -> Self {
        match mode {
            Mode::Normal => Self::Idle,
            Mode::Foreman | Mode::Plan => Self::Planning,
        }
    }
}
