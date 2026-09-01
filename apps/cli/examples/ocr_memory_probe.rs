//! Local, explicit provider-quota measurement; never included in release products.
use into_markdown::{
    ConversionOptions, ExecutionContext, ExecutionOptions, OcrEngine, OcrRecognition, OcrRequest,
    ResourceLimits,
};
use into_markdown_process_plugin::{PluginManifest as ProcessManifest, ProcessPlugin};
use into_markdown_provider_plugin::{PluginManifest, ProcessCapability, ProviderBinding};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

const MAX_SIGNED_WORKER_MIB: u64 = 2048;

fn engine(root: &Path, bytes: u64) -> Result<impl OcrEngine + use<>, Box<dyn std::error::Error>> {
    let descriptor = std::fs::read(root.join("provider.json"))?;
    let manifest: PluginManifest = serde_json::from_slice(&descriptor)?;
    manifest.validate()?;
    let target =
        manifest.target(into_markdown_provider_plugin::current_target()).ok_or("target")?;
    for file in &target.files {
        let data = std::fs::read(root.join(&file.path))?;
        if data.len() as u64 != file.bytes || format!("{:x}", Sha256::digest(&data)) != file.sha256
        {
            return Err(format!("runtime identity changed: {}", file.path).into());
        }
    }
    let capability = manifest.capabilities.iter().find(|item| item.id == "ocr").ok_or("OCR")?;
    let binding = ProviderBinding {
        plugin_id: manifest.id.clone(),
        plugin_version: manifest.version.clone(),
        manifest_sha256: format!("{:x}", Sha256::digest(&descriptor)),
        capability_id: capability.id.clone(),
        provider_id: capability.provider_id.clone(),
        install_root: root.to_owned(),
    };
    let policy = ProcessCapability::runtime_policy(&manifest, &binding)?;
    let entry = target.files.iter().find(|file| file.path == target.entrypoint).ok_or("entry")?;
    // Copy-before-launch owns the complete private runtime; the caller does not
    // need to claim that its input directory is an immutable installed cache.
    let process = ProcessPlugin::new(
        ProcessManifest {
            plugin_id: manifest.id.clone(),
            executable: root.join(&target.entrypoint),
            runtime_root: root.to_owned(),
            executable_sha256: entry.sha256.clone(),
            protocol_versions: vec![1],
        },
        policy,
    )?;
    let mut options = ConversionOptions::default();
    options.limits.max_memory_bytes = bytes;
    Ok(ProcessCapability::new_embedded(process, &manifest, binding)?.ocr(options)?)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 3 && args.len() != 5 {
        return Err("usage: ocr_memory_probe RUNTIME_ROOT INPUT WORKER_MIB [auto|always|off best-effort|strict]".into());
    }
    let bytes = args[2].parse::<u64>()?.checked_mul(1024 * 1024).ok_or("quota overflow")?;
    if bytes == 0 || bytes > MAX_SIGNED_WORKER_MIB * 1024 * 1024 {
        return Err(format!("probe quota must be between 1 and {MAX_SIGNED_WORKER_MIB} MiB").into());
    }
    let image = std::fs::read(&args[1])?;
    let root = PathBuf::from(&args[0]).canonicalize()?;
    let engine = engine(&root, bytes)?;
    let context = ExecutionContext::new(
        ExecutionOptions { timeout: Some(Duration::from_secs(3600)), ..Default::default() },
        ResourceLimits::default(),
    );
    let start = Instant::now();
    if args.len() == 5 {
        return convert(&args, engine, context, bytes, start);
    }
    let mut work = context.reserve_memory(bytes + 160 * 1024 * 1024)?;
    let credited = context.with_memory_credit(&mut work)?;
    let request = OcrRequest { image: &image, media_type: "image/png", languages: &["en"] };
    let mut future = engine.recognize_bound(request, &credited);
    let result = match future.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(result) => result,
        Poll::Pending => return Err("process adapter unexpectedly yielded".into()),
    };
    drop(future);
    let outcome = match result {
        Ok(OcrRecognition::Bound(result)) => {
            let (recognized, _, _) = result.into_parts();
            serde_json::json!({"status":"success", "regions":recognized.regions.len()})
        }
        Ok(_) => return Err("process provider omitted bound recognition".into()),
        Err(error) => serde_json::json!({"status":"failed", "code":error.code().as_str(),
                                       "reason":error.reason_code(), "detail":error.to_string()}),
    };
    drop(credited);
    drop(work);
    println!(
        "{}",
        serde_json::json!({"workerBudgetBytes":bytes,
        "imageSha256":format!("{:x}",Sha256::digest(&image)), "outcome":outcome,
        "elapsedMs":start.elapsed().as_millis(), "memoryAfter":context.reserved_memory_bytes(),
        "temporaryAfter":context.reserved_temporary_bytes(),
        "leasePeakBytes":context.resource_usage().shared_lease_peak_bytes})
    );
    Ok(())
}

fn convert(
    args: &[String],
    ocr: impl OcrEngine + 'static,
    context: ExecutionContext,
    bytes: u64,
    start: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    use into_markdown::{
        AssetMode, ConversionRequest, DtoJsonStyle, ErrorPolicy, InputRef, OcrPolicy, ResultDto,
        Services, default_engine_with_services,
    };
    let engine = default_engine_with_services(Services {
        ocr: Some(std::sync::Arc::new(ocr)),
        ..Default::default()
    })?;
    let mut request = ConversionRequest::new(InputRef::Path(PathBuf::from(&args[1])));
    request.options.ocr.policy = match args[3].as_str() {
        "auto" => OcrPolicy::Auto,
        "always" => OcrPolicy::Always,
        "off" => OcrPolicy::Off,
        _ => return Err("invalid OCR policy".into()),
    };
    request.options.error_policy = match args[4].as_str() {
        "strict" => ErrorPolicy::Strict,
        "best-effort" => ErrorPolicy::BestEffort,
        _ => return Err("invalid error policy".into()),
    };
    request.options.output.asset_mode = AssetMode::Omit;
    let result = futures::executor::block_on(engine.convert_with_context(request, context.clone()));
    let outcome = match result {
        Ok(result) => {
            let dto = ResultDto::json_from_result(&result, DtoJsonStyle::Compact)?;
            serde_json::json!({"status":"success", "result":serde_json::from_str::<serde_json::Value>(&dto)?})
        }
        Err(error) => serde_json::json!({"status":"failed", "code":error.code().as_str(),
                                       "reason":error.reason_code(), "detail":error.to_string()}),
    };
    let usage = context.resource_usage();
    let runtime = context.ocr_runtime_usage();
    println!(
        "{}",
        serde_json::json!({"workerBudgetBytes":bytes, "outcome":outcome,
        "elapsedMs":start.elapsed().as_millis(), "memoryAfter":context.reserved_memory_bytes(),
        "temporaryAfter":context.reserved_temporary_bytes(),
        "leasePeakBytes":usage.shared_lease_peak_bytes,
        "ocrRequests":runtime.requests, "recognitionMemoryRefusals":runtime.recognition_memory_refusals,
        "recognizedRegions":usage.ocr_recognized_regions, "recognizedChars":usage.ocr_recognized_chars})
    );
    Ok(())
}
