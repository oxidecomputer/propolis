// Scan Code Set 2 codes

pub const SC2_RELEASE_CODE: u8 = 0xf0;
pub const SC2_EXTENDED_PREFIX_0: u8 = 0xe0;

// Letter keys
pub const SC2_A: u8 = 0x1c;
pub const SC2_B: u8 = 0x32;
pub const SC2_C: u8 = 0x21;
pub const SC2_D: u8 = 0x23;
pub const SC2_E: u8 = 0x24;
pub const SC2_F: u8 = 0x2b;
pub const SC2_G: u8 = 0x34;
pub const SC2_H: u8 = 0x33;
pub const SC2_I: u8 = 0x43;
pub const SC2_J: u8 = 0x3b;
pub const SC2_K: u8 = 0x42;
pub const SC2_L: u8 = 0x4b;
pub const SC2_M: u8 = 0x3a;
pub const SC2_N: u8 = 0x31;
pub const SC2_O: u8 = 0x44;
pub const SC2_P: u8 = 0x4d;
pub const SC2_Q: u8 = 0x15;
pub const SC2_R: u8 = 0x2d;
pub const SC2_S: u8 = 0x1b;
pub const SC2_T: u8 = 0x2c;
pub const SC2_U: u8 = 0x3c;
pub const SC2_V: u8 = 0x2a;
pub const SC2_W: u8 = 0x1d;
pub const SC2_X: u8 = 0x22;
pub const SC2_Y: u8 = 0x35;
pub const SC2_Z: u8 = 0x1a;

// Number keys
pub const SC2_0_RPAREN: u8 = 0x45;
pub const SC2_1_EXCLAMATION: u8 = 0x16;
pub const SC2_2_AT: u8 = 0x1f;
pub const SC2_3_HASH: u8 = 0x26;
pub const SC2_4_DOLLAR: u8 = 0x25;
pub const SC2_5_PERCENT: u8 = 0x2e;
pub const SC2_6_CARET: u8 = 0x36;
pub const SC2_7_AMPERSAND: u8 = 0x3d;
pub const SC2_8_ASTERISK: u8 = 0x3e;
pub const SC2_9_LPAREN: u8 = 0x46;

// Numpad Keys
pub const SC2_1_END: u8 = 0x69;
pub const SC2_2_DOWN: u8 = 0x72;
pub const SC2_3_PGDN: u8 = 0x7a;
pub const SC2_4_LEFT: u8 = 0x6b;
pub const SC2_5_CENTER: u8 = 0x73;
pub const SC2_6_RIGHT: u8 = 0x74;
pub const SC2_7_HOME: u8 = 0x6c;
pub const SC2_8_UP: u8 = 0x75;
pub const SC2_9_PGUP: u8 = 0x7d;
pub const SC2_0_INSERT: u8 = 0x70;

// Symbols
pub const SC2_QUOTE_DBLQUOTE: u8 = 0x52;
pub const SC2_COMMA_LESSTHAN: u8 = 0x41;
pub const SC2_DASH_UNDERSCORE: u8 = 0x4e;
pub const SC2_PERIOD_GREATERTHAN: u8 = 0x49;
pub const SC2_SLASH_QUESTIONMARK: u8 = 0x4a;
pub const SC2_SEMICOLON_COLON: u8 = 0x4c;
pub const SC2_EQUALS_PLUS: u8 = 0x55;
pub const SC2_LBRACKET_LCURLY: u8 = 0x54;
pub const SC2_BACKSLASH_PIPE: u8 = 0x5d;
pub const SC2_RBRACKET_RCURLY: u8 = 0x5b;
pub const SC2_BACKTICK_TILDE: u8 = 0x0e;

pub const SC2_BACKSPACE: u8 = 0x66;
pub const SC2_TAB: u8 = 0x0d;

pub const SC2_SPACE: u8 = 0x29;
pub const SC2_DELETE: u8 = 0x71;

pub const SC2_ENTER: u8 = 0x5a;
pub const SC2_ESCAPE: u8 = 0x76;
pub const SC2_PRINTSCREEN: u8 = 0x7c;
pub const SC2_F1: u8 = 0x05;
pub const SC2_F2: u8 = 0x06;
pub const SC2_F3: u8 = 0x04;
pub const SC2_F4: u8 = 0x0c;
pub const SC2_F5: u8 = 0x03;
pub const SC2_F6: u8 = 0x0b;
pub const SC2_F7: u8 = 0x83;
pub const SC2_F8: u8 = 0x0a;
pub const SC2_F9: u8 = 0x01;
pub const SC2_F10: u8 = 0x09;
pub const SC2_F11: u8 = 0x78;
pub const SC2_F12: u8 = 0x07;

pub const SC2_KP_ENTER: u8 = 0x5a;
pub const SC2_KP_DEL_PERIOD: u8 = 0x71;

// Gray keys
pub const SC2_KP_SLASH: u8 = 0x4a;
pub const SC2_KP_ASTERISK: u8 = 0x7c;
pub const SC2_KP_MINUS: u8 = 0x7b;
pub const SC2_KP_PLUS: u8 = 0x79;
pub const SC2_DOWN: u8 = 0x4f;
pub const SC2_LEFT: u8 = 0x4b;
pub const SC2_RIGHT: u8 = 0x4d;
pub const SC2_UP: u8 = 0x48;
pub const SC2_HOME: u8 = 0x6c;
pub const SC2_INSERT: u8 = 0x70;
pub const SC2_END: u8 = 0x69;
pub const SC2_PGUP: u8 = 0x49;
pub const SC2_PGDN: u8 = 0x7a;

pub const SC2_CTRL_LEFT: u8 = 0x14;
pub const SC2_CTRL_RIGHT: u8 = 0x14;
pub const SC2_SHIFT_LEFT: u8 = 0x12;
pub const SC2_CAPS_LOCK: u8 = 0x58;
pub const SC2_NUM_LOCK: u8 = 0x77;
pub const SC2_SCROLL_LOCK: u8 = 0x7e;
pub const SC2_SHIFT_RIGHT: u8 = 0x59;
pub const SC2_ALT_LEFT: u8 = 0x11;
pub const SC2_ALT_RIGHT: u8 = 0x11;

// TODO(JPH): pause key

pub const SC2_SUPER_LEFT: u8 = 0x1f;
pub const SC2_SUPER_RIGHT: u8 = 0x27;
