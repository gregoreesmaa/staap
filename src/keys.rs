//! Keystroke → PTY bytes, framework-free (regression shield).
//!
//! Shared by the gpui shell (`gui::spawn::forward_key`, re-exported here)
//! and the native shells (via `shell::encode_key` / the `staap_key_*` FFI):
//! one key table, pinned by unit tests, surviving toolkit upgrades
//! untouched. Moved out of `gui/` so the framework-free core owns it —
//! the gpui shell is one consumer among four, not the owner.

/// Plain key data extracted from a windowing key event.
pub struct KeyPress<'a> {
    /// Logical key name (`"a"`, `"enter"`, `"up"`, `"f5"`, …).
    pub key: &'a str,
    /// Typed character, if the key produces one (`"ß"` for option-s, …).
    pub key_char: Option<&'a str>,
    pub ctrl: bool,
    pub alt: bool,
}

/// Encode a keypress as the raw bytes a PTY child expects. `None` means the
/// app shell handles the key itself (focus keys, quit, …).
pub fn keystroke_to_pty(p: &KeyPress) -> Option<Vec<u8>> {
    // Ctrl+letter → control byte (Ctrl+C = ETX). Takes precedence so a
    // terminal can always be interrupted, whatever the key name is.
    if p.ctrl {
        if let Some(c) = p.key.chars().next().filter(|c| c.is_ascii_alphabetic()) {
            let upper = c.to_ascii_uppercase() as u8;
            if (b'@'..=b'_').contains(&upper) {
                return Some(vec![upper - b'@']);
            }
        }
        // Ctrl+D on an empty line ends most REPL input.
        if p.key.eq_ignore_ascii_case("d") {
            return Some(vec![0x04]);
        }
        return None;
    }
    // Word-jump on Alt+arrows (issue #113): named nav keys honor `alt`
    // by NAME, before the printable fallback, so a layout-typed char
    // (Estonian `~` on Option+arrows) never changes the bytes.
    // Alt+Left/Right are readline word jumps (`ESC b` / `ESC f`,
    // what bash/zsh forward-word expects); Alt+Up/Down stay unbound:
    // no shell emits a modified-CSI form, so None beats invented bytes.
    if p.alt {
        match p.key {
            "left" => return Some(vec![0x1b, b'b']),
            "right" => return Some(vec![0x1b, b'f']),
            "up" | "down" => return None,
            _ => {}
        }
    }
    let bytes: Vec<u8> = match p.key {
        "enter" => b"\r".to_vec(),
        "backspace" => vec![0x7f],
        "tab" => b"\t".to_vec(),
        "escape" => b"\x1b".to_vec(),
        "delete" => b"\x1b[3~".to_vec(),
        "insert" => b"\x1b[2~".to_vec(),
        "up" => b"\x1b[A".to_vec(),
        "down" => b"\x1b[B".to_vec(),
        "right" => b"\x1b[C".to_vec(),
        "left" => b"\x1b[D".to_vec(),
        "home" => b"\x1b[H".to_vec(),
        "end" => b"\x1b[F".to_vec(),
        "pageup" => b"\x1b[5~".to_vec(),
        "pagedown" => b"\x1b[6~".to_vec(),
        "f1" => b"\x1bOP".to_vec(),
        "f2" => b"\x1bOQ".to_vec(),
        "f3" => b"\x1bOR".to_vec(),
        "f4" => b"\x1bOS".to_vec(),
        "f5" => b"\x1b[15~".to_vec(),
        "f6" => b"\x1b[17~".to_vec(),
        "f7" => b"\x1b[18~".to_vec(),
        "f8" => b"\x1b[19~".to_vec(),
        "f9" => b"\x1b[20~".to_vec(),
        "f10" => b"\x1b[21~".to_vec(),
        "f11" => b"\x1b[23~".to_vec(),
        "f12" => b"\x1b[24~".to_vec(),
        _ => {
            // Printable input: prefer the typed char (layout-correct),
            // fall back to a single-char key name.
            let text = match p.key_char {
                Some(c) if !c.is_empty() => c.to_string(),
                _ if p.key.chars().count() == 1 => p.key.to_string(),
                _ => return None,
            };
            let mut buf = Vec::new();
            if p.alt {
                buf.push(0x1b);
            }
            buf.extend_from_slice(text.as_bytes());
            buf
        }
    };
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press<'a>(key: &'a str, key_char: Option<&'a str>) -> KeyPress<'a> {
        KeyPress {
            key,
            key_char,
            ctrl: false,
            alt: false,
        }
    }

    #[test]
    fn typing_keys_encode_to_pty_bytes() {
        assert_eq!(
            keystroke_to_pty(&press("a", Some("a"))),
            Some(b"a".to_vec())
        );
        assert_eq!(
            keystroke_to_pty(&press("ß", Some("ß"))),
            Some("ß".as_bytes().to_vec())
        );
        assert_eq!(
            keystroke_to_pty(&press("enter", None)),
            Some(b"\r".to_vec())
        );
        assert_eq!(
            keystroke_to_pty(&press("backspace", None)),
            Some(vec![0x7f])
        );
    }

    #[test]
    fn navigation_keys_encode_csi() {
        assert_eq!(
            keystroke_to_pty(&press("up", None)),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            keystroke_to_pty(&press("pagedown", None)),
            Some(b"\x1b[6~".to_vec())
        );
        assert_eq!(
            keystroke_to_pty(&press("delete", None)),
            Some(b"\x1b[3~".to_vec())
        );
        assert_eq!(
            keystroke_to_pty(&press("f1", None)),
            Some(b"\x1bOP".to_vec())
        );
        assert_eq!(
            keystroke_to_pty(&press("f5", None)),
            Some(b"\x1b[15~".to_vec())
        );
    }

    #[test]
    fn ctrl_c_encodes_etx_and_ctrl_d_encodes_eot() {
        let c = KeyPress {
            key: "c",
            key_char: None,
            ctrl: true,
            alt: false,
        };
        assert_eq!(keystroke_to_pty(&c), Some(vec![0x03]));
        let d = KeyPress {
            key: "d",
            key_char: None,
            ctrl: true,
            alt: false,
        };
        assert_eq!(keystroke_to_pty(&d), Some(vec![0x04]));
        // Ctrl+letter by first letter (Ctrl+F = ACK).
        let x = KeyPress {
            key: "f5",
            key_char: None,
            ctrl: true,
            alt: false,
        };
        assert_eq!(keystroke_to_pty(&x), Some(vec![0x06]));
        // Ctrl+non-letter is app-handled.
        let y = KeyPress {
            key: " ",
            key_char: None,
            ctrl: true,
            alt: false,
        };
        assert_eq!(keystroke_to_pty(&y), None);
    }

    #[test]
    fn alt_prefixes_escape() {
        let s = KeyPress {
            key: "s",
            key_char: Some("ß"),
            ctrl: false,
            alt: true,
        };
        let mut expected = vec![0x1b];
        expected.extend_from_slice("ß".as_bytes());
        assert_eq!(keystroke_to_pty(&s), Some(expected));
    }

    #[test]
    fn unknown_multichar_keys_encode_to_nothing() {
        assert_eq!(keystroke_to_pty(&press("shift", None)), None);
        assert_eq!(keystroke_to_pty(&press("capslock", None)), None);
    }

    #[test]
    fn alt_arrows_emit_word_jumps_regardless_of_layout() {
        // Issue #113: Option+arrows must jump words on every layout.
        // Estonian types `~` for Option+arrows: the bytes must key off
        // the key NAME + alt flag and ignore `key_char` entirely.
        for (key, want) in [("left", b'b'), ("right", b'f')] {
            for key_char in [Some("~"), None] {
                let p = KeyPress {
                    key,
                    key_char,
                    ctrl: false,
                    alt: true,
                };
                assert_eq!(keystroke_to_pty(&p), Some(vec![0x1b, want]), "key={key:?}");
            }
        }
        // Plain arrows are unchanged (no word jump without alt).
        assert_eq!(
            keystroke_to_pty(&press("left", Some("~"))),
            Some(b"\x1b[D".to_vec())
        );
        assert_eq!(
            keystroke_to_pty(&press("right", None)),
            Some(b"\x1b[C".to_vec())
        );
        // Alt+Up/Down stay unbound: no shell emits a modified-CSI form,
        // so inventing bytes would fork the table (None instead).
        for key in ["up", "down"] {
            let p = KeyPress {
                key,
                key_char: Some("~"),
                ctrl: false,
                alt: true,
            };
            assert_eq!(keystroke_to_pty(&p), None, "key={key:?}");
        }
    }
}
