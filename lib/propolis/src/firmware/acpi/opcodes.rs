// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! AML opcode constants.
//!
//! See ACPI spec section 20: <https://uefi.org/specs/ACPI/6.5/20_AML_Specification.html>

// Namespace modifier objects
pub const SCOPE_OP: u8 = 0x10;
pub const NAME_OP: u8 = 0x08;

// Named objects (require EXT_OP_PREFIX)
pub const EXT_OP_PREFIX: u8 = 0x5B;
pub const DEVICE_OP: u8 = 0x82; // ExtOp
pub const PROCESSOR_OP: u8 = 0x83; // ExtOp
pub const POWER_RES_OP: u8 = 0x84; // ExtOp
pub const THERMAL_ZONE_OP: u8 = 0x85; // ExtOp
pub const FIELD_OP: u8 = 0x81; // ExtOp
pub const OP_REGION_OP: u8 = 0x80; // ExtOp

// Method
pub const METHOD_OP: u8 = 0x14;

// Data objects
pub const ZERO_OP: u8 = 0x00;
pub const ONE_OP: u8 = 0x01;
pub const ONES_OP: u8 = 0xFF;
pub const BYTE_PREFIX: u8 = 0x0A;
pub const WORD_PREFIX: u8 = 0x0B;
pub const DWORD_PREFIX: u8 = 0x0C;
pub const QWORD_PREFIX: u8 = 0x0E;
pub const STRING_PREFIX: u8 = 0x0D;
pub const BUFFER_OP: u8 = 0x11;
pub const PACKAGE_OP: u8 = 0x12;
pub const VAR_PACKAGE_OP: u8 = 0x13;

// Name prefixes
pub const DUAL_NAME_PREFIX: u8 = 0x2E;
pub const MULTI_NAME_PREFIX: u8 = 0x2F;
pub const ROOT_PREFIX: u8 = 0x5C; // '\'
pub const PARENT_PREFIX: u8 = 0x5E; // '^'
pub const NULL_NAME: u8 = 0x00;

// Local and argument references
pub const LOCAL0_OP: u8 = 0x60;
pub const LOCAL1_OP: u8 = 0x61;
pub const LOCAL2_OP: u8 = 0x62;
pub const LOCAL3_OP: u8 = 0x63;
pub const LOCAL4_OP: u8 = 0x64;
pub const LOCAL5_OP: u8 = 0x65;
pub const LOCAL6_OP: u8 = 0x66;
pub const LOCAL7_OP: u8 = 0x67;
pub const ARG0_OP: u8 = 0x68;
pub const ARG1_OP: u8 = 0x69;
pub const ARG2_OP: u8 = 0x6A;
pub const ARG3_OP: u8 = 0x6B;
pub const ARG4_OP: u8 = 0x6C;
pub const ARG5_OP: u8 = 0x6D;
pub const ARG6_OP: u8 = 0x6E;

// Control flow
pub const IF_OP: u8 = 0xA0;
pub const ELSE_OP: u8 = 0xA1;
pub const WHILE_OP: u8 = 0xA2;
pub const RETURN_OP: u8 = 0xA4;
pub const BREAK_OP: u8 = 0xA5;
pub const CONTINUE_OP: u8 = 0x9F;

// Logical operators
pub const LAND_OP: u8 = 0x90;
pub const LOR_OP: u8 = 0x91;
pub const LNOT_OP: u8 = 0x92;
pub const LEQUAL_OP: u8 = 0x93;
pub const LGREATER_OP: u8 = 0x94;
pub const LLESS_OP: u8 = 0x95;

// Arithmetic operators
pub const ADD_OP: u8 = 0x72;
pub const SUBTRACT_OP: u8 = 0x74;
pub const MULTIPLY_OP: u8 = 0x77;
pub const DIVIDE_OP: u8 = 0x78;
pub const AND_OP: u8 = 0x7B;
pub const OR_OP: u8 = 0x7D;
pub const XOR_OP: u8 = 0x7F;
pub const NOT_OP: u8 = 0x80;
pub const SHIFT_LEFT_OP: u8 = 0x79;
pub const SHIFT_RIGHT_OP: u8 = 0x7A;

// Miscellaneous
pub const STORE_OP: u8 = 0x70;
pub const NOTIFY_OP: u8 = 0x86;
pub const SIZEOF_OP: u8 = 0x87;
pub const INDEX_OP: u8 = 0x88;
pub const DEREF_OF_OP: u8 = 0x83;
pub const REF_OF_OP: u8 = 0x71;
pub const CREATE_DWORD_FIELD_OP: u8 = 0x8A;

// Resource template end tag
pub const END_TAG: u8 = 0x79;

// Operation region address space types
pub const SYSTEM_MEMORY: u8 = 0x00;
pub const SYSTEM_IO: u8 = 0x01;
pub const PCI_CONFIG: u8 = 0x02;
pub const EMBEDDED_CONTROL: u8 = 0x03;
pub const SMBUS: u8 = 0x04;
pub const CMOS: u8 = 0x05;
pub const PCI_BAR_TARGET: u8 = 0x06;

// Field access types
pub const ACCESS_ANY: u8 = 0x00;
pub const ACCESS_BYTE: u8 = 0x01;
pub const ACCESS_WORD: u8 = 0x02;
pub const ACCESS_DWORD: u8 = 0x03;
pub const ACCESS_QWORD: u8 = 0x04;
pub const ACCESS_BUFFER: u8 = 0x05;
