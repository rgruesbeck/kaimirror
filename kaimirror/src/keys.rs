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
    // MENU is the left soft key under the name KaiOS gives its usual job;
    // HELP is what the keylayout calls FAVORITE_CONTACTS.
    ("MENU", 1, 139), ("HELP", 1, 138),
    ("CALL", 1, 231), ("SEND", 1, 231),
    ("STAR", 1, 522), ("POUND", 1, 523),
    // Read out of /system/usr/keylayout/matrix-keypad.kl: 30 MESSAGE, 48 DEL,
    // 139 SOFT_LEFT, 158 SOFT_RIGHT.  An earlier table had 30/48 as the soft
    // keys, which is why "SOFT_LEFT" opened the Messages app.
    ("SOFT_LEFT", 1, 139), ("SOFT_RIGHT", 1, 158),
    // BACK is 48, not 158, and that was settled by watching `getevent` while
    // the handset's own back key was pressed: it emits `KEY_B`, which is code
    // 48 -- the one the keylayout calls DEL.  Back and delete are the same
    // physical key, and that is not a quirk of the table but of the phone: it
    // deletes a character where there is one to delete and goes back where
    // there is not.  Which is why a stray DEL on an empty field used to close
    // the app it was typing into.  158 is the right *soft* key, and only that.
    ("BACK", 1, 48), ("DEL", 1, 48), ("BACKSPACE", 1, 48),
    ("MESSAGE", 1, 30),
    // The red key, likewise measured rather than guessed: it emits KEY_POWER
    // on node 0, the same switch as the power button, so the phone decides
    // from press length whether it means "go home" or "blank the screen".
    // Sending it is exactly what the handset sends; it is not a home key.
    ("END", 0, 116), ("RED", 0, 116), ("POWER", 0, 116),
    ("VOLUMEDOWN", 0, 114),
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

/// Terminal keystrokes -> device keys, for `view --control`.
///
/// The phone keys that are not on a keyboard sit on the navigation cluster --
/// Ins, Del, PgUp, PgDn -- rather than on punctuation.  Punctuation was worse
/// than it looked: `,` and `.` are characters someone will want to *type*, and
/// having them mean soft keys in one mode and commas in the other is the kind
/// of split nobody can predict.  The cluster keys mean nothing to a phone
/// text field, so they can navigate in both modes, the way the arrows do.
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
        "\x1b[2~" => "SOFT_LEFT",   // Ins
        "\x1b[5~" => "SOFT_RIGHT",  // PgUp
        "\x1b[3~" => "CALL",        // Del
        "\x1b[6~" => "END",         // PgDn -- the red key
        "m" => "MENU",
        "-" => "VOLUMEDOWN",
        "+" => "VOLUMEUP",
        d if d.len() == 1 && d.as_bytes()[0].is_ascii_digit() => {
            return KEYS.iter().find(|(n, ..)| *n == d).map(|&(n, ..)| n)
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_named_keys_are_all_findable_case_insensitively() {
        for (name, node, code) in KEYS {
            assert_eq!(lookup(name), Some((*node, *code)));
            assert_eq!(lookup(&name.to_lowercase()), Some((*node, *code)));
        }
        assert_eq!(lookup("nosuchkey"), None);
    }

    /// The navigation cluster carries the keys a keyboard has no equivalent
    /// for.  Whole sequences, four bytes each -- the reader has to gather a
    /// CSI sequence to its final byte or these arrive truncated.
    #[test]
    fn the_navigation_cluster_reaches_the_phone_keys() {
        assert_eq!(from_keystroke("\x1b[2~"), Some("SOFT_LEFT"));
        assert_eq!(from_keystroke("\x1b[5~"), Some("SOFT_RIGHT"));
        assert_eq!(from_keystroke("\x1b[3~"), Some("CALL"));
    }

    /// The punctuation those keys moved off has to be free again, or text
    /// mode still cannot type a comma.
    #[test]
    fn punctuation_no_longer_means_a_phone_key() {
        for seq in [",", ".", "c"] {
            assert_eq!(from_keystroke(seq), None, "{seq:?} should be typeable");
        }
    }

    /// Measured with `getevent` while the handset's own keys were pressed --
    /// the back key emits `KEY_B` (48) and the red key `KEY_POWER` (116) on
    /// the power node.  Back and delete really are one key; the soft key is a
    /// different one, and calling *it* BACK is what sent "back" to the wrong
    /// place for so long.
    #[test]
    fn back_is_the_delete_key_and_not_a_soft_key() {
        assert_eq!(lookup("BACK"), Some((1, 48)));
        assert_eq!(lookup("BACK"), lookup("DEL"));
        assert_ne!(lookup("BACK"), lookup("SOFT_RIGHT"));
        assert_eq!(lookup("SOFT_RIGHT"), Some((1, 158)));
    }

    /// The red key is the power switch, so this must not quietly become a
    /// second name for something gentler.
    #[test]
    fn the_red_key_is_the_power_node() {
        assert_eq!(lookup("END"), Some((0, 116)));
        assert_eq!(lookup("END"), lookup("POWER"));
        assert_eq!(from_keystroke("\x1b[6~"), Some("END"));
    }
}
