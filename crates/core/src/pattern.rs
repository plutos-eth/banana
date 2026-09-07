//! A regex that survives a round trip through JSON.
//!
//! A saved strategy **is** its JSON file (spec §7.2), so a keyword rule has to serialise
//! as the pattern the user typed and come back as something that can match. Compiling on
//! load rather than on every evaluation also means a malformed pattern is rejected when
//! the strategy is opened, not silently when the first launch arrives.

use std::fmt;

use regex::Regex;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A compiled regular expression that serialises as its source string.
#[derive(Debug, Clone)]
pub struct Pattern {
    raw: String,
    re: Regex,
}

impl Pattern {
    /// Compile a pattern, rejecting it if it is malformed.
    pub fn new(raw: impl Into<String>) -> Result<Self, regex::Error> {
        let raw = raw.into();
        let re = Regex::new(&raw)?;
        Ok(Self { raw, re })
    }

    pub fn is_match(&self, haystack: &str) -> bool {
        self.re.is_match(haystack)
    }

    /// The pattern as written, for display in a refusal message.
    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

/// Two patterns are equal when their source is. Comparing compiled automata is neither
/// cheap nor meaningful; comparing what the user wrote is both.
impl PartialEq for Pattern {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}
impl Eq for Pattern {}

impl fmt::Display for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl Serialize for Pattern {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.raw)
    }
}

impl<'de> Deserialize<'de> for Pattern {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Pattern::new(&raw).map_err(|e| D::Error::custom(format!("invalid keyword regex: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_as_a_plain_string() {
        let p = Pattern::new("(?i)cat|dog").unwrap();
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, r#""(?i)cat|dog""#, "must save as what the user typed");
        assert_eq!(serde_json::from_str::<Pattern>(&json).unwrap(), p);
    }

    #[test]
    fn a_malformed_pattern_is_rejected_on_load_not_at_match_time() {
        assert!(Pattern::new("(unclosed").is_err());
        let err = serde_json::from_str::<Pattern>(r#""(unclosed""#).unwrap_err();
        assert!(err.to_string().contains("invalid keyword regex"));
    }

    #[test]
    fn matching_is_case_sensitive_unless_asked_otherwise() {
        assert!(!Pattern::new("waffle").unwrap().is_match("SpaceWaffle"));
        assert!(Pattern::new("(?i)waffle").unwrap().is_match("SpaceWaffle"));
    }
}
