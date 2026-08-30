//! Public aggregate-versus-collecting parity gates for issue 272.

use into_markdown::*;
use into_markdown_converters::{
    EmbeddedVisualOcrEnricher, HintFormatDetector, MemorySourceResolver, PresentationConverter,
    WorkbookConverter,
};
use std::fmt::Write as _;
use std::future::Future;
use std::io::{Cursor, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

fn test_fixture_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = std::task::Waker::noop();
    let mut context = std::task::Context::from_waker(waker);
    loop {
        match future.as_mut().poll(&mut context) {
            std::task::Poll::Ready(output) => return output,
            std::task::Poll::Pending => std::thread::yield_now(),
        }
    }
}

fn aggregate(
    converter: &dyn Converter,
    bytes: Arc<[u8]>,
    name: &str,
    format: InputFormat,
) -> ConverterOutput {
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let input = ResolvedInput {
        metadata: SourceMetadata {
            name: Some(name.into()),
            size: u64::try_from(bytes.len()).unwrap(),
            ..SourceMetadata::default()
        },
        bytes,
    };
    let candidate = FormatCandidate::new(format, 1.0, "test.aggregate.authority");
    let plan = converter.planned_output_bytes(&input, &candidate, &options, &context).unwrap();
    let mut admission = context.reserve_memory(plan).unwrap();
    let credit = context.with_memory_credit(&mut admission).unwrap();
    block_on(converter.convert(&input, &candidate, &options, &Services::default(), &credit))
        .unwrap()
}

fn assert_public_matches_aggregate(
    converter: &dyn Converter,
    bytes: Arc<[u8]>,
    name: &str,
    format: InputFormat,
) {
    let expected = aggregate(converter, Arc::clone(&bytes), name, format);
    let expected_markdown =
        render_markdown(&expected.document, &expected.assets, &ConversionOptions::default())
            .unwrap();
    let result =
        block_on(default_engine().unwrap().convert(ConversionRequest::new(InputRef::Bytes {
            data: bytes,
            name: Some(name.into()),
        })))
        .unwrap();
    assert_eq!(result.document, expected.document);
    assert_eq!(result.markdown, expected_markdown);
    assert_eq!(result.assets, expected.assets);
    assert_eq!(result.diagnostics, expected.diagnostics);
}

fn public(bytes: Arc<[u8]>, name: &str) -> ConversionResult {
    block_on(
        default_engine().unwrap().convert(ConversionRequest::new(InputRef::Bytes {
            data: bytes,
            name: Some(name.into()),
        })),
    )
    .unwrap()
}

fn assert_results_equal(actual: &ConversionResult, expected: &ConversionResult) {
    assert_eq!(actual.document, expected.document);
    assert_eq!(actual.markdown.as_bytes(), expected.markdown.as_bytes());
    assert_eq!(actual.assets, expected.assets);
    assert_eq!(actual.diagnostics, expected.diagnostics);
}

struct AggregateOnly<T>(T);

impl<T: Converter> Converter for AggregateOnly<T> {
    fn id(&self) -> &'static str {
        self.0.id()
    }

    fn priority(&self) -> i32 {
        self.0.priority()
    }

    fn supported_formats(&self) -> &'static [InputFormat] {
        self.0.supported_formats()
    }

    fn probe<'a>(
        &'a self,
        input: &'a ResolvedInput,
        candidate: &'a FormatCandidate,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ProbeOutcome, ConversionError>> {
        self.0.probe(input, candidate, context)
    }

    fn planned_output_bytes(
        &self,
        input: &ResolvedInput,
        candidate: &FormatCandidate,
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<u64, ConversionError> {
        self.0.planned_output_bytes(input, candidate, options, context)
    }

    fn convert<'a>(
        &'a self,
        input: &'a ResolvedInput,
        candidate: &'a FormatCandidate,
        options: &'a ConversionOptions,
        services: &'a Services,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
        self.0.convert(input, candidate, options, services, context)
    }
}

fn aggregate_engine(converter: Arc<dyn Converter>) -> Engine {
    let mut builder = EngineBuilder::new()
        .renderer(Arc::new(into_markdown_render_markdown::GfmRenderer))
        .enricher(Arc::new(EmbeddedVisualOcrEnricher))
        .enricher(Arc::new(StructuredAiPatchEnricher));
    builder
        .registry_mut()
        .register_source_resolver(Arc::new(MemorySourceResolver))
        .register_format_detector(Arc::new(HintFormatDetector))
        .register_converter(converter);
    builder.build().unwrap()
}

fn convert_with(engine: &Engine, bytes: Arc<[u8]>, name: &str) -> ConversionResult {
    block_on(
        engine.convert(ConversionRequest::new(InputRef::Bytes {
            data: bytes,
            name: Some(name.into()),
        })),
    )
    .unwrap()
}

#[derive(Clone, Copy, Default)]
struct Profile {
    elapsed_us: u128,
    logical_memory_peak: u64,
    temporary_peak: u64,
    rss_delta_peak: u64,
}

fn profile_conversion(engine: &Engine, bytes: Arc<[u8]>, name: &str) -> Profile {
    let request = ConversionRequest::new(InputRef::Bytes { data: bytes, name: Some(name.into()) });
    let context = ExecutionContext::new(request.execution.clone(), request.options.limits.clone());
    let limit = request.options.limits.max_memory_bytes;
    let stop = Arc::new(AtomicBool::new(false));
    let memory_peak = Arc::new(AtomicU64::new(0));
    let temporary_peak = Arc::new(AtomicU64::new(0));
    let rss_peak = Arc::new(AtomicU64::new(0));
    let baseline_rss = process_rss();
    let sampler_context = context.clone();
    let sampler_stop = Arc::clone(&stop);
    let sampler_memory = Arc::clone(&memory_peak);
    let sampler_temporary = Arc::clone(&temporary_peak);
    let sampler_rss = Arc::clone(&rss_peak);
    let sampler = std::thread::spawn(move || {
        use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
        let pid = Pid::from_u32(std::process::id());
        let mut system = System::new();
        while !sampler_stop.load(Ordering::Acquire) {
            sampler_memory.fetch_max(
                limit.saturating_sub(sampler_context.available_memory_bytes()),
                Ordering::Relaxed,
            );
            sampler_temporary
                .fetch_max(sampler_context.reserved_temporary_bytes(), Ordering::Relaxed);
            system.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[pid]),
                false,
                ProcessRefreshKind::nothing().with_memory(),
            );
            let rss = system.process(pid).map_or(0, sysinfo::Process::memory);
            sampler_rss.fetch_max(rss, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    let started = Instant::now();
    let result = block_on(engine.convert_with_context(request, context)).unwrap();
    let elapsed_us = started.elapsed().as_micros();
    stop.store(true, Ordering::Release);
    sampler.join().unwrap();
    drop(result);
    Profile {
        elapsed_us,
        logical_memory_peak: memory_peak.load(Ordering::Relaxed),
        temporary_peak: temporary_peak.load(Ordering::Relaxed),
        rss_delta_peak: rss_peak.load(Ordering::Relaxed).saturating_sub(baseline_rss),
    }
}

fn process_rss() -> u64 {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let pid = Pid::from_u32(std::process::id());
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        false,
        ProcessRefreshKind::nothing().with_memory(),
    );
    system.process(pid).map_or(0, sysinfo::Process::memory)
}

fn alternating_profile(
    native: &Engine,
    aggregate: &Engine,
    bytes: &Arc<[u8]>,
    name: &str,
) -> (Profile, Profile) {
    const RUNS: u128 = 4;
    drop(convert_with(native, Arc::clone(bytes), name));
    drop(convert_with(aggregate, Arc::clone(bytes), name));
    let mut native_total = Profile::default();
    let mut aggregate_total = Profile::default();
    for run in 0..RUNS {
        let (first, second) =
            if run.is_multiple_of(2) { (native, aggregate) } else { (aggregate, native) };
        let first_profile = profile_conversion(first, Arc::clone(bytes), name);
        let second_profile = profile_conversion(second, Arc::clone(bytes), name);
        let (native_profile, aggregate_profile) = if run.is_multiple_of(2) {
            (first_profile, second_profile)
        } else {
            (second_profile, first_profile)
        };
        merge_profile(&mut native_total, native_profile);
        merge_profile(&mut aggregate_total, aggregate_profile);
    }
    native_total.elapsed_us /= RUNS;
    aggregate_total.elapsed_us /= RUNS;
    (native_total, aggregate_total)
}

fn merge_profile(total: &mut Profile, sample: Profile) {
    total.elapsed_us += sample.elapsed_us;
    total.logical_memory_peak = total.logical_memory_peak.max(sample.logical_memory_peak);
    total.temporary_peak = total.temporary_peak.max(sample.temporary_peak);
    total.rss_delta_peak = total.rss_delta_peak.max(sample.rss_delta_peak);
}

fn replace_zip_entry(source: &[u8], target: &str, replacement: &[u8]) -> Vec<u8> {
    let mut input = zip::ZipArchive::new(Cursor::new(source)).unwrap();
    let mut output = Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut output);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for index in 0..input.len() {
            let mut entry = input.by_index(index).unwrap();
            if entry.is_dir() {
                writer.add_directory(entry.name(), options).unwrap();
                continue;
            }
            writer.start_file(entry.name(), options).unwrap();
            if entry.name() == target {
                writer.write_all(replacement).unwrap();
            } else {
                std::io::copy(&mut entry, &mut writer).unwrap();
            }
        }
        writer.finish().unwrap();
    }
    output.into_inner()
}

fn zip_entry(source: &[u8], target: &str) -> String {
    let mut archive = zip::ZipArchive::new(Cursor::new(source)).unwrap();
    let mut entry = archive.by_name(target).unwrap();
    let mut value = String::new();
    std::io::Read::read_to_string(&mut entry, &mut value).unwrap();
    value
}

fn large_sparse_worksheet() -> Vec<u8> {
    const ROWS: usize = 4_096;
    let mut xml = String::from(
        "<?xml version=\"1.0\"?><worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><dimension ref=\"A1:A65521\"/><sheetData>",
    );
    for index in 0..ROWS {
        let row = index * 16 + 1;
        write!(
            xml,
            "<row r=\"{row}\"><c r=\"A{row}\" t=\"inlineStr\"><is><t>row-{index:04}-sparse-production-content-{index:04}</t></is></c></row>"
        )
        .unwrap();
    }
    xml.push_str("</sheetData></worksheet>");
    assert!(xml.len() >= 256 * 1024);
    xml.into_bytes()
}

fn large_presentation(source: &[u8]) -> Vec<u8> {
    let original = zip_entry(source, "ppt/slides/slide1.xml");
    let marker = "</p:txBody></p:sp></p:spTree>";
    let mut paragraphs = String::new();
    for index in 0..2_048 {
        write!(
            paragraphs,
            "<a:p><a:r><a:rPr lang=\"en-US\"/><a:t>slide-production-{index:04}-{}</a:t></a:r></a:p>",
            "content".repeat(8)
        )
        .unwrap();
    }
    let replacement = original.replacen(marker, &format!("{paragraphs}{marker}"), 1);
    assert!(replacement.len() > original.len() + 256 * 1024);
    replace_zip_entry(source, "ppt/slides/slide1.xml", replacement.as_bytes())
}

#[test]
fn pptx_collecting_matches_aggregate_document_markdown_assets_and_diagnostics() {
    let small = std::fs::read(test_fixture_root().join("small/pptx/normal.pptx")).unwrap();
    assert_public_matches_aggregate(
        &PresentationConverter,
        Arc::from(small.clone()),
        "ordinary.pptx",
        InputFormat::Pptx,
    );
    let bytes: Arc<[u8]> = large_presentation(&small).into();
    assert_public_matches_aggregate(
        &PresentationConverter,
        bytes,
        "normal.pptx",
        InputFormat::Pptx,
    );
}

#[test]
fn large_sparse_xlsx_collecting_matches_aggregate_while_small_uses_fallback() {
    let small = std::fs::read(test_fixture_root().join("small/xlsx/normal.xlsx")).unwrap();
    let aggregate = aggregate_engine(Arc::new(AggregateOnly(WorkbookConverter)));
    let small: Arc<[u8]> = small.into();
    let expected_small = convert_with(&aggregate, Arc::clone(&small), "ordinary.xlsx");
    let collected_small = public(Arc::clone(&small), "ordinary.xlsx");
    assert_results_equal(&collected_small, &expected_small);

    let large: Arc<[u8]> =
        replace_zip_entry(&small, "xl/worksheets/sheet1.xml", &large_sparse_worksheet()).into();
    let expected = convert_with(&aggregate, Arc::clone(&large), "normal.xlsx");
    let collected = public(large, "normal.xlsx");
    assert_results_equal(&collected, &expected);
}

#[test]
fn production_large_inputs_have_bounded_native_overhead_and_resources() {
    let pptx_small = std::fs::read(test_fixture_root().join("small/pptx/normal.pptx")).unwrap();
    let pptx: Arc<[u8]> = large_presentation(&pptx_small).into();
    let pptx_native = default_engine().unwrap();
    let pptx_aggregate = aggregate_engine(Arc::new(AggregateOnly(PresentationConverter)));
    let (pptx_native_profile, pptx_aggregate_profile) =
        alternating_profile(&pptx_native, &pptx_aggregate, &pptx, "production-large.pptx");
    let (small_pptx_native_profile, small_pptx_aggregate_profile) =
        alternating_profile(&pptx_native, &pptx_aggregate, &Arc::from(pptx_small), "ordinary.pptx");

    let xlsx_small = std::fs::read(test_fixture_root().join("small/xlsx/normal.xlsx")).unwrap();
    let xlsx: Arc<[u8]> =
        replace_zip_entry(&xlsx_small, "xl/worksheets/sheet1.xml", &large_sparse_worksheet())
            .into();
    let xlsx_native = default_engine().unwrap();
    let xlsx_aggregate = aggregate_engine(Arc::new(AggregateOnly(WorkbookConverter)));
    let (xlsx_native_profile, xlsx_aggregate_profile) =
        alternating_profile(&xlsx_native, &xlsx_aggregate, &xlsx, "production-sparse.xlsx");
    let (small_xlsx_native_profile, small_xlsx_aggregate_profile) =
        alternating_profile(&xlsx_native, &xlsx_aggregate, &Arc::from(xlsx_small), "ordinary.xlsx");

    for (format, native, aggregate) in [
        ("large-pptx", &pptx_native_profile, &pptx_aggregate_profile),
        ("large-xlsx", &xlsx_native_profile, &xlsx_aggregate_profile),
        ("ordinary-pptx", &small_pptx_native_profile, &small_pptx_aggregate_profile),
        ("ordinary-xlsx", &small_xlsx_native_profile, &small_xlsx_aggregate_profile),
    ] {
        assert!(
            native.elapsed_us <= aggregate.elapsed_us.saturating_mul(3) / 2 + 5_000,
            "{format} native average {}us exceeds aggregate {}us by more than 50%",
            native.elapsed_us,
            aggregate.elapsed_us
        );
        assert_eq!(native.temporary_peak, 0, "{format} native temporary bytes");
        assert!(native.logical_memory_peak <= 2 * 1024 * 1024 * 1024);
        println!(
            "production profile {format}: native={}us aggregate={}us native_logical_peak={} aggregate_logical_peak={} native_rss_delta={} aggregate_rss_delta={} native_temp={} aggregate_temp={}",
            native.elapsed_us,
            aggregate.elapsed_us,
            native.logical_memory_peak,
            aggregate.logical_memory_peak,
            native.rss_delta_peak,
            aggregate.rss_delta_peak,
            native.temporary_peak,
            aggregate.temporary_peak,
        );
    }
}

#[test]
fn compound_file_pptx_preserves_the_encrypted_error() {
    let mut request = ConversionRequest::new(InputRef::Bytes {
        data: Arc::from(&b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1"[..]),
        name: Some("encrypted.pptx".into()),
    });
    request.hint.format = Some(InputFormat::Pptx);
    assert!(matches!(
        block_on(default_engine().unwrap().convert(request)),
        Err(ConversionError::Encrypted)
    ));
}
