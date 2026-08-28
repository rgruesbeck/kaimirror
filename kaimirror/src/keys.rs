//! Linux input event codes, per `getevent -pl` on the device.
//!
//! The matrix-keypad (event1) carries the whole keypad; the power and volume
//! keys live on separate nodes because they are wired to different
//! controllers.  All of these were verified to register at the kernel level.

/// (name, input node, key code)
pub const KEYS: &[(&str, u32, u16)] = &[
    ("0", 1, 11), ("1", 1, 2), ("2", 1, 3), ("3", 1, 4), ("4", 1, 5),
    ("5", 1, 6), ("6", 1, 7), ("7", 1, 8), ("8", 1, 9), ("9", 1, 10),
    ("UP", 1, 103), ("DOWN", 1, 108), ("LEFT", 1, 105), ("RIGHT", 1, 106),
    ("OK", 1, 352), ("CENTER", 1, 352),
    ("BACK", 1, 158), ("MENU", 1, 139), ("HELP", 1, 138),
    ("CALL", 1, 231), ("SEND", 1, 231),
    ("STAR", 1, 522), ("POUND", 1, 523),
    ("SOFT_LEFT", 1, 30), ("SOFT_RIGHT", 1, 48),    // KEY_A / KEY_B
    ("POWER", 0, 116), ("VOLUMEDOWN", 0, 114),
    ("VOLUMEUP", 2, 115), ("CAMERA", 2, 212),
];

pub fn lookup(name: &str) -> Option<(u32, u16)> {
    let upper = name.to_uppercase();
    KEYS.iter().find(|(n, ..)| *n == upper).map(|&(_, node, code)| (node, code))
}

pub fn names() -> Vec<&'static str> {
    let mut v: Vec<&str> = KEYS.iter().map(|&(n, ..)| n).collect();
    v.sort_unstable();
    v
}

/// Terminal keystrokes -> device keys, for `view --control`.  Arrow keys
/// arrive as escape sequences; the rest are what a phone keypad has anyway.
pub fn from_keystroke(seq: &str) -> Option<&'static str> {
    Some(match seq {
        "\x1b[A" => "UP",
        "\x1b[B" => "DOWN",
        "\x1b[C" => "RIGHT",
        "\x1b[D" => "LEFT",
        "\r" | "\n" => "OK",
        "\x7f" | "\x08" => "BACK",
        "*" => "STAR",
        "#" => "POUND",
        "," => "SOFT_LEFT",
        "." => "SOFT_RIGHT",
        "m" => "MENU",
        "c" => "CALL",
        "-" => "VOLUMEDOWN",
        "+" => "VOLUMEUP",
        d if d.len() == 1 && d.as_bytes()[0].is_ascii_digit() => {
            return KEYS.iter().find(|(n, ..)| *n == d).map(|&(n, ..)| n)
        }
        _ => return None,
    })
}
