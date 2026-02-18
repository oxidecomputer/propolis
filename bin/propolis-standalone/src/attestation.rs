// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

/// Facilities for using VM attestation with standalone

use crate::config::Config;

use std::io::Read;

use sha2::{Digest, Sha256};

// 1 GiB
const MAX_BUF_SIZE: usize = 1024 * 1024 * 1024;


fn get_file_path(cfg: &Config) -> String {
    // TODO: add read only check
    let boot_order = cfg
        .main
        .boot_order
        .as_ref()
        .expect("must specify boot order to calculate boot disk");

    assert!(
        boot_order.len() > 0,
        "must specify at least one disk in `boot_order`"
    );

    let boot_devname = boot_order[0].clone();
    let boot_dev = cfg
        .devices
        .get(&boot_devname)
        .expect("could not find boot device {boot_devname}");
    let backend_name = boot_dev
        .options
        .get("block_dev")
        .expect("couldn't find block_dev for boot disk")
        .as_str()
        .expect("string");
    let backend = cfg
        .block_devs
        .get(backend_name)
        .expect("block_dev {backend_name} not found in cfg");
    let path = backend
        .options
        .get("path")
        .expect("backend {backend_name} missing path");

    path.as_str().expect("path must be a string").to_owned()
}

/// Calculate the digest of the boot disk of the VM.
pub(crate) fn calc_boot_digest(cfg: &Config, log: &slog::Logger) -> String {
    let path = get_file_path(cfg);
    slog::info!(&log, "calc boot digest, path={}", path);

    let mut file =
        std::fs::File::open(&path).expect("failed to open disk image");
    let file_len =
        file.metadata().expect("failed to get file metadata").len() as usize;
    slog::info!(log, "requested hash of file {}, size={}", path, file_len);
    let buf_size = file_len.min(MAX_BUF_SIZE);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; buf_size];
    loop {
        let n = file.read(&mut buf).expect("failed to read disk image");
        if n == 0 {
            break;
        }
        slog::info!(log, "read {} bytes", n);
        hasher.update(&buf[..n]);
    }
    slog::info!(log, "done");
    format!("{:x}", hasher.finalize())
}
