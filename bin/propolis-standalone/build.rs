const OXIDE_PLATFORM: &str = "/usr/platform/oxide/lib/amd64/";

fn main() {
    println!("cargo:rustc-link-arg=-Wl,-R{}", OXIDE_PLATFORM);
    println!("cargo:rustc-link-search={}", OXIDE_PLATFORM);
}
