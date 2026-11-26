//! Links NVIDIA's NGX library (DLSS) when its SDK is there: `VICTORY_DLSS_SDK`,
//! else `dlss-sdk` beside the repository (`work/dlss-sdk`). Without it the
//! build has no DLSS (`cfg(victory_dlss)` unset) and the title's own
//! temporal resolve runs.

use std::path::PathBuf;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(victory_dlss)");
    println!("cargo::rerun-if-env-changed=VICTORY_DLSS_SDK");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let sdk = std::env::var_os("VICTORY_DLSS_SDK")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("../../../dlss-sdk"));
    let lib = sdk.join("lib");
    println!("cargo::rerun-if-changed={}", lib.join("nvsdk_ngx_d.lib").display());
    if !lib.join("nvsdk_ngx_d.lib").is_file() {
        println!("cargo::warning=no DLSS SDK at {}: built without DLSS", sdk.display());
        return;
    }
    println!("cargo::rustc-link-search=native={}", lib.display());
    // The dynamic-CRT build of the library, as Rust links the CRT.
    println!("cargo::rustc-link-lib=static=nvsdk_ngx_d");
    // It finds the driver's NGX through the registry.
    println!("cargo::rustc-link-lib=dylib=advapi32");
    println!("cargo::rustc-cfg=victory_dlss");
}
