//! Test-worker linker configuration for authority-level process fixtures.

fn main() {
    let target = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let argument = match target.as_str() {
        "macos" => "-Wl,-export_dynamic",
        "linux" => "-Wl,--export-dynamic",
        "windows" => "/EXPORT:libreofficekit_hook_2",
        _ => return,
    };
    println!("cargo::rustc-link-arg-bin=legacy-office-test-worker={argument}");
}
