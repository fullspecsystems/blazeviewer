//! winit → [`PbKey`] adapter (NS0, ADR-021).
//!
//! The one place winit's `KeyCode` is translated into PhotoBlaze's shell-neutral
//! [`PbKey`]. Keeping this glue in its own module (rather than in `pb_key.rs`)
//! means the key *model* stays winit-free and moves into `pb-app-core` untouched,
//! while this adapter stays behind in the winit shell — the future macOS AppKit
//! host adds a parallel `NSEvent → PbKey` adapter without touching either.
//!
//! Mapping is 1:1 by physical position (variant names mirror each other). Keys
//! the keymap can't name — modifiers, exotic function keys — map to `None`; the
//! shell then ignores them, exactly as before (an unnamed key never produced a
//! usable binding).

use winit::keyboard::KeyCode;

use crate::pb_key::PbKey;

/// Translate a winit physical `KeyCode` into a [`PbKey`], or `None` if PhotoBlaze
/// doesn't model that key (modifiers and any key outside the bindable set).
pub fn from_winit(code: KeyCode) -> Option<PbKey> {
    let key = match code {
        KeyCode::Space => PbKey::Space,
        KeyCode::Backspace => PbKey::Backspace,
        KeyCode::Enter => PbKey::Enter,
        KeyCode::NumpadEnter => PbKey::NumpadEnter,
        KeyCode::Escape => PbKey::Escape,
        KeyCode::Tab => PbKey::Tab,
        KeyCode::Delete => PbKey::Delete,
        KeyCode::Insert => PbKey::Insert,
        KeyCode::Home => PbKey::Home,
        KeyCode::End => PbKey::End,
        KeyCode::PageUp => PbKey::PageUp,
        KeyCode::PageDown => PbKey::PageDown,
        KeyCode::ArrowLeft => PbKey::ArrowLeft,
        KeyCode::ArrowRight => PbKey::ArrowRight,
        KeyCode::ArrowUp => PbKey::ArrowUp,
        KeyCode::ArrowDown => PbKey::ArrowDown,
        KeyCode::Equal => PbKey::Equal,
        KeyCode::Minus => PbKey::Minus,
        KeyCode::Comma => PbKey::Comma,
        KeyCode::Period => PbKey::Period,
        KeyCode::Slash => PbKey::Slash,
        KeyCode::Backslash => PbKey::Backslash,
        KeyCode::Semicolon => PbKey::Semicolon,
        KeyCode::Quote => PbKey::Quote,
        KeyCode::BracketLeft => PbKey::BracketLeft,
        KeyCode::BracketRight => PbKey::BracketRight,
        KeyCode::Backquote => PbKey::Backquote,
        KeyCode::NumpadAdd => PbKey::NumpadAdd,
        KeyCode::NumpadSubtract => PbKey::NumpadSubtract,
        KeyCode::NumpadMultiply => PbKey::NumpadMultiply,
        KeyCode::NumpadDivide => PbKey::NumpadDivide,
        KeyCode::NumpadDecimal => PbKey::NumpadDecimal,
        KeyCode::Digit0 => PbKey::Digit0,
        KeyCode::Digit1 => PbKey::Digit1,
        KeyCode::Digit2 => PbKey::Digit2,
        KeyCode::Digit3 => PbKey::Digit3,
        KeyCode::Digit4 => PbKey::Digit4,
        KeyCode::Digit5 => PbKey::Digit5,
        KeyCode::Digit6 => PbKey::Digit6,
        KeyCode::Digit7 => PbKey::Digit7,
        KeyCode::Digit8 => PbKey::Digit8,
        KeyCode::Digit9 => PbKey::Digit9,
        KeyCode::Numpad0 => PbKey::Numpad0,
        KeyCode::Numpad1 => PbKey::Numpad1,
        KeyCode::Numpad2 => PbKey::Numpad2,
        KeyCode::Numpad3 => PbKey::Numpad3,
        KeyCode::Numpad4 => PbKey::Numpad4,
        KeyCode::Numpad5 => PbKey::Numpad5,
        KeyCode::Numpad6 => PbKey::Numpad6,
        KeyCode::Numpad7 => PbKey::Numpad7,
        KeyCode::Numpad8 => PbKey::Numpad8,
        KeyCode::Numpad9 => PbKey::Numpad9,
        KeyCode::KeyA => PbKey::KeyA,
        KeyCode::KeyB => PbKey::KeyB,
        KeyCode::KeyC => PbKey::KeyC,
        KeyCode::KeyD => PbKey::KeyD,
        KeyCode::KeyE => PbKey::KeyE,
        KeyCode::KeyF => PbKey::KeyF,
        KeyCode::KeyG => PbKey::KeyG,
        KeyCode::KeyH => PbKey::KeyH,
        KeyCode::KeyI => PbKey::KeyI,
        KeyCode::KeyJ => PbKey::KeyJ,
        KeyCode::KeyK => PbKey::KeyK,
        KeyCode::KeyL => PbKey::KeyL,
        KeyCode::KeyM => PbKey::KeyM,
        KeyCode::KeyN => PbKey::KeyN,
        KeyCode::KeyO => PbKey::KeyO,
        KeyCode::KeyP => PbKey::KeyP,
        KeyCode::KeyQ => PbKey::KeyQ,
        KeyCode::KeyR => PbKey::KeyR,
        KeyCode::KeyS => PbKey::KeyS,
        KeyCode::KeyT => PbKey::KeyT,
        KeyCode::KeyU => PbKey::KeyU,
        KeyCode::KeyV => PbKey::KeyV,
        KeyCode::KeyW => PbKey::KeyW,
        KeyCode::KeyX => PbKey::KeyX,
        KeyCode::KeyY => PbKey::KeyY,
        KeyCode::KeyZ => PbKey::KeyZ,
        KeyCode::F1 => PbKey::F1,
        KeyCode::F2 => PbKey::F2,
        KeyCode::F3 => PbKey::F3,
        KeyCode::F4 => PbKey::F4,
        KeyCode::F5 => PbKey::F5,
        KeyCode::F6 => PbKey::F6,
        KeyCode::F7 => PbKey::F7,
        KeyCode::F8 => PbKey::F8,
        KeyCode::F9 => PbKey::F9,
        KeyCode::F10 => PbKey::F10,
        KeyCode::F11 => PbKey::F11,
        KeyCode::F12 => PbKey::F12,
        _ => return None,
    };
    Some(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_representative_keys() {
        assert_eq!(from_winit(KeyCode::KeyS), Some(PbKey::KeyS));
        assert_eq!(from_winit(KeyCode::Space), Some(PbKey::Space));
        assert_eq!(from_winit(KeyCode::Escape), Some(PbKey::Escape));
        assert_eq!(from_winit(KeyCode::ArrowLeft), Some(PbKey::ArrowLeft));
        assert_eq!(from_winit(KeyCode::Equal), Some(PbKey::Equal));
        assert_eq!(from_winit(KeyCode::NumpadEnter), Some(PbKey::NumpadEnter));
        assert_eq!(from_winit(KeyCode::F11), Some(PbKey::F11));
    }

    #[test]
    fn unmodelled_keys_map_to_none() {
        // Modifiers are tracked as chord flags, never as the key itself.
        assert_eq!(from_winit(KeyCode::ShiftLeft), None);
        assert_eq!(from_winit(KeyCode::ControlRight), None);
        assert_eq!(from_winit(KeyCode::SuperLeft), None);
        // Exotic keys the keymap can't name.
        assert_eq!(from_winit(KeyCode::F13), None);
        assert_eq!(from_winit(KeyCode::PrintScreen), None);
    }

    #[test]
    fn adapter_output_always_round_trips_through_the_name() {
        // Anything the adapter accepts must be a fully-named key (so a captured
        // chord can be displayed and persisted).
        for code in [
            KeyCode::KeyR,
            KeyCode::Digit0,
            KeyCode::NumpadAdd,
            KeyCode::Slash,
            KeyCode::Backquote,
        ] {
            let key = from_winit(code).unwrap();
            assert_eq!(PbKey::from_name(key.as_str()), Some(key));
        }
    }
}
