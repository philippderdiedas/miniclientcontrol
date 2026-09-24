//! Who may use a screen, per mode: anyone, signed-in accounts only, or nobody.
//!
//! Below the venue's switches, never instead of them: `cast_enabled` and
//! `guest_pages_enabled` still stop a mode everywhere, and a screen can only
//! narrow what they allow. The presence check (`cast_auth`) runs after this,
//! unchanged -- an account says who, not that they are in the room.

use serde::{Deserialize, Serialize};

use super::ClaimMode;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    #[default]
    Anyone,
    Account,
    Off,
}

impl Access {
    fn from_column(raw: &str) -> Self {
        match raw {
            "account" => Access::Account,
            "off" => Access::Off,
            _ => Access::Anyone,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Access::Anyone => "anyone",
            Access::Account => "account",
            Access::Off => "off",
        }
    }

    /// This screen's access with the venue switch folded in: what a guest can
    /// actually do, which is what the guest page is told.
    pub fn effective(self, venue_on: bool) -> Self {
        if venue_on { self } else { Access::Off }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    Off,
    NeedsAccount,
}

pub fn decide(venue_on: bool, access: Access, signed_in: bool) -> Result<(), Refusal> {
    match access.effective(venue_on) {
        Access::Off => Err(Refusal::Off),
        Access::Account if !signed_in => Err(Refusal::NeedsAccount),
        _ => Ok(()),
    }
}

/// The screen's stored access for `mode`. A row that cannot be read is
/// `Anyone` -- the behaviour before this existed -- and logged, rather than
/// locking every guest out over a database hiccup.
pub async fn load(pool: &sqlx::SqlitePool, screen: &str, mode: ClaimMode) -> Access {
    let column = match mode {
        ClaimMode::Cast => "cast_access",
        ClaimMode::Page => "page_access",
    };
    let sql = format!("SELECT COALESCE({column}, 'anyone') FROM displays WHERE name = ?");
    match sqlx::query_scalar::<_, String>(&sql).bind(screen).fetch_optional(pool).await {
        Ok(raw) => raw.as_deref().map(Access::from_column).unwrap_or_default(),
        Err(e) => {
            tracing::error!("Failed to read {} of display {}: {}", column, screen, e);
            Access::Anyone
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_shape() {
        assert_eq!(serde_json::to_value(Access::Account).unwrap(), serde_json::json!("account"));
        let parsed: Access = serde_json::from_value(serde_json::json!("off")).unwrap();
        assert_eq!(parsed, Access::Off);
    }

    #[test]
    fn the_decision() {
        assert_eq!(decide(true, Access::Anyone, false), Ok(()));
        assert_eq!(decide(true, Access::Account, true), Ok(()));
        assert_eq!(decide(true, Access::Account, false), Err(Refusal::NeedsAccount));
        assert_eq!(decide(true, Access::Off, true), Err(Refusal::Off));
        // The venue switch wins over everything the screen says.
        assert_eq!(decide(false, Access::Anyone, true), Err(Refusal::Off));
        assert_eq!(decide(false, Access::Account, true), Err(Refusal::Off));
    }
}
