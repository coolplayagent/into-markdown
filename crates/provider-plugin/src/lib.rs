//! Capability metadata, adapters, and deterministic routing for signed plugins.
//!
//! This crate is the host-side authority for OCR, transcription, and speaker
//! diarization plugins. It contains no model implementation and never loads a
//! Rust dynamic library into the host process.

mod manifest;
mod process;
mod routing;

pub use manifest::{
    CAPABILITY_PROTOCOL, CapabilityKind, HostApiRange, ModelArtifactDescriptor,
    ModelBundleDescriptor, PROVIDER_MANIFEST_NAME, PluginCapabilityDescriptor,
    PluginFileDescriptor, PluginManifest, PluginPermissions, PluginTargetDescriptor,
    ResourceEnvelope, load_installed_manifest,
};
pub use process::{
    DiarizationParameters, OcrCapabilityResponse, OcrParameters, ProcessCapability,
    ProcessDiarizer, ProcessOcrEngine, ProcessTranscriber, ReadinessParameters,
    TranscriptionParameters,
};
pub use routing::{
    CapabilityRegistry, CapabilityRoute, ProviderBinding, ProviderReference, ResolutionMode,
    RouteError,
};

/// Capability-provider host API implemented by this build.
pub const HOST_API_VERSION: u32 = 1;

/// Rust target triple used for package selection.
#[must_use]
pub const fn current_target() -> &'static str {
    if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_arch = "x86_64", target_os = "macos")) {
        "x86_64-apple-darwin"
    } else if cfg!(all(target_arch = "x86_64", target_os = "linux")) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_arch = "aarch64", target_os = "linux")) {
        "aarch64-unknown-linux-gnu"
    } else if cfg!(all(target_arch = "x86_64", target_os = "windows")) {
        "x86_64-pc-windows-msvc"
    } else if cfg!(all(target_arch = "aarch64", target_os = "windows")) {
        "aarch64-pc-windows-msvc"
    } else {
        "unsupported-target"
    }
}
