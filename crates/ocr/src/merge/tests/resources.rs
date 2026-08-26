use super::*;
use into_markdown_core::{CancellationToken, ErrorCode, ListItem, ListKind, NodeId, OcrPolicy};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

struct ResetTraversalHook;

impl Drop for ResetTraversalHook {
    fn drop(&mut self) {
        set_traversal_test_hook(None);
    }
}

struct ResetTextHook;

impl Drop for ResetTextHook {
    fn drop(&mut self) {
        text::set_test_hook(None);
    }
}

struct ResetFingerprintHook;

impl Drop for ResetFingerprintHook {
    fn drop(&mut self) {
        crate::batch::set_fingerprint_test_hook(None);
    }
}

#[test]
fn region_text_and_work_limits_fail_before_publication() {
    let detection =
        detection(&[(polygon(0.0, 0.0, 20.0, 10.0), 0.9), (polygon(30.0, 0.0, 20.0, 10.0), 0.9)]);
    let recognition = recognition(&[(0, "a", 0.9), (1, "b", 0.9)]);
    for limits in [
        MergeLimits { max_regions: 1, ..MergeLimits::default() },
        MergeLimits { max_text_bytes: 1, ..MergeLimits::default() },
        MergeLimits { max_comparisons: 1, ..MergeLimits::default() },
    ] {
        let context = context();
        let error = merge_document(
            page_document(Vec::new()),
            &[input(&detection, &recognition)],
            &MergeConfig { policy: OcrPolicy::Always, limits, ..MergeConfig::default() },
            &context,
        )
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::ResourceLimit);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }
}

#[test]
fn context_memory_is_reserved_before_allocating_and_released_on_failure() {
    let detection = detection(&[(polygon(0.0, 0.0, 20.0, 10.0), 0.9)]);
    let recognition = recognition(&[(0, "text", 0.9)]);
    let resource_limits = ResourceLimits { max_memory_bytes: 1, ..ResourceLimits::default() };
    let context = ExecutionContext::new(ExecutionOptions::default(), resource_limits);
    let error = merge_document(
        page_document(Vec::new()),
        &[input(&detection, &recognition)],
        &MergeConfig { policy: OcrPolicy::Always, ..MergeConfig::default() },
        &context,
    )
    .unwrap_err();
    assert_eq!(error.code(), ErrorCode::ResourceLimit);
    assert_eq!(context.reserved_memory_bytes(), 0);
}

#[test]
fn cancellation_is_observed_before_merge_work_or_output() {
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let context = ExecutionContext::new(
        ExecutionOptions { cancellation, ..ExecutionOptions::default() },
        ResourceLimits::default(),
    );
    let error =
        merge_document(Document::default(), &[], &MergeConfig::default(), &context).unwrap_err();
    assert_eq!(error.code(), ErrorCode::Cancelled);
    assert_eq!(context.reserved_memory_bytes(), 0);
}

#[test]
fn successful_output_keeps_same_context_memory_lease_alive() {
    let detection = detection(&[(polygon(0.0, 0.0, 20.0, 10.0), 0.9)]);
    let recognition = recognition(&[(0, "text", 0.9)]);
    let context = context();
    let output = merge_document(
        Document::default(),
        &[input(&detection, &recognition)],
        &MergeConfig { policy: OcrPolicy::Always, ..MergeConfig::default() },
        &context,
    )
    .unwrap();
    assert!(context.reserved_memory_bytes() > 0);
    assert!(output.leased_memory_for(&context) > 0);
    drop(output);
    assert_eq!(context.reserved_memory_bytes(), 0);
}

#[test]
fn maximum_merge_depth_native_ir_uses_bounded_explicit_walk_stacks() {
    let mut blocks = vec![BlockNode {
        id: NodeId("deep-0".into()),
        block: Block::Paragraph(Vec::new()),
        provenance: native_provenance(None),
    }];
    for depth in 1..=6 {
        blocks = vec![BlockNode {
            id: NodeId(format!("deep-{depth}")),
            block: Block::List {
                kind: ListKind::Bullet,
                start: 1,
                items: vec![ListItem { checked: None, marker_label: None, blocks }],
            },
            provenance: native_provenance(None),
        }];
    }
    let document = Document { blocks, ..Document::default() };
    let detection = detection(&[(polygon(20.0, 20.0, 80.0, 16.0), 0.98)]);
    let recognition = recognition(&[(0, "deep", 0.98)]);
    let output = merge_document(
        document,
        &[input(&detection, &recognition)],
        &MergeConfig { policy: OcrPolicy::Always, ..MergeConfig::default() },
        &context(),
    )
    .unwrap();
    output.document.validate().unwrap();
}

#[test]
fn large_native_traversal_observes_in_flight_cancel_and_releases_merge_lease() {
    let document = Document {
        blocks: (0..20_000)
            .map(|index| BlockNode {
                id: NodeId(format!("large-{index}")),
                block: Block::Paragraph(vec![into_markdown_core::Inline::Text {
                    value: "native".into(),
                    marks: vec![],
                }]),
                provenance: native_provenance(None),
            })
            .collect(),
        ..Document::default()
    };
    let cancellation = CancellationToken::new();
    let reached = Arc::new(AtomicUsize::new(0));
    let hook_reached = Arc::clone(&reached);
    let hook_cancellation = cancellation.clone();
    set_traversal_test_hook(Some(Box::new(move |visited| {
        hook_reached.store(visited, Ordering::SeqCst);
        hook_cancellation.cancel();
    })));
    let _reset = ResetTraversalHook;
    let context = ExecutionContext::new(
        ExecutionOptions { cancellation, ..ExecutionOptions::default() },
        ResourceLimits::default(),
    );
    let error = merge_document(document, &[], &MergeConfig::default(), &context).unwrap_err();
    assert_eq!(reached.load(Ordering::SeqCst), 256);
    assert_eq!(error.code(), ErrorCode::Cancelled);
    assert_eq!(context.reserved_memory_bytes(), 0);
}

#[test]
fn every_large_text_stage_observes_in_flight_cancel_without_output() {
    for target in [
        text::TextStage::Associate,
        text::TextStage::NormalizePlan,
        text::TextStage::NormalizeCopy,
        text::TextStage::LineMaterialize,
    ] {
        let cancellation = CancellationToken::new();
        let reached = Arc::new(AtomicUsize::new(0));
        let hook_reached = Arc::clone(&reached);
        let hook_cancellation = cancellation.clone();
        text::set_test_hook(Some(Box::new(move |stage, bytes| {
            if stage == target {
                hook_reached.store(bytes, Ordering::SeqCst);
                hook_cancellation.cancel();
            }
        })));
        let _reset = ResetTextHook;
        let context = ExecutionContext::new(
            ExecutionOptions { cancellation, ..ExecutionOptions::default() },
            ResourceLimits::default(),
        );
        let detected = detection(&[(polygon(20.0, 20.0, 100.0, 16.0), 0.99)]);
        let large = "a".repeat(16 * 1024);
        let recognized = recognition(&[(0, &large, 0.99)]);
        let error = merge_document(
            Document::default(),
            &[input(&detected, &recognized)],
            &MergeConfig { policy: OcrPolicy::Always, ..MergeConfig::default() },
            &context,
        )
        .unwrap_err();
        assert!(reached.load(Ordering::SeqCst) >= 4 * 1024, "stage={target:?}");
        assert_eq!(error.code(), ErrorCode::Cancelled, "stage={target:?}");
        assert_eq!(context.reserved_memory_bytes(), 0, "stage={target:?}");
    }
}

#[test]
fn large_text_checkpoint_observes_timeout_after_work_begins() {
    let reached = Arc::new(AtomicUsize::new(0));
    let hook_reached = Arc::clone(&reached);
    text::set_test_hook(Some(Box::new(move |stage, bytes| {
        if stage == text::TextStage::Associate {
            hook_reached.store(bytes, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(30));
        }
    })));
    let _reset = ResetTextHook;
    let context = ExecutionContext::new(
        ExecutionOptions {
            timeout: Some(Duration::from_millis(10)),
            ..ExecutionOptions::default()
        },
        ResourceLimits::default(),
    );
    let detected = detection(&[(polygon(20.0, 20.0, 100.0, 16.0), 0.99)]);
    let large = "a".repeat(16 * 1024);
    let recognized = recognition(&[(0, &large, 0.99)]);
    let error = merge_document(
        Document::default(),
        &[input(&detected, &recognized)],
        &MergeConfig { policy: OcrPolicy::Always, ..MergeConfig::default() },
        &context,
    )
    .unwrap_err();
    assert!(reached.load(Ordering::SeqCst) >= 4 * 1024);
    assert_eq!(error.code(), ErrorCode::Timeout);
    assert_eq!(context.reserved_memory_bytes(), 0);
}

fn subthreshold_recognition_regions() -> (DetectionResult, RecognitionResult) {
    let chunk = "a".repeat(3_000);
    (
        detection(&[
            (polygon(20.0, 20.0, 100.0, 16.0), 0.99),
            (polygon(20.0, 80.0, 100.0, 16.0), 0.99),
        ]),
        RecognitionResult {
            regions: Arc::from(
                (0..2)
                    .map(|source_index| RecognizedText {
                        source_index,
                        text: chunk.clone(),
                        confidence: 0.99,
                    })
                    .collect::<Vec<_>>(),
            ),
            provider: Arc::from("test.recognizer"),
            language_hint: None,
            _memory_lease: None,
        },
    )
}

#[test]
fn subthreshold_regions_share_every_text_stage_cancellation_meter() {
    for target in [
        text::TextStage::Associate,
        text::TextStage::NormalizePlan,
        text::TextStage::NormalizeCopy,
        text::TextStage::LineMaterialize,
    ] {
        let (detected, recognized) = subthreshold_recognition_regions();
        let page = input(&detected, &recognized);
        let cancellation = CancellationToken::new();
        let reached = Arc::new(AtomicUsize::new(0));
        let hook_reached = Arc::clone(&reached);
        let hook_cancellation = cancellation.clone();
        text::set_test_hook(Some(Box::new(move |stage, bytes| {
            if stage == target {
                hook_reached.store(bytes, Ordering::SeqCst);
                hook_cancellation.cancel();
            }
        })));
        let _reset = ResetTextHook;
        let context = ExecutionContext::new(
            ExecutionOptions { cancellation, ..ExecutionOptions::default() },
            ResourceLimits::default(),
        );
        let error = merge_document(
            Document::default(),
            &[page],
            &MergeConfig { policy: OcrPolicy::Always, ..MergeConfig::default() },
            &context,
        )
        .unwrap_err();
        assert!(reached.load(Ordering::SeqCst) >= 4 * 1024, "stage={target:?}");
        assert_eq!(error.code(), ErrorCode::Cancelled, "stage={target:?}");
        assert_eq!(context.reserved_memory_bytes(), 0, "stage={target:?}");
    }
}

#[test]
fn subthreshold_regions_share_nfc_timeout_meter() {
    let (detected, recognized) = subthreshold_recognition_regions();
    let page = input(&detected, &recognized);
    let reached = Arc::new(AtomicUsize::new(0));
    let hook_reached = Arc::clone(&reached);
    text::set_test_hook(Some(Box::new(move |stage, bytes| {
        if stage == text::TextStage::NormalizePlan {
            hook_reached.store(bytes, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(30));
        }
    })));
    let _reset = ResetTextHook;
    let context = ExecutionContext::new(
        ExecutionOptions {
            timeout: Some(Duration::from_millis(10)),
            ..ExecutionOptions::default()
        },
        ResourceLimits::default(),
    );
    let error = merge_document(
        Document::default(),
        &[page],
        &MergeConfig { policy: OcrPolicy::Always, ..MergeConfig::default() },
        &context,
    )
    .unwrap_err();
    assert!(reached.load(Ordering::SeqCst) >= 4 * 1024);
    assert_eq!(error.code(), ErrorCode::Timeout);
    assert_eq!(context.reserved_memory_bytes(), 0);
}

fn fingerprint_result(context: &ExecutionContext) -> RecognitionResult {
    let chunk = "f".repeat(4_095);
    RecognitionResult {
        regions: Arc::from(
            (0..3_000)
                .map(|source_index| RecognizedText {
                    source_index,
                    text: chunk.clone(),
                    confidence: 0.99,
                })
                .collect::<Vec<_>>(),
        ),
        provider: Arc::from("test.recognizer"),
        language_hint: Some(Arc::from("en")),
        _memory_lease: Some(Arc::new(context.reserve_memory(1024).unwrap())),
    }
}

fn fingerprint_identity() -> crate::batch::BatchIdentity {
    crate::batch::BatchIdentity::new(
        1,
        600.0,
        800.0,
        crate::batch::DETECTOR_MODEL_ID,
        &detection(&[]),
    )
    .unwrap()
}

#[test]
fn three_thousand_subthreshold_regions_share_fingerprint_cancel_meter() {
    let cancellation = CancellationToken::new();
    let context = ExecutionContext::new(
        ExecutionOptions { cancellation: cancellation.clone(), ..ExecutionOptions::default() },
        ResourceLimits::default(),
    );
    let result = fingerprint_result(&context);
    let reached = Arc::new(AtomicUsize::new(0));
    let hook_reached = Arc::clone(&reached);
    crate::batch::set_fingerprint_test_hook(Some(Box::new(move |bytes| {
        hook_reached.store(bytes, Ordering::SeqCst);
        cancellation.cancel();
    })));
    let _reset = ResetFingerprintHook;
    let error = crate::BoundRecognition::new(result, fingerprint_identity(), &context).unwrap_err();
    assert!(reached.load(Ordering::SeqCst) >= 4 * 1024);
    assert_eq!(error.code(), ErrorCode::Cancelled);
    assert_eq!(context.reserved_memory_bytes(), 0);
}

#[test]
fn three_thousand_subthreshold_regions_share_fingerprint_timeout_meter() {
    // Preparing the large fixture is not part of the fingerprint operation
    // under test. Start the deadline only after that immutable input exists.
    let setup_context = context();
    let result = fingerprint_result(&setup_context);
    let context = ExecutionContext::new(
        ExecutionOptions {
            timeout: Some(Duration::from_millis(10)),
            ..ExecutionOptions::default()
        },
        ResourceLimits::default(),
    );
    let reached = Arc::new(AtomicUsize::new(0));
    let hook_reached = Arc::clone(&reached);
    crate::batch::set_fingerprint_test_hook(Some(Box::new(move |bytes| {
        hook_reached.store(bytes, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(30));
    })));
    let _reset = ResetFingerprintHook;
    let error = crate::BoundRecognition::new(result, fingerprint_identity(), &context).unwrap_err();
    assert!(reached.load(Ordering::SeqCst) >= 4 * 1024);
    assert_eq!(error.code(), ErrorCode::Timeout);
    assert_eq!(context.reserved_memory_bytes(), 0);
}

#[test]
fn three_thousand_subthreshold_regions_checkpoint_through_the_whole_fingerprint() {
    let context = context();
    let result = fingerprint_result(&context);
    let checkpoints = Arc::new(AtomicUsize::new(0));
    let last = Arc::new(AtomicUsize::new(0));
    let hook_checkpoints = Arc::clone(&checkpoints);
    let hook_last = Arc::clone(&last);
    crate::batch::set_fingerprint_test_hook(Some(Box::new(move |bytes| {
        hook_checkpoints.fetch_add(1, Ordering::SeqCst);
        hook_last.store(bytes, Ordering::SeqCst);
    })));
    let _reset = ResetFingerprintHook;
    let bound = crate::BoundRecognition::new(result, fingerprint_identity(), &context).unwrap();
    assert!(checkpoints.load(Ordering::SeqCst) >= 3_000);
    assert!(last.load(Ordering::SeqCst) >= 3_000 * 4_095);
    drop(bound);
    assert_eq!(context.reserved_memory_bytes(), 0);
}
