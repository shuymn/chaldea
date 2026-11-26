//! Key encoding for `chaldea key` (`docs/design/supervisor-send.md` §2.6): named keys and
//! printable ASCII characters as the VT input bytes a terminal sends for them.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// The most tokens one `key` request carries.
pub const MAX_KEYS: usize = 32;

/// One key of a `key` request: a name (`"enter"`) or one printable ASCII character
/// (`{ "char": "y" }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum KeyToken {
    Enter,
    Escape,
    Tab,
    Backspace,
    CtrlC,
    Up,
    Down,
    Right,
    Left,
    /// One printable ASCII scalar, `' '` to `'~'`.
    Char(char),
}

/// Why no bytes were encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyError {
    /// No token, or more than [`MAX_KEYS`].
    Count,
    /// A `char` token that is not one printable ASCII scalar.
    NotPrintable,
}

impl KeyToken {
    fn bytes(self, app_cursor: bool, out: &mut Vec<u8>) -> Result<(), KeyError> {
        let arrow = |final_byte: u8, out: &mut Vec<u8>| {
            // DECCKM selects SS3 (`ESC O x`) over CSI (`ESC [ x`) for the cursor keys.
            out.extend_from_slice(if app_cursor { b"\x1bO" } else { b"\x1b[" });
            out.push(final_byte);
        };
        match self {
            KeyToken::Enter => out.push(b'\r'),
            KeyToken::Escape => out.push(0x1b),
            KeyToken::Tab => out.push(b'\t'),
            KeyToken::Backspace => out.push(0x7f),
            KeyToken::CtrlC => out.push(0x03),
            KeyToken::Up => arrow(b'A', out),
            KeyToken::Down => arrow(b'B', out),
            KeyToken::Right => arrow(b'C', out),
            KeyToken::Left => arrow(b'D', out),
            KeyToken::Char(c) => {
                if !(' '..='~').contains(&c) {
                    return Err(KeyError::NotPrintable);
                }
                out.push(c as u8);
            }
        }
        Ok(())
    }
}

/// The bytes of `tokens` in order, as one `Bytes` part: 1 to [`MAX_KEYS`] tokens, arrows in
/// SS3 form when `app_cursor` (DECCKM) is set.
///
/// # Errors
///
/// [`KeyError::Count`] for no token or too many; [`KeyError::NotPrintable`] for a `char`
/// outside printable ASCII.
pub fn encode_keys(tokens: &[KeyToken], app_cursor: bool) -> Result<Vec<u8>, KeyError> {
    if tokens.is_empty() || tokens.len() > MAX_KEYS {
        return Err(KeyError::Count);
    }
    let mut out = Vec::with_capacity(tokens.len() * 3);
    for t in tokens {
        t.bytes(app_cursor, &mut out)?;
    }
    Ok(out)
}
