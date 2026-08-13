use super::*;
use into_markdown_core::{CancellationToken, ErrorCode, ListItem, ListKind, NodeId, OcrPolicy};
use std::sync::atomic::{AtomicUsize, Ordering};

struct ResetTraversalHook;

impl Drop for ResetTraversalHook {
    fn drop(&mut self) {
        set_traversal_test_hook(None);
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
