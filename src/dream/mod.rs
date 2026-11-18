//! The key choice older projects and presets stored for Dream Eater.
//!
//! Dream Eater no longer tunes a chord, so the choice does not change the sound. It is still
//! read and written so a project or preset from before that change loads, and still round-trips.

use serde::{Deserialize, Serialize};

/// Pitch class names, C first.
pub const KEY_NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
/// How keys are stored in the plugin state.
const KEY_IDS: [&str; 12] = ["c", "c#", "d", "d#", "e", "f", "f#", "g", "g#", "a", "a#", "b"];

/// The key a saved Dream Eater state names. Kept so old projects load; the spectral gate ignores it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub enum KeyChoice {
    /// What a project saved before a key was chosen, and what factory presets store.
    #[default]
    Auto,
    /// A pitch class, C = 0.
    Fixed(u8),
}

impl KeyChoice {
    /// Auto, then the twelve keys from C.
    pub fn all() -> impl Iterator<Item = KeyChoice> {
        std::iter::once(KeyChoice::Auto).chain((0..12).map(KeyChoice::Fixed))
    }

    /// Auto, or just the root, as in C or F#.
    pub fn label(self) -> String {
        match self {
            KeyChoice::Auto => "Auto".to_owned(),
            KeyChoice::Fixed(key) => KEY_NAMES[key as usize % 12].to_owned(),
        }
    }

    pub fn to_bits(self) -> u8 {
        match self {
            KeyChoice::Auto => u8::MAX,
            KeyChoice::Fixed(key) => key % 12,
        }
    }

    pub fn from_bits(bits: u8) -> Self {
        if bits < 12 {
            KeyChoice::Fixed(bits)
        } else {
            KeyChoice::Auto
        }
    }
}

impl From<KeyChoice> for String {
    fn from(choice: KeyChoice) -> String {
        match choice {
            KeyChoice::Auto => "auto".to_owned(),
            KeyChoice::Fixed(key) => KEY_IDS[key as usize % 12].to_owned(),
        }
    }
}

impl TryFrom<String> for KeyChoice {
    type Error = String;

    fn try_from(id: String) -> Result<Self, String> {
        let id = id.trim().to_ascii_lowercase();
        if id == "auto" {
            return Ok(KeyChoice::Auto);
        }
        KEY_IDS
            .iter()
            .position(|known| *known == id)
            .map(|key| KeyChoice::Fixed(key as u8))
            .ok_or_else(|| format!("{id} is not a key"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_choices_round_trip_through_their_ids() {
        assert_eq!(KeyChoice::all().count(), 13);
        for choice in KeyChoice::all() {
            let json = serde_json::to_string(&choice).unwrap();
            assert_eq!(serde_json::from_str::<KeyChoice>(&json).unwrap(), choice, "{json}");
            assert_eq!(KeyChoice::from_bits(choice.to_bits()), choice);
        }
        assert_eq!(serde_json::to_string(&KeyChoice::Auto).unwrap(), r#""auto""#);
        assert_eq!(serde_json::to_string(&KeyChoice::Fixed(1)).unwrap(), r#""c#""#);
        assert!(serde_json::from_str::<KeyChoice>(r#""h""#).is_err());
        assert_eq!(
            KeyChoice::all().map(KeyChoice::label).collect::<Vec<_>>(),
            ["Auto", "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"]
        );
    }
}
