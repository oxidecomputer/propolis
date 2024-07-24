// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

mod scan_code_1;
mod scan_code_2;

use std::convert::TryFrom;

use anyhow::{anyhow, Result};
use rfb::keysym::AsciiChar;
use rfb::keysym::KeySym::*;
use rfb::proto::KeyEvent;

use scan_code_1::*;
use scan_code_2::*;

use super::ctrl::PS2ScanCodeSet;

/// A struct that contains all information necessary to construct a scan code
/// for multiple scan code sets, as scan codes from all sets have a similar
/// structure.
///
/// Keyboards send a "make" code when a key is pressed, and a "break" code when
/// a key is released.
///
/// A key's make code is generally 1 byte, which here is referred to as the
/// "base value" of the scan code. Some keys are represented by the "extended"
/// set of codes, which have a prefix code indicating to the controller that the
/// next byte is also relevant. A handful of keys, for backwards compatibility
/// reasons, are represented by multiple scan codes.
///
/// So possible make codes are:
/// - 1 byte: [base value]
/// - 2 bytes: [extended prefix] + [base value]
/// - multibyte sequences composed of 2 or more make codes
///
/// A key's break code varies by scan code set. For Set 1, the highest bit of
/// the make code base value is set. For Set 2, an additional release code is
/// sent ahead of the make code, but after the extended prefix if one is used.
///
/// So possible break codes for Scan Code Set 1 are:
/// - 1 byte: [base value | release flag]
/// - 2 bytes: [extended prefix] + [base value | release flag]
/// - multibyte sequences composed of 2 or more break codes
///
/// And possible break codes for Scan Code Set 2 are:
/// - 2 bytes: [release code] + [base value]
/// - 3 bytes: [extended prefix] + [release code] + [base value]
/// - multibyte sequences composed of 2 or more break codes
///
/// Exceptions:
/// - In Set 2, the pause key does not have a break code.
///
#[derive(Debug)]
pub struct ScanCodeBase {
    base_val: u8,
    prefix: Option<Vec<u8>>,
}

// A representation of a key event that allows us to map keysyms, which are a
// much richer diversity of keys, to representation in scan codes, the set of
// which is more limited.
#[derive(Debug)]
pub struct KeyEventRep {
    pub keysym_raw: u32,
    pub is_pressed: bool,
    pub scan_code_1: ScanCodeBase,
    pub scan_code_2: ScanCodeBase,
}

impl KeyEventRep {
    /// Convert the given key to its scan code within a given scan code set.
    pub(crate) fn to_scan_code(&self, sc_set: PS2ScanCodeSet) -> Vec<u8> {
        let mut bytes = Vec::new();
        let sc = match sc_set {
            PS2ScanCodeSet::Set1 => &self.scan_code_1,
            PS2ScanCodeSet::Set2 => &self.scan_code_2,
        };

        if let Some(prefix) = &sc.prefix {
            bytes.extend(prefix);
        }

        if self.is_pressed {
            bytes.push(sc.base_val);
        } else {
            match sc_set {
                PS2ScanCodeSet::Set1 => {
                    bytes.push(sc.base_val | SC1_RELEASE_FLAG)
                }
                PS2ScanCodeSet::Set2 => {
                    bytes.push(SC2_RELEASE_CODE);
                    bytes.push(sc.base_val);
                }
            }
        }

        bytes
    }
}

impl TryFrom<KeyEvent> for KeyEventRep {
    type Error = anyhow::Error;

    fn try_from(keyevent: KeyEvent) -> Result<Self, Self::Error> {
        let (base_val_1, base_val_2) = match keyevent.keysym {
            Ascii(ascii_char) => match ascii_char {
                AsciiChar::BackSpace => (SC1_BACKSPACE, SC2_BACKSPACE),
                AsciiChar::Tab => (SC1_TAB, SC2_TAB),
                AsciiChar::ESC => (SC1_ESCAPE, SC2_ESCAPE),
                AsciiChar::Space => (SC1_SPACE, SC2_SPACE),
                AsciiChar::Apostrophe | AsciiChar::Quotation => {
                    (SC1_QUOTE_DBLQUOTE, SC2_QUOTE_DBLQUOTE)
                }
                AsciiChar::_1 | AsciiChar::Exclamation => {
                    (SC1_1_EXCLAMATION, SC2_1_EXCLAMATION)
                }
                AsciiChar::_2 | AsciiChar::At => (SC1_2_AT, SC2_2_AT),
                AsciiChar::_3 | AsciiChar::Hash => (SC1_3_HASH, SC2_3_HASH),
                AsciiChar::_4 | AsciiChar::Dollar => {
                    (SC1_4_DOLLAR, SC2_4_DOLLAR)
                }
                AsciiChar::_5 | AsciiChar::Percent => {
                    (SC1_5_PERCENT, SC2_5_PERCENT)
                }
                AsciiChar::_6 | AsciiChar::Caret => (SC1_6_CARET, SC2_6_CARET),
                AsciiChar::_7 | AsciiChar::Ampersand => {
                    (SC1_7_AMPERSAND, SC2_7_AMPERSAND)
                }
                AsciiChar::_9 | AsciiChar::ParenOpen => {
                    (SC1_9_LPAREN, SC2_9_LPAREN)
                }
                AsciiChar::_0 | AsciiChar::ParenClose => {
                    (SC1_0_RPAREN, SC2_0_RPAREN)
                }
                AsciiChar::_8 | AsciiChar::Asterisk => {
                    (SC1_8_ASTERISK, SC2_8_ASTERISK)
                }
                AsciiChar::Equal | AsciiChar::Plus => {
                    (SC1_EQUALS_PLUS, SC2_EQUALS_PLUS)
                }
                AsciiChar::Comma | AsciiChar::LessThan => {
                    (SC1_COMMA_LESSTHAN, SC2_COMMA_LESSTHAN)
                }
                AsciiChar::Minus | AsciiChar::UnderScore => {
                    (SC1_DASH_UNDERSCORE, SC2_DASH_UNDERSCORE)
                }
                AsciiChar::Dot | AsciiChar::GreaterThan => {
                    (SC1_PERIOD_GREATERTHAN, SC2_PERIOD_GREATERTHAN)
                }
                AsciiChar::Slash | AsciiChar::Question => {
                    (SC1_SLASH_QUESTIONMARK, SC2_SLASH_QUESTIONMARK)
                }
                AsciiChar::Semicolon | AsciiChar::Colon => {
                    (SC1_SEMICOLON_COLON, SC2_SEMICOLON_COLON)
                }
                AsciiChar::A | AsciiChar::a => (SC1_A, SC2_A),
                AsciiChar::B | AsciiChar::b => (SC1_B, SC2_B),
                AsciiChar::C | AsciiChar::c => (SC1_C, SC2_C),
                AsciiChar::D | AsciiChar::d => (SC1_D, SC2_D),
                AsciiChar::E | AsciiChar::e => (SC1_E, SC2_E),
                AsciiChar::F | AsciiChar::f => (SC1_F, SC2_F),
                AsciiChar::G | AsciiChar::g => (SC1_G, SC2_G),
                AsciiChar::H | AsciiChar::h => (SC1_H, SC2_H),
                AsciiChar::I | AsciiChar::i => (SC1_I, SC2_I),
                AsciiChar::J | AsciiChar::j => (SC1_J, SC2_J),
                AsciiChar::K | AsciiChar::k => (SC1_K, SC2_K),
                AsciiChar::L | AsciiChar::l => (SC1_L, SC2_L),
                AsciiChar::M | AsciiChar::m => (SC1_M, SC2_M),
                AsciiChar::N | AsciiChar::n => (SC1_N, SC2_N),
                AsciiChar::O | AsciiChar::o => (SC1_O, SC2_O),
                AsciiChar::P | AsciiChar::p => (SC1_P, SC2_P),
                AsciiChar::Q | AsciiChar::q => (SC1_Q, SC2_Q),
                AsciiChar::R | AsciiChar::r => (SC1_R, SC2_R),
                AsciiChar::S | AsciiChar::s => (SC1_S, SC2_S),
                AsciiChar::T | AsciiChar::t => (SC1_T, SC2_T),
                AsciiChar::U | AsciiChar::u => (SC1_U, SC2_U),
                AsciiChar::V | AsciiChar::v => (SC1_V, SC2_V),
                AsciiChar::W | AsciiChar::w => (SC1_W, SC2_W),
                AsciiChar::X | AsciiChar::x => (SC1_X, SC2_X),
                AsciiChar::Y | AsciiChar::y => (SC1_Y, SC2_Y),
                AsciiChar::Z | AsciiChar::z => (SC1_Z, SC2_Z),
                AsciiChar::BracketOpen | AsciiChar::CurlyBraceOpen => {
                    (SC1_LBRACKET_LCURLY, SC2_LBRACKET_LCURLY)
                }
                AsciiChar::BackSlash | AsciiChar::VerticalBar => {
                    (SC1_BACKSLASH_PIPE, SC2_BACKSLASH_PIPE)
                }
                AsciiChar::BracketClose | AsciiChar::CurlyBraceClose => {
                    (SC1_RBRACKET_RCURLY, SC2_RBRACKET_RCURLY)
                }
                AsciiChar::Grave | AsciiChar::Tilde => {
                    (SC1_BACKTICK_TILDE, SC2_BACKTICK_TILDE)
                }
                AsciiChar::DEL => (SC1_DELETE, SC2_DELETE),

                // values that are unrepresentable, or we are explicitly ignoring
                AsciiChar::Null
                | AsciiChar::SOH
                | AsciiChar::SOX
                | AsciiChar::ETX
                | AsciiChar::EOT
                | AsciiChar::ENQ
                | AsciiChar::ACK
                | AsciiChar::Bell
                | AsciiChar::LineFeed
                | AsciiChar::VT
                | AsciiChar::FF
                | AsciiChar::CarriageReturn
                | AsciiChar::SI
                | AsciiChar::SO
                | AsciiChar::DLE
                | AsciiChar::DC1
                | AsciiChar::DC2
                | AsciiChar::DC3
                | AsciiChar::DC4
                | AsciiChar::NAK
                | AsciiChar::SYN
                | AsciiChar::ETB
                | AsciiChar::CAN
                | AsciiChar::EM
                | AsciiChar::SUB
                | AsciiChar::FS
                | AsciiChar::GS
                | AsciiChar::RS
                | AsciiChar::US => (0x0, 0x0),
            },
            Backspace => (SC1_BACKSPACE, SC2_BACKSPACE),
            Tab => (SC1_TAB, SC2_TAB),
            ReturnOrEnter => (SC1_ENTER, SC2_ENTER),
            Escape => (SC1_ESCAPE, SC2_ESCAPE),
            Insert => (SC1_INSERT, SC2_INSERT),
            Delete => (SC1_DELETE, SC2_DELETE),
            Home => (SC1_HOME, SC2_HOME),
            End => (SC1_END, SC2_END),
            PageUp => (SC1_PGUP, SC2_PGUP),
            PageDown => (SC1_PGDN, SC2_PGDN),
            Print => (SC1_PRINTSCREEN, SC2_PRINTSCREEN),
            CapsLock => (SC1_CAPS_LOCK, SC2_CAPS_LOCK),
            SuperLeft => (SC1_SUPER_LEFT, SC2_SUPER_LEFT),
            SuperRight => (SC1_SUPER_RIGHT, SC2_SUPER_RIGHT),

            Left => (SC1_LEFT, SC2_LEFT),
            Up => (SC1_UP, SC2_UP),
            Right => (SC1_RIGHT, SC2_RIGHT),
            Down => (SC1_DOWN, SC2_DOWN),

            FunctionKey(1) => (SC1_F1, SC2_F1),
            FunctionKey(2) => (SC1_F2, SC2_F2),
            FunctionKey(3) => (SC1_F3, SC2_F3),
            FunctionKey(4) => (SC1_F4, SC2_F4),
            FunctionKey(5) => (SC1_F5, SC2_F5),
            FunctionKey(6) => (SC1_F6, SC2_F6),
            FunctionKey(7) => (SC1_F7, SC2_F7),
            FunctionKey(8) => (SC1_F8, SC2_F8),
            FunctionKey(9) => (SC1_F9, SC2_F9),
            FunctionKey(10) => (SC1_F10, SC2_F10),
            FunctionKey(11) => (SC1_F11, SC2_F11),
            FunctionKey(12) => (SC1_F12, SC2_F12),

            ShiftLeft => (SC1_SHIFT_LEFT, SC2_SHIFT_LEFT),
            ShiftRight => (SC1_SHIFT_RIGHT, SC2_SHIFT_RIGHT),
            ControlLeft => (SC1_CTRL_LEFT, SC2_CTRL_LEFT),
            ControlRight => (SC1_CTRL_RIGHT, SC2_CTRL_RIGHT),
            AltLeft => (SC1_ALT_LEFT, SC2_ALT_LEFT),
            AltRight => (SC1_ALT_RIGHT, SC2_ALT_RIGHT),
            ScrollLock => (SC1_SCROLL_LOCK, SC2_SCROLL_LOCK),
            NumLock => (SC1_NUM_LOCK, SC2_NUM_LOCK),

            KeypadSlash => (SC1_KP_SLASH, SC2_KP_SLASH),
            KeypadAsterisk => (SC1_KP_ASTERISK, SC2_KP_ASTERISK),
            KeypadMinus => (SC1_KP_MINUS, SC2_KP_MINUS),
            KeypadPlus => (SC1_KP_PLUS, SC2_KP_PLUS),
            KeypadEnter => (SC1_KP_ENTER, SC2_KP_ENTER),
            KeypadPeriod | KeypadDelete => {
                (SC1_KP_DEL_PERIOD, SC2_KP_DEL_PERIOD)
            }
            Keypad0 | KeypadInsert => (SC1_0_INSERT, SC2_0_INSERT),
            Keypad1 | KeypadEnd => (SC1_1_END, SC2_1_END),
            Keypad2 | KeypadDown => (SC1_2_DOWN, SC2_2_DOWN),
            Keypad3 | KeypadPgDown => (SC1_3_PGDN, SC2_3_PGDN),
            Keypad4 | KeypadLeft => (SC1_4_LEFT, SC2_4_LEFT),
            Keypad5 | KeypadEmpty => (SC1_5_CENTER, SC2_5_CENTER),
            Keypad6 | KeypadRight => (SC1_6_RIGHT, SC2_6_RIGHT),
            Keypad7 | KeypadHome => (SC1_7_HOME, SC2_7_HOME),
            Keypad8 | KeypadUp => (SC1_8_UP, SC2_8_UP),
            Keypad9 | KeypadPgUp => (SC1_9_PGUP, SC2_9_PGUP),

            // Keys we're choosing to drop explicitly for now
            FunctionKey(_) | Pause | Menu => (0x0, 0x0),
        };

        if matches!((base_val_1, base_val_2), (0x0, 0x0)) {
            return Err(anyhow!(
                "unrecognized keysym value: 0x{:x}",
                keyevent.keysym_raw
            ));
        }

        let prefix_1 = match keyevent.keysym {
            AltRight | ControlRight | Home | Insert | End | PageUp
            | PageDown | KeypadSlash | KeypadEnter | SuperLeft | SuperRight
            | Left | Right | Up | Down => Some(vec![SC1_EXTENDED_PREFIX_0]),
            _ => None,
        };

        let prefix_2 = match keyevent.keysym {
            AltRight | ControlRight | Home | Insert | End | PageUp
            | PageDown | KeypadSlash | KeypadEnter | SuperLeft | SuperRight
            | Left | Right | Up | Down => Some(vec![SC2_EXTENDED_PREFIX_0]),
            _ => None,
        };

        let sc1 = ScanCodeBase { base_val: base_val_1, prefix: prefix_1 };
        let sc2 = ScanCodeBase { base_val: base_val_2, prefix: prefix_2 };

        Ok(Self {
            keysym_raw: keyevent.keysym_raw,
            is_pressed: keyevent.is_pressed,
            scan_code_1: sc1,
            scan_code_2: sc2,
        })
    }
}
