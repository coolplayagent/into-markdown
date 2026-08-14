//! Deterministic compatibility-worker protocol fixture.

fn main() -> std::process::ExitCode {
    std::hint::black_box(libreofficekit_hook_2 as *const () as usize);
    into_markdown_legacy_office::test_worker_main()
}

/// Link-visible fixture satisfying authority ABI validation in integration tests.
#[unsafe(no_mangle)]
pub extern "C" fn libreofficekit_hook_2(
    _: *const std::ffi::c_char,
    _: *const std::ffi::c_char,
) -> *mut std::ffi::c_void {
    std::ptr::null_mut()
}
