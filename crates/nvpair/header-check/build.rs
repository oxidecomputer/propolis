// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![deny(warnings)]

fn main() {
    let mut cfg = ctest2::TestGenerator::new();

    cfg.header("libnvpair.h");

    cfg.type_name(|ty, is_struct, is_union| match ty {
        t if t.ends_with("_t") => t.to_string(),
        t if is_struct => format!("struct {t}"),
        t if is_union => format!("union {t}"),
        t => t.to_string(),
    });

    cfg.skip_const(move |name| match name {
        _ => false,
    });

    cfg.skip_struct(|name| match name {
        _ => false,
    });

    cfg.skip_field_type(|ty, field| match (ty, field) {
        _ => false,
    });

    cfg.generate("../sys/src/lib.rs", "main.rs");
}
