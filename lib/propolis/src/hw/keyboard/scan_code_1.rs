// Scan Code Set 1 codes

pub const SC1_RELEASE_FLAG: u8 = 0x80;
pub const SC1_EXTENDED_PREFIX_0: u8 = 0xe0;

// Letter keys
pub const SC1_A: u8 = 0x1e;
pub const SC1_B: u8 = 0x30;
pub const SC1_C: u8 = 0x2e;
pub const SC1_D: u8 = 0x20;
pub const SC1_E: u8 = 0x12;
pub const SC1_F: u8 = 0x21;
pub const SC1_G: u8 = 0x22;
pub const SC1_H: u8 = 0x23;
pub const SC1_I: u8 = 0x17;
pub const SC1_J: u8 = 0x24;
pub const SC1_K: u8 = 0x25;
pub const SC1_L: u8 = 0x26;
pub const SC1_M: u8 = 0x32;
pub const SC1_N: u8 = 0x31;
pub const SC1_O: u8 = 0x18;
pub const SC1_P: u8 = 0x19;
pub const SC1_Q: u8 = 0x10;
pub const SC1_R: u8 = 0x13;
pub const SC1_S: u8 = 0x1f;
pub const SC1_T: u8 = 0x14;
pub const SC1_U: u8 = 0x16;
pub const SC1_V: u8 = 0x2f;
pub const SC1_W: u8 = 0x11;
pub const SC1_X: u8 = 0x2d;
pub const SC1_Y: u8 = 0x15;
pub const SC1_Z: u8 = 0x2c;

// Number keys
pub const SC1_0_RPAREN: u8 = 0x0b;
pub const SC1_1_EXCLAMATION: u8 = 0x02;
pub const SC1_2_AT: u8 = 0x03;
pub const SC1_3_HASH: u8 = 0x04;
pub const SC1_4_DOLLAR: u8 = 0x05;
pub const SC1_5_PERCENT: u8 = 0x06;
pub const SC1_6_CARET: u8 = 0x07;
pub const SC1_7_AMPERSAND: u8 = 0x08;
pub const SC1_8_ASTERISK: u8 = 0x09;
pub const SC1_9_LPAREN: u8 = 0x0a;

// Numpad Keys
pub const SC1_1_END: u8 = 0x4f;
pub const SC1_2_DOWN: u8 = 0x50;
pub const SC1_3_PGDN: u8 = 0x51;
pub const SC1_4_LEFT: u8 = 0x4b;
pub const SC1_5_CENTER: u8 = 0x4c;
pub const SC1_6_RIGHT: u8 = 0x4d;
pub const SC1_7_HOME: u8 = 0x47;
pub const SC1_8_UP: u8 = 0x48;
pub const SC1_9_PGUP: u8 = 0x49;
pub const SC1_0_INSERT: u8 = 0x52;

// Symbols
pub const SC1_QUOTE_DBLQUOTE: u8 = 0x28;
pub const SC1_COMMA_LESSTHAN: u8 = 0x33;
pub const SC1_DASH_UNDERSCORE: u8 = 0x0c;
pub const SC1_PERIOD_GREATERTHAN: u8 = 0x34;
pub const SC1_SLASH_QUESTIONMARK: u8 = 0x35;
pub const SC1_SEMICOLON_COLON: u8 = 0x27;
pub const SC1_EQUALS_PLUS: u8 = 0x0d;
pub const SC1_LBRACKET_LCURLY: u8 = 0x1a;
pub const SC1_BACKSLASH_PIPE: u8 = 0x2b;
pub const SC1_RBRACKET_RCURLY: u8 = 0x1b;
pub const SC1_BACKTICK_TILDE: u8 = 0x29;

pub const SC1_BACKSPACE: u8 = 0x0e;
pub const SC1_TAB: u8 = 0x0f;

pub const SC1_SPACE: u8 = 0x39;
pub const SC1_DELETE: u8 = 0x53;

pub const SC1_ENTER: u8 = 0x1c;
pub const SC1_ESCAPE: u8 = 0x01;
pub const SC1_PRINTSCREEN: u8 = 0x37;
pub const SC1_F1: u8 = 0x3b;
pub const SC1_F2: u8 = 0x3c;
pub const SC1_F3: u8 = 0x3d;
pub const SC1_F4: u8 = 0x3e;
pub const SC1_F5: u8 = 0x3f;
pub const SC1_F6: u8 = 0x40;
pub const SC1_F7: u8 = 0x41;
pub const SC1_F8: u8 = 0x42;
pub const SC1_F9: u8 = 0x43;
pub const SC1_F10: u8 = 0x44;
pub const SC1_F11: u8 = 0x57;
pub const SC1_F12: u8 = 0x58;

pub const SC1_KP_ENTER: u8 = 0x1c;
pub const SC1_KP_DEL_PERIOD: u8 = 0x71;

// Gray keys
pub const SC1_KP_SLASH: u8 = 0x35;
pub const SC1_KP_ASTERISK: u8 = 0x37;
pub const SC1_KP_MINUS: u8 = 0x4a;
pub const SC1_KP_PLUS: u8 = 0x4e;
pub const SC1_DOWN: u8 = 0x72;
pub const SC1_LEFT: u8 = 0x6b;
pub const SC1_RIGHT: u8 = 0x74;
pub const SC1_UP: u8 = 0x75;
pub const SC1_HOME: u8 = 0x47;
pub const SC1_INSERT: u8 = 0x52;
pub const SC1_END: u8 = 0x4f;
pub const SC1_PGUP: u8 = 0x7d;
pub const SC1_PGDN: u8 = 0xf1;

pub const SC1_CTRL_LEFT: u8 = 0x1d;
pub const SC1_CTRL_RIGHT: u8 = 0x1d;
pub const SC1_SHIFT_LEFT: u8 = 0x2a;
pub const SC1_CAPS_LOCK: u8 = 0x3a;
pub const SC1_NUM_LOCK: u8 = 0x45;
pub const SC1_SCROLL_LOCK: u8 = 0x46;
pub const SC1_SHIFT_RIGHT: u8 = 0x36;
pub const SC1_ALT_LEFT: u8 = 0x38;
pub const SC1_ALT_RIGHT: u8 = 0x38;

// TODO(JPH): pause key

pub const SC1_SUPER_LEFT: u8 = 0x5b;
pub const SC1_SUPER_RIGHT: u8 = 0x5c;
