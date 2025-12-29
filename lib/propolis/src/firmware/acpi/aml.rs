// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::names::{encode_name_string, EisaId};
use super::opcodes::*;

pub trait AmlWriter {
    fn write_aml(&self, buf: &mut Vec<u8>);
}

impl AmlWriter for u8 {
    fn write_aml(&self, buf: &mut Vec<u8>) {
        match *self {
            0 => buf.push(ZERO_OP),
            1 => buf.push(ONE_OP),
            v => {
                buf.push(BYTE_PREFIX);
                buf.push(v);
            }
        }
    }
}

impl AmlWriter for u16 {
    fn write_aml(&self, buf: &mut Vec<u8>) {
        if *self <= u8::MAX as u16 {
            (*self as u8).write_aml(buf);
        } else {
            buf.push(WORD_PREFIX);
            buf.extend_from_slice(&self.to_le_bytes());
        }
    }
}

impl AmlWriter for u32 {
    fn write_aml(&self, buf: &mut Vec<u8>) {
        if *self <= u16::MAX as u32 {
            (*self as u16).write_aml(buf);
        } else {
            buf.push(DWORD_PREFIX);
            buf.extend_from_slice(&self.to_le_bytes());
        }
    }
}

impl AmlWriter for u64 {
    fn write_aml(&self, buf: &mut Vec<u8>) {
        if *self <= u32::MAX as u64 {
            (*self as u32).write_aml(buf);
        } else {
            buf.push(QWORD_PREFIX);
            buf.extend_from_slice(&self.to_le_bytes());
        }
    }
}

impl AmlWriter for &str {
    fn write_aml(&self, buf: &mut Vec<u8>) {
        buf.push(STRING_PREFIX);
        buf.extend_from_slice(self.as_bytes());
        buf.push(0);
    }
}

impl AmlWriter for String {
    fn write_aml(&self, buf: &mut Vec<u8>) {
        self.as_str().write_aml(buf);
    }
}

impl AmlWriter for EisaId {
    fn write_aml(&self, buf: &mut Vec<u8>) {
        buf.push(DWORD_PREFIX);
        buf.extend_from_slice(&self.0.to_le_bytes());
    }
}

impl AmlWriter for Vec<u8> {
    fn write_aml(&self, buf: &mut Vec<u8>) {
        write_buffer(buf, self);
    }
}

impl AmlWriter for &[u8] {
    fn write_aml(&self, buf: &mut Vec<u8>) {
        write_buffer(buf, self);
    }
}

fn write_buffer(buf: &mut Vec<u8>, data: &[u8]) {
    buf.push(BUFFER_OP);

    let mut size_buf = Vec::new();
    (data.len() as u64).write_aml(&mut size_buf);

    write_pkg_length(buf, size_buf.len() + data.len());
    buf.extend_from_slice(&size_buf);
    buf.extend_from_slice(data);
}

fn encode_pkg_length(total_len: usize) -> ([u8; MAX_PKG_LENGTH_BYTES], usize) {
    let mut bytes = [0u8; MAX_PKG_LENGTH_BYTES];
    let size = pkg_length_size(total_len.saturating_sub(1));
    match size {
        1 => bytes[0] = total_len as u8,
        2 => {
            bytes[0] = 0x40 | ((total_len & 0x0F) as u8);
            bytes[1] = ((total_len >> 4) & 0xFF) as u8;
        }
        3 => {
            bytes[0] = 0x80 | ((total_len & 0x0F) as u8);
            bytes[1] = ((total_len >> 4) & 0xFF) as u8;
            bytes[2] = ((total_len >> 12) & 0xFF) as u8;
        }
        4 => {
            bytes[0] = 0xC0 | ((total_len & 0x0F) as u8);
            bytes[1] = ((total_len >> 4) & 0xFF) as u8;
            bytes[2] = ((total_len >> 12) & 0xFF) as u8;
            bytes[3] = ((total_len >> 20) & 0xFF) as u8;
        }
        _ => unreachable!(),
    }
    (bytes, size)
}

fn write_pkg_length(buf: &mut Vec<u8>, content_len: usize) {
    let pkg_size = pkg_length_size(content_len);
    let (bytes, size) = encode_pkg_length(pkg_size + content_len);
    buf.extend_from_slice(&bytes[..size]);
}

fn pkg_length_size(content_len: usize) -> usize {
    if content_len < 0x3F - 1 {
        1
    } else if content_len < 0xFFF - 2 {
        2
    } else if content_len < 0xFFFFF - 3 {
        3
    } else {
        4
    }
}

const MAX_PKG_LENGTH_BYTES: usize = 4;

#[must_use = "call .finish() to get the AML bytes"]
pub struct AmlBuilder {
    buf: Vec<u8>,
}

impl AmlBuilder {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn scope(&mut self, name: &str) -> ScopeGuard<'_> {
        ScopeGuard::new(self, name)
    }

    pub fn device(&mut self, name: &str) -> DeviceGuard<'_> {
        DeviceGuard::new(self, name)
    }

    pub fn method(
        &mut self,
        name: &str,
        arg_count: u8,
        serialized: bool,
    ) -> MethodGuard<'_> {
        MethodGuard::new(self, name, arg_count, serialized)
    }

    pub fn name<T: AmlWriter>(&mut self, name: &str, value: &T) {
        self.buf.push(NAME_OP);
        encode_name_string(name, &mut self.buf);
        value.write_aml(&mut self.buf);
    }

    pub fn name_package<T: AmlWriter>(&mut self, name: &str, elements: &[T]) {
        self.buf.push(NAME_OP);
        encode_name_string(name, &mut self.buf);
        write_package(&mut self.buf, elements);
    }

    pub fn raw(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    pub fn return_value<T: AmlWriter>(&mut self, value: &T) {
        self.buf.push(RETURN_OP);
        value.write_aml(&mut self.buf);
    }

    pub fn finish(self) -> Vec<u8> {
        self.buf
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }
}

impl Default for AmlBuilder {
    fn default() -> Self {
        Self::new()
    }
}

fn write_package<T: AmlWriter>(buf: &mut Vec<u8>, elements: &[T]) {
    buf.push(PACKAGE_OP);

    let mut content = Vec::new();
    content.push(elements.len() as u8);
    for elem in elements {
        elem.write_aml(&mut content);
    }

    write_pkg_length(buf, content.len());
    buf.extend_from_slice(&content);
}

/// ```compile_fail
/// use propolis::firmware::acpi::AmlBuilder;
/// let mut builder = AmlBuilder::new();
/// {
///     let mut sb = builder.scope("\\_SB_");
///     {
///         let mut pci = sb.device("PCI0");
///         {
///             let dev = pci.device("DEV0");
///         }
///         sb.device("DEV1"); // error: `sb` still borrowed by `pci`
///     }
/// }
/// ```
pub struct ScopeGuard<'a> {
    builder: &'a mut AmlBuilder,
    start_pos: usize,
    content_start: usize,
}

impl<'a> ScopeGuard<'a> {
    fn new(builder: &'a mut AmlBuilder, name: &str) -> Self {
        builder.buf.push(SCOPE_OP);
        let start_pos = builder.buf.len();
        builder.buf.extend_from_slice(&[0; MAX_PKG_LENGTH_BYTES]);
        encode_name_string(name, &mut builder.buf);
        let content_start = builder.buf.len();
        Self { builder, start_pos, content_start }
    }

    pub fn scope(&mut self, name: &str) -> ScopeGuard<'_> {
        ScopeGuard::new(self.builder, name)
    }

    pub fn device(&mut self, name: &str) -> DeviceGuard<'_> {
        DeviceGuard::new(self.builder, name)
    }

    pub fn method(
        &mut self,
        name: &str,
        arg_count: u8,
        serialized: bool,
    ) -> MethodGuard<'_> {
        MethodGuard::new(self.builder, name, arg_count, serialized)
    }

    pub fn name<T: AmlWriter>(&mut self, name: &str, value: &T) {
        self.builder.name(name, value);
    }

    pub fn name_package<T: AmlWriter>(&mut self, name: &str, elements: &[T]) {
        self.builder.name_package(name, elements);
    }

    pub fn processor(&mut self, name: &str, proc_id: u8) {
        self.builder.buf.push(EXT_OP_PREFIX);
        self.builder.buf.push(PROCESSOR_OP);

        let mut name_buf = Vec::new();
        encode_name_string(name, &mut name_buf);
        write_pkg_length(&mut self.builder.buf, name_buf.len() + 6);

        self.builder.buf.extend_from_slice(&name_buf);
        self.builder.buf.push(proc_id);
        self.builder.buf.extend_from_slice(&[0u8; 4]);
        self.builder.buf.push(0);
    }

    pub fn raw(&mut self, bytes: &[u8]) {
        self.builder.raw(bytes);
    }
}

impl Drop for ScopeGuard<'_> {
    fn drop(&mut self) {
        finalize_pkg_length(
            &mut self.builder.buf,
            self.start_pos,
            self.content_start,
        );
    }
}

/// ```compile_fail
/// use propolis::firmware::acpi::AmlBuilder;
/// let mut builder = AmlBuilder::new();
/// {
///     let mut sb = builder.scope("\\_SB_");
///     {
///         let mut dev = sb.device("DEV0");
///         {
///             let m = dev.method("_STA", 0, false);
///         }
///         dev.name("_UID", &0u32);
///         sb.device("DEV1"); // error: `sb` still borrowed by `dev`
///     }
/// }
/// ```
pub struct DeviceGuard<'a> {
    builder: &'a mut AmlBuilder,
    start_pos: usize,
    content_start: usize,
}

impl<'a> DeviceGuard<'a> {
    fn new(builder: &'a mut AmlBuilder, name: &str) -> Self {
        builder.buf.push(EXT_OP_PREFIX);
        builder.buf.push(DEVICE_OP);
        let start_pos = builder.buf.len();
        builder.buf.extend_from_slice(&[0; MAX_PKG_LENGTH_BYTES]);
        encode_name_string(name, &mut builder.buf);
        let content_start = builder.buf.len();
        Self { builder, start_pos, content_start }
    }

    pub fn device(&mut self, name: &str) -> DeviceGuard<'_> {
        DeviceGuard::new(self.builder, name)
    }

    pub fn method(
        &mut self,
        name: &str,
        arg_count: u8,
        serialized: bool,
    ) -> MethodGuard<'_> {
        MethodGuard::new(self.builder, name, arg_count, serialized)
    }

    pub fn name<T: AmlWriter>(&mut self, name: &str, value: &T) {
        self.builder.name(name, value);
    }

    pub fn name_package<T: AmlWriter>(&mut self, name: &str, elements: &[T]) {
        self.builder.name_package(name, elements);
    }

    pub fn raw(&mut self, bytes: &[u8]) {
        self.builder.raw(bytes);
    }
}

impl Drop for DeviceGuard<'_> {
    fn drop(&mut self) {
        finalize_pkg_length(
            &mut self.builder.buf,
            self.start_pos,
            self.content_start,
        );
    }
}

/// ```compile_fail
/// use propolis::firmware::acpi::AmlBuilder;
/// let mut builder = AmlBuilder::new();
/// {
///     let mut sb = builder.scope("\\_SB_");
///     {
///         let mut dev = sb.device("DEV0");
///         {
///             let m = dev.method("_STA", 0, false);
///             dev.method("_ON_", 0, false); // error: `dev` still borrowed by `m`
///         }
///     }
/// }
/// ```
pub struct MethodGuard<'a> {
    builder: &'a mut AmlBuilder,
    start_pos: usize,
    content_start: usize,
}

impl<'a> MethodGuard<'a> {
    fn new(
        builder: &'a mut AmlBuilder,
        name: &str,
        arg_count: u8,
        serialized: bool,
    ) -> Self {
        assert!(arg_count <= 7, "method can have at most 7 arguments");
        builder.buf.push(METHOD_OP);
        let start_pos = builder.buf.len();
        builder.buf.extend_from_slice(&[0; MAX_PKG_LENGTH_BYTES]);
        encode_name_string(name, &mut builder.buf);
        let flags = arg_count | if serialized { 0x08 } else { 0 };
        builder.buf.push(flags);
        let content_start = builder.buf.len();
        Self { builder, start_pos, content_start }
    }

    pub fn return_value<T: AmlWriter>(&mut self, value: &T) {
        self.builder.return_value(value);
    }

    pub fn raw(&mut self, bytes: &[u8]) {
        self.builder.raw(bytes);
    }
}

impl Drop for MethodGuard<'_> {
    fn drop(&mut self) {
        finalize_pkg_length(
            &mut self.builder.buf,
            self.start_pos,
            self.content_start,
        );
    }
}

fn finalize_pkg_length(
    buf: &mut Vec<u8>,
    start_pos: usize,
    content_start: usize,
) {
    let name_len = content_start - start_pos - MAX_PKG_LENGTH_BYTES;
    let body_len = buf.len() - content_start;
    let content_len = name_len + body_len;
    let pkg_size = pkg_length_size(content_len);
    let (pkg_bytes, size) = encode_pkg_length(pkg_size + content_len);

    buf.splice(
        start_pos..start_pos + MAX_PKG_LENGTH_BYTES,
        pkg_bytes[..size].iter().copied(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_encoding() {
        let mut buf = Vec::new();
        0u8.write_aml(&mut buf);
        assert_eq!(buf, vec![ZERO_OP]);

        buf.clear();
        1u8.write_aml(&mut buf);
        assert_eq!(buf, vec![ONE_OP]);

        buf.clear();
        42u8.write_aml(&mut buf);
        assert_eq!(buf, vec![BYTE_PREFIX, 42]);

        buf.clear();
        0x1234u16.write_aml(&mut buf);
        assert_eq!(buf, vec![WORD_PREFIX, 0x34, 0x12]);

        buf.clear();
        0xDEADBEEFu32.write_aml(&mut buf);
        assert_eq!(buf, vec![DWORD_PREFIX, 0xEF, 0xBE, 0xAD, 0xDE]);

        buf.clear();
        0x123456789ABCDEF0u64.write_aml(&mut buf);
        assert_eq!(buf[0], QWORD_PREFIX);
    }

    #[test]
    fn string_encoding() {
        let mut buf = Vec::new();
        "Hello".write_aml(&mut buf);
        assert_eq!(buf, vec![STRING_PREFIX, b'H', b'e', b'l', b'l', b'o', 0]);
    }

    #[test]
    fn scope_with_named_object() {
        let mut builder = AmlBuilder::new();
        {
            let mut scope = builder.scope("_SB_");
            scope.name("TEST", &42u8);
        }
        let aml = builder.finish();

        assert_eq!(aml[0], SCOPE_OP);
        assert!(aml.windows(4).any(|w| w == b"TEST"));
    }

    #[test]
    fn device_in_scope() {
        use super::super::names::EisaId;

        let mut builder = AmlBuilder::new();
        {
            let mut sb = builder.scope("\\_SB_");
            {
                let mut dev = sb.device("PCI0");
                dev.name("_HID", &EisaId::from_str("PNP0A08"));
            }
        }
        let aml = builder.finish();

        assert_eq!(aml[0], SCOPE_OP);
        assert!(aml.windows(2).any(|w| w == [EXT_OP_PREFIX, DEVICE_OP]));
    }

    #[test]
    fn method_with_return() {
        let mut builder = AmlBuilder::new();
        {
            let mut scope = builder.scope("_SB_");
            {
                let mut method = scope.method("_STA", 0, false);
                method.return_value(&0x0Fu8);
            }
        }
        let aml = builder.finish();

        assert!(aml.windows(1).any(|w| w == [METHOD_OP]));
        assert!(aml.windows(4).any(|w| w == b"_STA"));
    }

    #[test]
    fn nested_scopes() {
        let mut builder = AmlBuilder::new();
        {
            let mut sb = builder.scope("\\_SB_");
            {
                let mut pci = sb.scope("PCI0");
                pci.name("_ADR", &0u32);
            }
        }
        let aml = builder.finish();

        let scope_count = aml.iter().filter(|&&b| b == SCOPE_OP).count();
        assert_eq!(scope_count, 2);
    }

    #[test]
    fn pkg_length_single_byte() {
        let mut builder = AmlBuilder::new();
        {
            let scope = builder.scope("TEST");
            drop(scope);
        }
        let aml = builder.finish();

        assert_eq!(aml.len(), 6);
        assert_eq!(aml[0], SCOPE_OP);
        assert_eq!(aml[1], 5);
    }

    #[test]
    fn pkg_length_zero() {
        let (bytes, size) = encode_pkg_length(0);
        assert_eq!(size, 1);
        assert_eq!(bytes[0], 0);
    }
}
