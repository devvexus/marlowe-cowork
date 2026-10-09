//! Who the user is: the name Marlowe uses and the form of address in greetings.

use std::fmt;
use std::str::FromStr;

/// Longest name or address the daemon accepts, in characters.
pub const USER_FIELD_MAX: usize = 64;

/// The three address choices offered by onboarding and `marlowe me --address`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressForm {
    /// "sir".
    Sir,
    /// The user's name.
    Name,
    /// No address in greetings.
    Nothing,
}

impl AddressForm {
    pub const ALL: [AddressForm; 3] = [AddressForm::Sir, AddressForm::Name, AddressForm::Nothing];

    /// The address string this choice stores; `Name` without a name is no address.
    pub fn resolve(self, name: Option<&str>) -> Option<String> {
        match self {
            AddressForm::Sir => Some("sir".into()),
            AddressForm::Name => name.map(str::trim).filter(|n| !n.is_empty()).map(str::to_string),
            AddressForm::Nothing => None,
        }
    }

    /// Which choice a stored address is. `None` for a custom form (e.g. set via env).
    pub fn classify(address: Option<&str>, name: Option<&str>) -> Option<AddressForm> {
        match address.map(str::trim).filter(|a| !a.is_empty()) {
            None => Some(AddressForm::Nothing),
            Some(a) if a.eq_ignore_ascii_case("sir") => Some(AddressForm::Sir),
            Some(a) if name.is_some_and(|n| n.trim() == a) => Some(AddressForm::Name),
            Some(_) => None,
        }
    }
}

impl fmt::Display for AddressForm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            AddressForm::Sir => "sir",
            AddressForm::Name => "name",
            AddressForm::Nothing => "none",
        })
    }
}

/// Not one of `sir`, `name`, `none`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown address '{0}' (sir, name, none)")]
pub struct AddressParseError(pub String);

impl FromStr for AddressForm {
    type Err = AddressParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "sir" => Ok(AddressForm::Sir),
            "name" => Ok(AddressForm::Name),
            "none" | "nothing" => Ok(AddressForm::Nothing),
            _ => Err(AddressParseError(s.to_string())),
        }
    }
}

/// Trims a name or address; blank is `None`. Refuses control characters (they would break the
/// system prompt's user block) and anything over [`USER_FIELD_MAX`] characters.
pub fn clean_user_field(what: &str, v: Option<&str>) -> Result<Option<String>, String> {
    let Some(v) = v.map(str::trim).filter(|v| !v.is_empty()) else { return Ok(None) };
    if v.chars().any(char::is_control) {
        return Err(format!("the {what} has a control character"));
    }
    if v.chars().count() > USER_FIELD_MAX {
        return Err(format!("the {what} is longer than {USER_FIELD_MAX} characters"));
    }
    Ok(Some(v.to_string()))
}
