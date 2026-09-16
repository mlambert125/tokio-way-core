//! Parsing of config-file key and mouse combinations.

use enumflags2::{BitFlags, bitflags};
use std::collections::HashSet;
use tokio_way_backends::input::MouseButton;

/// A modifier key, matched on physical key rather than an xkb mask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[bitflags]
#[repr(u8)]
pub enum Modifier {
    Shift = 1,
    Ctrl = 2,
    Alt = 4,
    Super = 8,
}

// evdev keycodes for the modifier keys, left and right.
pub const KEY_LEFTSHIFT: u32 = 42;
pub const KEY_RIGHTSHIFT: u32 = 54;
pub const KEY_LEFTCTRL: u32 = 29;
pub const KEY_RIGHTCTRL: u32 = 97;
pub const KEY_LEFTALT: u32 = 56;
pub const KEY_RIGHTALT: u32 = 100;
pub const KEY_LEFTMETA: u32 = 125;
pub const KEY_RIGHTMETA: u32 = 126;

impl Modifier {
    /// Whether either physical key for this modifier is currently held.
    fn held(self, pressed_keys: &HashSet<u32>) -> bool {
        let (left, right) = match self {
            Self::Shift => (KEY_LEFTSHIFT, KEY_RIGHTSHIFT),
            Self::Ctrl => (KEY_LEFTCTRL, KEY_RIGHTCTRL),
            Self::Alt => (KEY_LEFTALT, KEY_RIGHTALT),
            Self::Super => (KEY_LEFTMETA, KEY_RIGHTMETA),
        };
        pressed_keys.contains(&left) || pressed_keys.contains(&right)
    }
}

/// Split a combo string into its modifiers and the trigger
fn split_modifiers(spec: &str) -> Result<(BitFlags<Modifier>, &str), String> {
    let mut modifiers = BitFlags::empty();
    let mut trigger = None;
    for part in spec.split('+').map(str::trim) {
        if part.is_empty() {
            return Err(format!("empty part in keybind \"{spec}\""));
        }
        match part.to_ascii_lowercase().as_str() {
            "shift" => modifiers |= Modifier::Shift,
            "ctrl" | "control" => modifiers |= Modifier::Ctrl,
            "alt" => modifiers |= Modifier::Alt,
            "super" | "logo" | "meta" | "win" => modifiers |= Modifier::Super,
            _ if trigger.is_some() => {
                return Err(format!("more than one key in keybind \"{spec}\""));
            }
            _ => trigger = Some(part),
        }
    }
    let trigger =
        trigger.ok_or_else(|| format!("keybind \"{spec}\" names no key, only modifiers"))?;
    Ok((modifiers, trigger))
}

/// A modifier set plus one key, parsed from a config string like `"Alt+F4"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyCombo {
    pub modifiers: BitFlags<Modifier>,
    pub key: u32,
}

impl KeyCombo {
    /// Parse a combo string: modifier names and one key name, joined by `+`,
    /// in any order and case-insensitive (`"Alt+F4"`, `"ctrl+shift+q"`).
    pub fn parse(spec: &str) -> Result<Self, String> {
        let (modifiers, name) = split_modifiers(spec)?;
        let key = key_by_name(name)
            .ok_or_else(|| format!("unknown key \"{name}\" in keybind \"{spec}\""))?;
        Ok(Self { modifiers, key })
    }

    /// Whether this combo is satisfied by a key press: `evdev_key` is this
    /// combo's key, and every modifier it names is currently held.
    pub fn matches(self, evdev_key: u32, pressed_keys: &HashSet<u32>) -> bool {
        evdev_key == self.key && self.modifiers.iter().all(|m| m.held(pressed_keys))
    }
}

/// A modifier set plus one mouse button, parsed from a config string like `"Alt+LeftClick"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseCombo {
    pub modifiers: BitFlags<Modifier>,
    pub button: MouseButton,
}

impl MouseCombo {
    /// Parse a combo string the same way [`KeyCombo::parse`] does, but with a mouse button instead of a key.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let (modifiers, name) = split_modifiers(spec)?;
        let button = button_by_name(name)
            .ok_or_else(|| format!("unknown mouse button \"{name}\" in keybind \"{spec}\""))?;
        Ok(Self { modifiers, button })
    }

    /// Whether this combo is satisfied by a button press and modifier keys
    pub fn matches(self, button: MouseButton, pressed_keys: &HashSet<u32>) -> bool {
        button == self.button && self.modifiers.iter().all(|m| m.held(pressed_keys))
    }
}

/// Look up a mouse button by the name a user would type in a config file.
fn button_by_name(name: &str) -> Option<MouseButton> {
    match name.to_ascii_uppercase().as_str() {
        "LEFT" | "LEFTCLICK" => Some(MouseButton::LEFT),
        "RIGHT" | "RIGHTCLICK" => Some(MouseButton::RIGHT),
        "MIDDLE" | "MIDDLECLICK" => Some(MouseButton::MIDDLE),
        _ => None,
    }
}

/// Look up a key by the name a user would type in a config file.
fn key_by_name(name: &str) -> Option<u32> {
    let code = match name.to_ascii_uppercase().as_str() {
        "A" => 30,
        "B" => 48,
        "C" => 46,
        "D" => 32,
        "E" => 18,
        "F" => 33,
        "G" => 34,
        "H" => 35,
        "I" => 23,
        "J" => 36,
        "K" => 37,
        "L" => 38,
        "M" => 50,
        "N" => 49,
        "O" => 24,
        "P" => 25,
        "Q" => 16,
        "R" => 19,
        "S" => 31,
        "T" => 20,
        "U" => 22,
        "V" => 47,
        "W" => 17,
        "X" => 45,
        "Y" => 21,
        "Z" => 44,
        "0" => 11,
        "1" => 2,
        "2" => 3,
        "3" => 4,
        "4" => 5,
        "5" => 6,
        "6" => 7,
        "7" => 8,
        "8" => 9,
        "9" => 10,
        "F1" => 59,
        "F2" => 60,
        "F3" => 61,
        "F4" => 62,
        "F5" => 63,
        "F6" => 64,
        "F7" => 65,
        "F8" => 66,
        "F9" => 67,
        "F10" => 68,
        "F11" => 87,
        "F12" => 88,
        "TAB" => 15,
        "ESC" | "ESCAPE" => 1,
        "SPACE" => 57,
        "ENTER" | "RETURN" => 28,
        "BACKSPACE" => 14,
        "INSERT" => 110,
        "DELETE" => 111,
        "HOME" => 102,
        "END" => 107,
        "PAGEUP" => 104,
        "PAGEDOWN" => 109,
        "UP" => 103,
        "DOWN" => 108,
        "LEFT" => 105,
        "RIGHT" => 106,
        _ => return None,
    };
    Some(code)
}
