// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! AmlSink that writes AML bytecode to a file.

use acpi_tables::AmlSink;
use std::fs::File;
use std::io::prelude::*;

pub struct FileSink {
    f: File,
}

/// AmlSink to write AML bytecode to a file.
///
/// Panics if the file can't be created or written to.
impl FileSink {
    pub fn new(file: &str) -> Self {
        let path = std::path::Path::new(file);
        let prefix = path.parent().unwrap();
        std::fs::create_dir_all(prefix).unwrap();
        let f = File::create(file).unwrap();
        Self { f }
    }
}

impl AmlSink for FileSink {
    fn byte(&mut self, byte: u8) {
        self.f.write_all(&[byte]).unwrap();
    }
}
