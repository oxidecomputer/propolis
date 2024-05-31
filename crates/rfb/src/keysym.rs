// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2022 Oxide Computer Company

pub use ascii::AsciiChar;
use ascii::ToAsciiChar;
use KeySym::*;

// ascii characters have the same values as their keysym
const ASCII_MAX: u32 = 0x7f;

const KEYSYM_BACKSPACE: u32 = 0xff08;
const KEYSYM_TAB: u32 = 0xff09;
const KEYSYM_RETURN_ENTER: u32 = 0xff0d;
const KEYSYM_ESCAPE: u32 = 0xff1b;
const KEYSYM_INSERT: u32 = 0xff63;
const KEYSYM_DELETE: u32 = 0xffff;
const KEYSYM_HOME: u32 = 0xff50;
const KEYSYM_END: u32 = 0xff57;
const KEYSYM_PAGE_UP: u32 = 0xff55;
const KEYSYM_PAGE_DOWN: u32 = 0xff56;
const KEYSYM_PRINT: u32 = 0xff61;
const KEYSYM_PAUSE: u32 = 0xff13;
const KEYSYM_CAPS_LOCK: u32 = 0xffe5;
const KEYSYM_SUPER_LEFT: u32 = 0xffeb;
const KEYSYM_SUPER_RIGHT: u32 = 0xffec;
const KEYSYM_MENU: u32 = 0xff67;

const KEYSYM_LEFT: u32 = 0xff51;
const KEYSYM_UP: u32 = 0xff52;
const KEYSYM_RIGHT: u32 = 0xff53;
const KEYSYM_DOWN: u32 = 0xff54;
// function keys are in the range: 0xffbe to 0xffc9, in order
const KEYSYM_F1: u32 = 0xffbe;
const KEYSYM_F12: u32 = 0xffc9;
const KEYSYM_SHIFT_LEFT: u32 = 0xffe1;
const KEYSYM_SHIFT_RIGHT: u32 = 0xffe2;
const KEYSYM_CTRL_LEFT: u32 = 0xffe3;
const KEYSYM_CTRL_RIGHT: u32 = 0xffe4;

// XXX(JPH): do we need to support meta keys?

const KEYSYM_ALT_LEFT: u32 = 0xffe9;
const KEYSYM_ALT_RIGHT: u32 = 0xffea;
const KEYSYM_SCROLL_LOCK: u32 = 0xff14;
const KEYSYM_NUM_LOCK: u32 = 0xff7f;

const KEYSYM_KP_ENTER: u32 = 0xff8d;
const KEYSYM_KP_SLASH: u32 = 0xffaf;
const KEYSYM_KP_ASTERISK: u32 = 0xffaa;
const KEYSYM_KP_MINUS: u32 = 0xffad;
const KEYSYM_KP_PLUS: u32 = 0xffab;
const KEYSYM_KP_7: u32 = 0xffb7;
const KEYSYM_KP_HOME: u32 = 0xff95;
const KEYSYM_KP_8: u32 = 0xffb8;
const KEYSYM_KP_UP: u32 = 0xff97;
const KEYSYM_KP_9: u32 = 0xffb9;
const KEYSYM_KP_PGUP: u32 = 0xff9a;
const KEYSYM_KP_4: u32 = 0xffb4;
const KEYSYM_KP_LEFT: u32 = 0xff96;
const KEYSYM_KP_5: u32 = 0xffb5;
const KEYSYM_KP_EMPTY: u32 = 0xff9d;
const KEYSYM_KP_6: u32 = 0xffb6;
const KEYSYM_KP_RIGHT: u32 = 0xff98;
const KEYSYM_KP_1: u32 = 0xffb1;
const KEYSYM_KP_END: u32 = 0xff9c;
const KEYSYM_KP_2: u32 = 0xffb2;
const KEYSYM_KP_DOWN: u32 = 0xff99;
const KEYSYM_KP_3: u32 = 0xffb3;
const KEYSYM_KP_PGDOWN: u32 = 0xff9b;
const KEYSYM_KP_0: u32 = 0xffb0;
const KEYSYM_KP_INSERT: u32 = 0xff9e;
const KEYSYM_KP_PERIOD: u32 = 0xffae;
const KEYSYM_KP_DELETE: u32 = 0xff9f;

#[derive(Debug, Copy, Clone)]
pub enum KeySym {
    Ascii(ascii::AsciiChar),
    Backspace,
    Tab,
    ReturnOrEnter,
    Escape,
    Insert,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    Print,
    Pause,
    CapsLock,

    // "super" = windows/command key
    SuperLeft,
    SuperRight,

    // usb-only
    Menu,

    Left,
    Up,
    Right,
    Down,

    FunctionKey(u8),

    ShiftLeft,
    ShiftRight,
    ControlLeft,
    ControlRight,
    AltLeft,
    AltRight,
    ScrollLock,

    // Number Keypad
    NumLock,
    KeypadSlash,
    KeypadAsterisk,
    KeypadMinus,
    KeypadPlus,
    KeypadEnter,
    KeypadPeriod,
    KeypadDelete,
    Keypad0,
    KeypadInsert,
    Keypad1,
    KeypadEnd,
    Keypad2,
    KeypadDown,
    Keypad3,
    KeypadPgDown,
    Keypad4,
    KeypadLeft,
    Keypad5,
    KeypadEmpty,
    Keypad6,
    KeypadRight,
    Keypad7,
    KeypadHome,
    Keypad8,
    KeypadUp,
    Keypad9,
    KeypadPgUp,
}

#[derive(Debug, thiserror::Error)]
#[error("unknown keysym: 0x{0:x}")]
pub struct KeySymError(u32);

impl TryFrom<u32> for KeySym {
    type Error = KeySymError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            // SAFETY: we're within the valid ascii range
            0..=ASCII_MAX => Ok(Ascii(unsafe { value.to_ascii_char_unchecked() })),
            KEYSYM_BACKSPACE => Ok(Backspace),
            KEYSYM_TAB => Ok(Tab),
            KEYSYM_RETURN_ENTER => Ok(ReturnOrEnter),
            KEYSYM_ESCAPE => Ok(Escape),
            KEYSYM_INSERT => Ok(Insert),
            KEYSYM_DELETE => Ok(Delete),
            KEYSYM_HOME => Ok(Home),
            KEYSYM_END => Ok(End),
            KEYSYM_PAGE_UP => Ok(PageUp),
            KEYSYM_PRINT => Ok(Print),
            KEYSYM_PAUSE => Ok(Pause),
            KEYSYM_CAPS_LOCK => Ok(CapsLock),
            KEYSYM_SUPER_LEFT => Ok(SuperLeft),
            KEYSYM_SUPER_RIGHT => Ok(SuperRight),
            KEYSYM_MENU => Ok(Menu),

            KEYSYM_PAGE_DOWN => Ok(PageDown),
            KEYSYM_LEFT => Ok(Left),
            KEYSYM_UP => Ok(Up),
            KEYSYM_RIGHT => Ok(Right),
            KEYSYM_DOWN => Ok(Down),

            f if (f >= KEYSYM_F1 && f <= KEYSYM_F12) => {
                let n = f - KEYSYM_F1 + 1;
                // TODO: handle cast
                Ok(FunctionKey(n as u8))
            }

            KEYSYM_SHIFT_LEFT => Ok(ShiftLeft),
            KEYSYM_SHIFT_RIGHT => Ok(ShiftRight),
            KEYSYM_CTRL_LEFT => Ok(ControlLeft),
            KEYSYM_CTRL_RIGHT => Ok(ControlRight),
            KEYSYM_ALT_LEFT => Ok(AltLeft),
            KEYSYM_ALT_RIGHT => Ok(AltRight),

            KEYSYM_SCROLL_LOCK => Ok(ScrollLock),
            KEYSYM_NUM_LOCK => Ok(NumLock),

            KEYSYM_KP_ENTER => Ok(KeypadEnter),
            KEYSYM_KP_SLASH => Ok(KeypadSlash),
            KEYSYM_KP_ASTERISK => Ok(KeypadAsterisk),
            KEYSYM_KP_MINUS => Ok(KeypadMinus),
            KEYSYM_KP_PLUS => Ok(KeypadPlus),
            KEYSYM_KP_7 => Ok(Keypad7),
            KEYSYM_KP_HOME => Ok(KeypadHome),
            KEYSYM_KP_8 => Ok(Keypad8),
            KEYSYM_KP_UP => Ok(KeypadUp),
            KEYSYM_KP_9 => Ok(Keypad9),
            KEYSYM_KP_PGUP => Ok(KeypadPgUp),
            KEYSYM_KP_4 => Ok(Keypad4),
            KEYSYM_KP_LEFT => Ok(KeypadLeft),
            KEYSYM_KP_5 => Ok(Keypad5),
            KEYSYM_KP_EMPTY => Ok(KeypadEmpty),
            KEYSYM_KP_6 => Ok(Keypad6),
            KEYSYM_KP_RIGHT => Ok(KeypadRight),
            KEYSYM_KP_1 => Ok(Keypad1),
            KEYSYM_KP_END => Ok(KeypadEnd),
            KEYSYM_KP_2 => Ok(Keypad2),
            KEYSYM_KP_DOWN => Ok(KeypadDown),
            KEYSYM_KP_3 => Ok(Keypad3),
            KEYSYM_KP_PGDOWN => Ok(KeypadPgDown),
            KEYSYM_KP_0 => Ok(Keypad0),
            KEYSYM_KP_INSERT => Ok(KeypadInsert),
            KEYSYM_KP_PERIOD => Ok(KeypadPeriod),
            KEYSYM_KP_DELETE => Ok(KeypadDelete),

            _ => Err(KeySymError(value)),
        }
    }
}
