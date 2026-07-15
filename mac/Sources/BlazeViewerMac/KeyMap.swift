import AppKit

/// macOS virtual key code (Carbon `kVK_*`) → the `PbKey` **name** the Rust core parses
/// (`PbKey::from_name` spellings: `"Space"`, `"Escape"`, `"Left"`, `"C"`, `"F5"`, … — NOT
/// winit's `"ArrowRight"`/`"KeyC"`). Virtual key codes are *positional* (layout-independent
/// for the ANSI block), matching `PbKey`'s physical-key model. The full NS1 item-4 map —
/// every key the keymap can bind. Modifier keys are absent by design (they ride as the
/// chord's flags, never as the key itself); numpad digits are unmapped for now
/// (`PbKey::from_name` has no spelling for them — parity with unbound winit numpad digits).
enum KeyMap {
    static func pbKeyName(for keyCode: UInt16) -> String? {
        switch keyCode {
        // Letters (ANSI positions).
        case 0x00: return "A"
        case 0x0B: return "B"
        case 0x08: return "C"
        case 0x02: return "D"
        case 0x0E: return "E"
        case 0x03: return "F"
        case 0x05: return "G"
        case 0x04: return "H"
        case 0x22: return "I"
        case 0x26: return "J"
        case 0x28: return "K"
        case 0x25: return "L"
        case 0x2E: return "M"
        case 0x2D: return "N"
        case 0x1F: return "O"
        case 0x23: return "P"
        case 0x0C: return "Q"
        case 0x0F: return "R"
        case 0x01: return "S"
        case 0x11: return "T"
        case 0x20: return "U"
        case 0x09: return "V"
        case 0x0D: return "W"
        case 0x07: return "X"
        case 0x10: return "Y"
        case 0x06: return "Z"
        // Digit row.
        case 0x1D: return "0"
        case 0x12: return "1"
        case 0x13: return "2"
        case 0x14: return "3"
        case 0x15: return "4"
        case 0x17: return "5"
        case 0x16: return "6"
        case 0x1A: return "7"
        case 0x1C: return "8"
        case 0x19: return "9"
        // Whitespace / editing.
        case 0x31: return "Space"
        case 0x24: return "Return"
        case 0x4C: return "NumpadEnter"
        case 0x30: return "Tab"
        case 0x33: return "Backspace" // kVK_Delete — the big backspace key
        case 0x75: return "Delete" // kVK_ForwardDelete
        case 0x35: return "Escape"
        // Navigation block.
        case 0x72: return "Insert" // kVK_Help — the Insert position on PC keyboards
        case 0x73: return "Home"
        case 0x77: return "End"
        case 0x74: return "PageUp"
        case 0x79: return "PageDown"
        case 0x7B: return "Left"
        case 0x7C: return "Right"
        case 0x7E: return "Up"
        case 0x7D: return "Down"
        // Punctuation (ANSI positions).
        case 0x18: return "Equal"
        case 0x1B: return "Minus"
        case 0x2B: return "Comma"
        case 0x2F: return "Period"
        case 0x2C: return "Slash"
        case 0x2A: return "Backslash"
        case 0x29: return "Semicolon"
        case 0x27: return "Quote"
        case 0x21: return "["
        case 0x1E: return "]"
        case 0x32: return "`"
        // Numpad operators.
        case 0x45: return "NumpadAdd"
        case 0x4E: return "NumpadSubtract"
        case 0x43: return "NumpadMultiply"
        case 0x4B: return "NumpadDivide"
        case 0x41: return "NumpadDecimal"
        // Function row.
        case 0x7A: return "F1"
        case 0x78: return "F2"
        case 0x63: return "F3"
        case 0x76: return "F4"
        case 0x60: return "F5"
        case 0x61: return "F6"
        case 0x62: return "F7"
        case 0x64: return "F8"
        case 0x65: return "F9"
        case 0x6D: return "F10"
        case 0x67: return "F11"
        case 0x6F: return "F12"
        default: return nil
        }
    }
}
