use std::env;

fn main() {
    println!("cargo:rerun-if-env-changed=HANDOFF_VERSION");
    println!("cargo:rerun-if-changed=assets/logo.ico");
    println!("cargo:rerun-if-changed=build.rs");

    let version = env::var("HANDOFF_VERSION")
        .ok()
        .map(|v| v.trim().trim_start_matches('v').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "dev".to_string());
    println!("cargo:rustc-env=HANDOFF_VERSION_STAMP={version}");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_icon();
    }
}

#[cfg(windows)]
fn embed_icon() {
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/logo.ico");
    if let Err(err) = res.compile() {
        println!("cargo:warning=could not embed the icon: {err}");
    }
}

#[cfg(not(windows))]
fn embed_icon() {}
