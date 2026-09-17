//! Keyboard accelerators, as written in the config file.
//!
//! A binding is stored as text — `"Ctrl+Alt+V"`, `"F1"` — and has to mean exactly
//! the same thing to the two sides that touch it: the settings window, which
//! captures a key press and writes the text, and the hotkey thread, which reads
//! the text and registers it with Windows. Parsing in both places, or formatting
//! in one and parsing in the other with slightly different rules, is how a
//! binding ends up displayed as one thing and registered as another.

use std::fmt;

/// A modifier key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modifier {
    Control,
    Alt,
    Shift,
    /// The Windows key.
    Win,
}

impl Modifier {
    /// The order they are printed in, which is also the order they are read in.
    pub const ALL: [Modifier; 4] = [
        Modifier::Control,
        Modifier::Alt,
        Modifier::Shift,
        Modifier::Win,
    ];

    const fn bit(self) -> u8 {
        match self {
            Modifier::Control => 1,
            Modifier::Alt => 2,
            Modifier::Shift => 4,
            Modifier::Win => 8,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Modifier::Control => "Ctrl",
            Modifier::Alt => "Alt",
            Modifier::Shift => "Shift",
            Modifier::Win => "Win",
        }
    }
}

/// Which modifiers are held, as a set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers(u8);

impl Modifiers {
    pub const NONE: Self = Self(0);

    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn contains(self, modifier: Modifier) -> bool {
        self.0 & modifier.bit() != 0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Add or remove one modifier, for a capture that is still in progress.
    pub fn with(self, modifier: Modifier, held: bool) -> Self {
        if held {
            Self(self.0 | modifier.bit())
        } else {
            Self(self.0 & !modifier.bit())
        }
    }
}

/// A key together with the modifiers held with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub modifiers: Modifiers,
    pub virtual_key: u32,
}

impl Modifiers {
    /// The modifiers as they are written in a spec, with a trailing `+`:
    /// `"Ctrl+Alt+"`. Empty when none are held.
    ///
    /// Used while a combination is being recorded, where the key is not known
    /// yet and the text has to grow as modifiers are pressed.
    pub fn prefix(self) -> String {
        let mut text = String::new();
        for modifier in Modifier::ALL {
            if self.contains(modifier) {
                text.push_str(modifier.label());
                text.push('+');
            }
        }
        text
    }
}

impl Binding {
    pub const fn new(modifiers: Modifiers, virtual_key: u32) -> Self {
        Self {
            modifiers,
            virtual_key,
        }
    }

    /// Can this be registered without shadowing ordinary typing?
    ///
    /// A bare letter or digit would be swallowed everywhere, so those need a
    /// modifier. A function key is not a character anyone types, and `F1`/`F3`
    /// are what a screenshot tool is expected to use — so those stand alone.
    pub fn is_usable(&self) -> bool {
        if !self.modifiers.is_empty() {
            return name_of(self.virtual_key).is_some();
        }
        is_function_key(self.virtual_key)
    }
}

impl fmt::Display for Binding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for modifier in Modifier::ALL {
            if self.modifiers.contains(modifier) {
                if !first {
                    f.write_str("+")?;
                }
                f.write_str(modifier.label())?;
                first = false;
            }
        }
        let key = name_of(self.virtual_key).unwrap_or("?");
        if first {
            f.write_str(key)
        } else {
            write!(f, "+{key}")
        }
    }
}

/// Is this virtual key one of `F1`..`F24`?
pub const fn is_function_key(virtual_key: u32) -> bool {
    virtual_key >= 0x70 && virtual_key <= 0x87
}

/// Is this virtual key a modifier by itself?
pub const fn is_modifier_key(virtual_key: u32) -> bool {
    matches!(virtual_key, 0x10 | 0x11 | 0x12 | 0x5B | 0x5C | 0xA0..=0xA5)
}

/// The canonical name of a key, or `None` when we do not name it.
///
/// Letters, digits and function keys cover every binding anyone writes here; the
/// rest is punctuation that a capture can produce without a name, which is why
/// `is_usable` refuses those rather than storing something unparseable.
pub fn name_of(virtual_key: u32) -> Option<&'static str> {
    match virtual_key {
        0x30..=0x39 => Some(DIGITS[(virtual_key - 0x30) as usize]),
        0x41..=0x5A => Some(LETTERS[(virtual_key - 0x41) as usize]),
        0x70..=0x87 => Some(FKEYS[(virtual_key - 0x70) as usize]),
        0x20 => Some("Space"),
        0x09 => Some("Tab"),
        0x0D => Some("Enter"),
        0x08 => Some("Backspace"),
        0x2E => Some("Delete"),
        0x2D => Some("Insert"),
        0x24 => Some("Home"),
        0x23 => Some("End"),
        0x21 => Some("PageUp"),
        0x22 => Some("PageDown"),
        0x26 => Some("Up"),
        0x28 => Some("Down"),
        0x25 => Some("Left"),
        0x27 => Some("Right"),
        0xC0 => Some("`"),
        0xBD => Some("-"),
        0xBB => Some("="),
        0xDB => Some("["),
        0xDD => Some("]"),
        0xDC => Some("\\"),
        0xBA => Some(";"),
        0xDE => Some("'"),
        0xBC => Some(","),
        0xBE => Some("."),
        0xBF => Some("/"),
        _ => None,
    }
}

const DIGITS: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];
const LETTERS: [&str; 26] = [
    "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S",
    "T", "U", "V", "W", "X", "Y", "Z",
];
const FKEYS: [&str; 24] = [
    "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12", "F13", "F14", "F15",
    "F16", "F17", "F18", "F19", "F20", "F21", "F22", "F23", "F24",
];

/// Parse a binding as written in the config file.
///
/// Accepts the modifiers in any order and any case, then exactly one key. `None`
/// for anything else — including a binding with no modifier on a character key,
/// which would shadow typing everywhere.
pub fn parse(spec: &str) -> Option<Binding> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }

    let mut modifiers = Modifiers::NONE;
    let mut key: Option<u32> = None;

    for part in spec.split('+').map(str::trim).filter(|part| !part.is_empty()) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers = modifiers.with(Modifier::Control, true),
            "alt" => modifiers = modifiers.with(Modifier::Alt, true),
            "shift" => modifiers = modifiers.with(Modifier::Shift, true),
            "win" | "super" | "meta" | "cmd" => modifiers = modifiers.with(Modifier::Win, true),
            other => {
                if key.is_some() {
                    // Two keys is a typo, not a binding.
                    return None;
                }
                key = Some(key_of(other)?);
            }
        }
    }

    let binding = Binding::new(modifiers, key?);
    binding.is_usable().then_some(binding)
}

/// The virtual key for a written key name.
fn key_of(name: &str) -> Option<u32> {
    let mut chars = name.chars();
    if let (Some(letter), None) = (chars.next(), chars.next()) {
        if letter.is_ascii_alphabetic() {
            return Some(letter.to_ascii_uppercase() as u32);
        }
        if letter.is_ascii_digit() {
            return Some(letter as u32);
        }
    }
    if let Some(rest) = name.strip_prefix('f') {
        if let Ok(number) = rest.parse::<u32>() {
            if (1..=24).contains(&number) {
                return Some(0x70 + number - 1);
            }
        }
    }
    // Anything else has to be a name this module itself prints, so the two
    // directions cannot drift.
    (0x01..=0xFF).find(|vk| {
        name_of(*vk).is_some_and(|candidate| candidate.eq_ignore_ascii_case(name))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_spellings() {
        let binding = parse("Ctrl+Alt+V").unwrap();
        assert_eq!(binding.virtual_key, 'V' as u32);
        assert!(binding.modifiers.contains(Modifier::Control));
        assert!(binding.modifiers.contains(Modifier::Alt));
        assert!(!binding.modifiers.contains(Modifier::Shift));

        assert_eq!(parse("alt+ctrl+v").unwrap(), binding, "order and case vary");
        assert_eq!(parse("CTRL + ALT + V").unwrap(), binding);
    }

    #[test]
    fn win_key_spellings_are_accepted() {
        for spec in ["Win+1", "Super+1", "Meta+1", "cmd+1"] {
            assert!(
                parse(spec).unwrap().modifiers.contains(Modifier::Win),
                "{spec}"
            );
        }
    }

    #[test]
    fn a_bare_function_key_is_allowed_and_a_bare_letter_is_not() {
        // F1 and F3 are what a screenshot tool is used with; a bare "V" would
        // swallow the key everywhere.
        assert_eq!(parse("F1").unwrap(), Binding::new(Modifiers::NONE, 0x70));
        assert_eq!(parse("f3").unwrap().virtual_key, 0x72);
        assert_eq!(parse("F24").unwrap().virtual_key, 0x87);

        assert!(parse("V").is_none());
        assert!(parse("1").is_none());
        assert!(parse("F25").is_none(), "there is no F25");
    }

    #[test]
    fn nonsense_is_rejected() {
        for spec in ["", "   ", "Ctrl+", "Ctrl+Alt", "Ctrl+V+B", "Ctrl+Banana"] {
            assert!(parse(spec).is_none(), "{spec:?} should not parse");
        }
    }

    #[test]
    fn every_printed_binding_parses_back_to_itself() {
        // The round trip is the property that matters: the settings window
        // writes what it prints, and the hotkey thread reads it.
        let keys = [0x70u32, 0x72, 0x87, 'V' as u32, '1' as u32, 0x20, 0x2E, 0x26];
        for virtual_key in keys {
            for bits in [0u8, 1, 2, 4, 8, 3, 15] {
                let binding = Binding::new(Modifiers::from_bits(bits), virtual_key);
                if !binding.is_usable() {
                    continue;
                }
                let text = binding.to_string();
                assert_eq!(parse(&text), Some(binding), "{text} did not round trip");
            }
        }
    }

    #[test]
    fn the_prefix_grows_as_modifiers_are_pressed() {
        assert_eq!(Modifiers::NONE.prefix(), "");
        let one = Modifiers::NONE.with(Modifier::Control, true);
        assert_eq!(one.prefix(), "Ctrl+");
        let two = one.with(Modifier::Alt, true);
        assert_eq!(two.prefix(), "Ctrl+Alt+");
        assert_eq!(two.with(Modifier::Alt, false).prefix(), "Ctrl+");
    }

    #[test]
    fn modifier_keys_are_recognised() {
        for vk in [0x10, 0x11, 0x12, 0x5B, 0x5C, 0xA0, 0xA5] {
            assert!(is_modifier_key(vk), "{vk:#x}");
        }
        assert!(!is_modifier_key('A' as u32));
    }

    #[test]
    fn a_capture_can_be_assembled_from_the_keyboard_state() {
        // This is what the settings window does while it is recording: the
        // modifiers come from `GetKeyState`, the key from the message.
        let mut modifiers = Modifiers::NONE;
        for (modifier, held) in [
            (Modifier::Control, true),
            (Modifier::Alt, true),
            (Modifier::Shift, false),
            (Modifier::Win, false),
        ] {
            modifiers = modifiers.with(modifier, held);
        }
        let binding = Binding::new(modifiers, 'A' as u32);
        assert_eq!(binding.to_string(), "Ctrl+Alt+A");
        assert!(binding.is_usable());
    }
}
