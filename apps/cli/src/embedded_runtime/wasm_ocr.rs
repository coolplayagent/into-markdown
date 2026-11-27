//! PP-OCR for a restricted Windows token. Only tensors cross the WebAssembly boundary.

use super::{OCR_WASM_MODULE, OCR_WASM_SHA256, ocr_payload, read_archive_member, runtime_error};
use into_markdown::{
    BoxFuture, ConversionError, ConversionOptions, ExecutionContext, OcrEngine, Tensor,
    TensorRuntime,
};
use into_markdown_ocr::{ModelManifest, PpOcrImageEngine};
use sha2::{Digest as _, Sha256};
use std::sync::{Arc, mpsc};
use std::time::Duration;
use wasmtime::{Config, Engine, Instance, Memory, Module, ResourceLimiter, Store, UpdateDeadline};

const DETECTOR: &str = "pp-ocrv6-tiny-detector-onnx";
const RECOGNIZER: &str = "pp-ocrv6-tiny-recognizer-onnx";
const MAX_GUEST_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_GUEST_FUEL: u64 = 10_000_000_000_000;

pub(super) fn planner(options: &ConversionOptions) -> Result<Arc<dyn OcrEngine>, ConversionError> {
    Ok(Arc::new(PpOcrImageEngine::from_embedded(
        Arc::new(into_markdown_ocr::PlaceholderTensorRuntime),
        Vec::new(),
        (options.limits.max_memory_bytes / 2).min(MAX_GUEST_BYTES),
        0,
        false,
        options.limits.clone(),
    )?))
}

pub(super) fn engine(
    options: &ConversionOptions,
    context: &ExecutionContext,
    source_owns_resident: bool,
) -> Result<Arc<dyn OcrEngine>, ConversionError> {
    let allocation = reserve_setup(options, context)?;
    load(options, context, allocation, source_owns_resident)
}

pub(super) fn reserve_setup(
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<into_markdown::ResourceReservation, ConversionError> {
    let bytes = setup_bytes();
    if bytes >= (options.limits.max_memory_bytes / 2).min(MAX_GUEST_BYTES) {
        return Err(memory_error("embedded OCR setup exceeds the worker budget"));
    }
    context.reserve_memory(bytes)
}

fn setup_bytes() -> u64 {
    OCR_WASM_MODULE.len() as u64 * 32
        + ocr_payload()
            .files
            .iter()
            .filter(|file| file.path.starts_with("models/"))
            .map(|file| file.bytes)
            .sum::<u64>()
}

pub(super) fn load(
    options: &ConversionOptions,
    context: &ExecutionContext,
    mut allocation: into_markdown::ResourceReservation,
    source_owns_resident: bool,
) -> Result<Arc<dyn OcrEngine>, ConversionError> {
    context.checkpoint()?;
    if OCR_WASM_MODULE.is_empty()
        || OCR_WASM_MODULE.len() > 16 * 1024 * 1024
        || format!("{:x}", Sha256::digest(OCR_WASM_MODULE)) != OCR_WASM_SHA256
    {
        return Err(runtime_error("embedded OCR WebAssembly module digest mismatch".into()));
    }
    // Charge compiler scratch, compiled code and the authenticated model/dictionary buffers
    // before decompressing or compiling. The module and inventories are fixed at build time.
    let budget = (options.limits.max_memory_bytes / 2).min(MAX_GUEST_BYTES);
    let manifest = ModelManifest::embedded()?;
    let detector = model_artifact(&manifest, DETECTOR, "detector")?;
    context.checkpoint()?;
    let recognizer = model_artifact(&manifest, RECOGNIZER, "recognizer")?;
    context.checkpoint()?;
    let dictionary = model_artifact(&manifest, RECOGNIZER, "character-table")?;
    let mut config = Config::new();
    config
        .consume_fuel(true)
        .epoch_interruption(true)
        .wasm_multi_memory(false)
        .memory_reservation(MAX_GUEST_BYTES);
    let engine = Engine::new(&config).map_err(wasm_error)?;
    let module = Module::from_binary(&engine, OCR_WASM_MODULE).map_err(wasm_error)?;
    if module.imports().next().is_some() {
        return Err(runtime_error("embedded OCR worker requests a host import".into()));
    }
    context.checkpoint()?;
    // Retain the compiled image, an allowance for engine metadata, and all model
    // buffers. Release the remaining compiler scratch after the compilation ends.
    let image = module.image_range();
    let resident_bytes = (image.end as usize - image.start as usize) as u64
        + OCR_WASM_MODULE.len() as u64
        + detector.capacity() as u64
        + recognizer.capacity() as u64
        + dictionary.capacity() as u64;
    let setup = setup_bytes();
    if resident_bytes > setup {
        allocation.grow(resident_bytes - setup)?;
    } else {
        allocation.shrink(setup - resident_bytes)?;
    }
    let runtime =
        Arc::new(WasmRuntime { module, detector, recognizer, budget, _resident: allocation });
    Ok(Arc::new(PpOcrImageEngine::from_embedded(
        runtime,
        dictionary,
        budget,
        resident_bytes,
        source_owns_resident,
        options.limits.clone(),
    )?))
}

fn model_artifact(
    manifest: &ModelManifest,
    model: &str,
    role: &str,
) -> Result<Vec<u8>, ConversionError> {
    let artifact = manifest
        .bundles
        .iter()
        .find(|bundle| bundle.id == model)
        .and_then(|bundle| bundle.runtime_artifacts.iter().find(|item| item.role == role))
        .ok_or_else(|| runtime_error(format!("missing official OCR {role} authority")))?;
    let member = format!("models/{model}/{}", artifact.file_name);
    let bytes =
        read_archive_member(ocr_payload(), &member, artifact.size).map_err(runtime_error)?;
    if bytes.len() as u64 != artifact.size
        || format!("{:x}", Sha256::digest(&bytes)) != artifact.sha256
    {
        return Err(runtime_error(format!("official OCR {role} digest mismatch")));
    }
    Ok(bytes)
}

struct WasmRuntime {
    module: Module,
    detector: Vec<u8>,
    recognizer: Vec<u8>,
    budget: u64,
    _resident: into_markdown::ResourceReservation,
}

impl TensorRuntime for WasmRuntime {
    fn id(&self) -> &'static str {
        "rten-wasm-cpu"
    }

    fn run<'a>(
        &'a self,
        model_id: &'a str,
        inputs: &'a [Tensor],
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<Vec<Tensor>, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            let [input] = inputs else {
                return Err(runtime_error("unexpected OCR worker input count".into()));
            };
            let model = match model_id {
                DETECTOR => &self.detector,
                RECOGNIZER => &self.recognizer,
                _ => return Err(runtime_error("unknown OCR worker model".into())),
            };
            let limits = WorkerLimits {
                allocation: context.reserve_memory(0)?,
                bytes: usize::try_from(self.budget)
                    .map_err(|_| memory_error("worker limit overflow"))?,
            };
            let mut store = Store::new(self.module.engine(), limits);
            store.limiter(|limits| limits);
            store.set_fuel(MAX_GUEST_FUEL).map_err(wasm_error)?;
            store.set_epoch_deadline(1);
            let controls = context.clone();
            store.epoch_deadline_callback(move |_| {
                controls.checkpoint().map_err(wasmtime::Error::msg)?;
                Ok(UpdateDeadline::Continue(1))
            });
            let instance = Instance::new(&mut store, &self.module, &[]).map_err(wasm_error)?;
            let memory = instance
                .get_memory(&mut store, "memory")
                .ok_or_else(|| runtime_error("OCR worker memory missing".into()))?;
            let result = std::thread::scope(|scope| {
                let (finished, wait) = mpsc::channel::<()>();
                // Every store checks its own request controls even when several calls share this engine.
                let clock = std::thread::Builder::new()
                    .name("ocr-wasm-epoch".into())
                    .spawn_scoped(scope, move || {
                        while wait.recv_timeout(Duration::from_millis(20))
                            == Err(mpsc::RecvTimeoutError::Timeout)
                        {
                            self.module.engine().increment_epoch();
                        }
                    })
                    .map_err(|error| runtime_error(format!("start OCR epoch clock: {error}")))?;
                let result = infer(&mut store, instance, memory, model_id, model, input, context);
                drop(finished);
                clock.join().map_err(|_| runtime_error("OCR epoch clock failed".into()))?;
                result
            });
            context.checkpoint()?;
            result.map(|tensor| vec![tensor])
        })
    }
}

fn infer(
    store: &mut Store<WorkerLimits>,
    instance: Instance,
    memory: Memory,
    model_id: &str,
    model: &[u8],
    input: &Tensor,
    context: &ExecutionContext,
) -> Result<Tensor, ConversionError> {
    let [batch, channels, height, width] = input.shape[..] else {
        return Err(runtime_error("invalid OCR input rank".into()));
    };
    if channels != 3
        || input.shape.iter().try_fold(1usize, |count, &n| count.checked_mul(n))
            != Some(input.values.len())
    {
        return Err(runtime_error("invalid OCR input dimensions".into()));
    }
    let model_buffer =
        instance.get_typed_func::<u32, u32>(&mut *store, "model_buffer").map_err(wasm_error)?;
    let input_buffer =
        instance.get_typed_func::<u32, u32>(&mut *store, "input_buffer").map_err(wasm_error)?;
    let inference = instance
        .get_typed_func::<(u32, u32, u32, u32), u32>(&mut *store, "infer")
        .map_err(wasm_error)?;
    let dimension =
        instance.get_typed_func::<u32, u32>(&mut *store, "output_dimension").map_err(wasm_error)?;
    let output_buffer =
        instance.get_typed_func::<(), u32>(&mut *store, "output_buffer").map_err(wasm_error)?;
    let offset = model_buffer.call(&mut *store, to_u32(model.len())?).map_err(wasm_error)?;
    memory.write(&mut *store, offset as usize, model).map_err(wasm_error)?;
    let offset = input_buffer.call(&mut *store, to_u32(input.values.len())?).map_err(wasm_error)?;
    memory
        .write(&mut *store, offset as usize, bytemuck::cast_slice(&input.values))
        .map_err(wasm_error)?;
    let rank = inference
        .call(&mut *store, (to_u32(batch)?, 3, to_u32(height)?, to_u32(width)?))
        .map_err(wasm_error)?;
    let expected_rank = if model_id == DETECTOR { 4 } else { 3 };
    if rank != expected_rank {
        return Err(runtime_error("OCR worker returned an invalid tensor rank".into()));
    }
    let shape = (0..rank)
        .map(|axis| dimension.call(&mut *store, axis).map(|n| n as usize).map_err(wasm_error))
        .collect::<Result<Vec<_>, _>>()?;
    let valid = if model_id == DETECTOR {
        shape == [1, 1, height, width]
    } else {
        shape[0] == batch && shape[1] > 0 && shape[1] <= width && shape[2] == 6906
    };
    if !valid {
        return Err(runtime_error("OCR worker returned invalid tensor dimensions".into()));
    }
    let count = shape
        .iter()
        .try_fold(1usize, |n, &dimension| n.checked_mul(dimension))
        .ok_or_else(|| memory_error("worker output size overflow"))?;
    let bytes = count.checked_mul(4).ok_or_else(|| memory_error("worker output size overflow"))?;
    let _lease = context.reserve_memory(bytes as u64)?;
    let offset = output_buffer.call(&mut *store, ()).map_err(wasm_error)? as usize;
    if offset.checked_add(bytes).is_none_or(|end| end > memory.data_size(&*store)) {
        return Err(runtime_error("OCR worker result exceeds its memory".into()));
    }
    let mut values = Vec::new();
    values.try_reserve_exact(count).map_err(|_| memory_error("worker output allocation failed"))?;
    values.resize(count, 0.0f32);
    memory.read(&*store, offset, bytemuck::cast_slice_mut(&mut values)).map_err(wasm_error)?;
    Ok(Tensor { shape, values })
}

fn to_u32(value: usize) -> Result<u32, ConversionError> {
    u32::try_from(value).map_err(|_| memory_error("worker input exceeds WebAssembly bounds"))
}

fn wasm_error(error: impl Into<wasmtime::Error>) -> ConversionError {
    let error = error.into();
    if let Some(error) = error.downcast_ref::<ConversionError>() {
        return error.clone();
    }
    if matches!(error.downcast_ref::<wasmtime::Trap>(), Some(wasmtime::Trap::OutOfFuel)) {
        return ConversionError::ResourceLimit {
            limit: "ocrWasmFuel",
            detail: "OCR inference instruction budget exhausted".into(),
        };
    }
    runtime_error(format!("embedded OCR WebAssembly worker: {error}"))
}

struct WorkerLimits {
    allocation: into_markdown::ResourceReservation,
    bytes: usize,
}

impl ResourceLimiter for WorkerLimits {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if desired > self.bytes || maximum.is_some_and(|maximum| desired > maximum) {
            return Err(wasmtime::Error::new(memory_error(
                "OCR worker linear memory limit exceeded",
            )));
        }
        self.allocation.grow((desired - current) as u64).map_err(wasmtime::Error::new)?;
        Ok(true)
    }

    fn memory_grow_failed(&mut self, _: wasmtime::Error) -> wasmtime::Result<()> {
        Err(wasmtime::Error::new(memory_error("OCR worker memory growth failed")))
    }

    fn table_growing(
        &mut self,
        _: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(desired <= 65_536 && maximum.is_none_or(|limit| desired <= limit))
    }

    fn instances(&self) -> usize {
        1
    }
    fn tables(&self) -> usize {
        1
    }
    fn memories(&self) -> usize {
        1
    }
}

fn memory_error(detail: &str) -> ConversionError {
    ConversionError::ResourceLimit { limit: "max_memory_bytes", detail: detail.into() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wasm_limit_returns_typed_memory_refusal() {
        let context = ExecutionContext::new(Default::default(), Default::default());
        let mut limits =
            WorkerLimits { bytes: 131_072, allocation: context.reserve_memory(0).unwrap() };
        assert!(limits.memory_growing(65_536, 131_072, None).unwrap());
        let error = limits.memory_growing(131_072, 196_608, None).unwrap_err();
        assert!(matches!(
            wasm_error(error),
            ConversionError::ResourceLimit { limit: "max_memory_bytes", .. }
        ));
        assert!(!limits.table_growing(0, 65_537, None).unwrap());
    }

    #[test]
    fn embedded_ocr_checks_cancellation_before_running_a_model() {
        let options = ConversionOptions::default();
        let setup = ExecutionContext::new(Default::default(), options.limits.clone());
        let ocr = engine(&options, &setup, true).unwrap();
        assert!(setup.reserved_memory_bytes() > 0);
        let token = into_markdown::CancellationToken::new();
        let cancelled = ExecutionContext::new(
            into_markdown::ExecutionOptions { cancellation: token.clone(), ..Default::default() },
            options.limits,
        );
        token.cancel();
        let request =
            into_markdown::OcrRequest { image: &[], media_type: "image/png", languages: &[] };
        assert!(matches!(
            futures::executor::block_on(ocr.recognize_bound(request, &cancelled)),
            Err(ConversionError::Cancelled)
        ));
        drop(ocr);
        assert_eq!(setup.reserved_memory_bytes(), 0);
    }

    #[test]
    fn source_runtime_reuses_one_root_lease_across_page_credits() {
        use super::super::{LazyEmbeddedOcr, OcrIsolation};

        let options = ConversionOptions::default();
        let root = ExecutionContext::new(Default::default(), options.limits.clone());
        let service = LazyEmbeddedOcr {
            options,
            engine: Default::default(),
            isolation: OcrIsolation::WasmSource(root.clone()),
        };
        let mut allowance = root.reserve_memory(1024).unwrap();
        let page = root.with_memory_credit(&mut allowance).unwrap();
        let first = service.get(&page, true).unwrap();
        let total = root.reserved_memory_bytes();
        assert!(total > 1024);
        assert_eq!(page.reserved_memory_bytes(), 0);
        assert!(Arc::ptr_eq(&first, &service.get(&page, false).unwrap()));
        assert_eq!(root.reserved_memory_bytes(), total);
        drop(page);
        allowance.shrink(1024).unwrap();
        drop(first);
        drop(service);
        assert_eq!(root.reserved_memory_bytes(), 0);
    }

    #[test]
    fn shared_service_plans_without_retaining_a_request() {
        use super::super::{LazyEmbeddedOcr, OcrIsolation};

        let options = ConversionOptions::default();
        let context = ExecutionContext::new(Default::default(), options.limits.clone());
        let service = LazyEmbeddedOcr {
            options: options.clone(),
            engine: Default::default(),
            isolation: OcrIsolation::WasmCall,
        };
        let plan = service.planned_normalized_png_output(640, 480, &options, &context).unwrap();
        assert!(plan.max_working_bytes() > 0);
        assert_eq!(context.resource_usage().shared_lease_peak_bytes, 0);
        assert!(service.engine.lock().unwrap().is_none());
    }

    #[test]
    fn converter_holding_the_entire_source_allowance_owns_only_its_live_runtime() {
        use super::super::{LazyEmbeddedOcr, OcrIsolation};

        let options = ConversionOptions::default();
        let root = ExecutionContext::new(Default::default(), options.limits.clone());
        let service = LazyEmbeddedOcr {
            options,
            engine: Default::default(),
            isolation: OcrIsolation::WasmSource(root.clone()),
        };
        let mut allowance = root.reserve_memory(root.available_memory_bytes()).unwrap();
        let page = root.with_memory_credit(&mut allowance).unwrap();
        drop(service.get(&page, true).unwrap());
        assert_eq!(page.reserved_memory_bytes(), 0);
        let runtime = service.get(&page, false).unwrap();
        assert!(page.reserved_memory_bytes() > 0);
        assert!(service.engine.lock().unwrap().is_none());
        drop(runtime);
        assert_eq!(page.reserved_memory_bytes(), 0);
        drop(page);
        drop(allowance);
        assert_eq!(root.reserved_memory_bytes(), 0);
    }
}
