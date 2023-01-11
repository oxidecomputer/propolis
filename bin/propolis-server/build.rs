// Copyright 2023 Oxide Computer Company

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    if !version_check::is_min_version("1.59").unwrap_or(false) {
        println!("cargo:rustc-cfg=usdt_need_asm");
    }

    #[cfg(target_os = "macos")]
    if version_check::supports_feature("asm_sym").unwrap_or(false)
        && !version_check::is_min_version("1.67").unwrap_or(false)
    {
        println!("cargo:rustc-cfg=usdt_need_asm_sym");
    }
}
