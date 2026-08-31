use super::*;

#[test]
fn link_geometry_classification_preserves_other_object_validation() {
    let info = PdfRect { left: 0.0, bottom: 0.0, right: 100.0, top: 200.0 };
    for raw in [[10.0, 20.0, 40.0, 60.0], [40.0, 60.0, 10.0, 20.0], [10.0, 60.0, 40.0, 20.0]] {
        assert_eq!(
            link_bounds(raw, info).unwrap(),
            PdfRect { left: 10.0, bottom: 20.0, right: 40.0, top: 60.0 }
        );
    }
    assert!(finite_rect("image_bounds", 40.0, 60.0, 10.0, 20.0).is_err());
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(link_bounds([value, 20.0, 40.0, 60.0], info), Err(LinkIssueReason::NonFinite));
    }
    for (raw, reason) in [
        ([f64::MAX, 20.0, 40.0, 60.0], LinkIssueReason::Unrepresentable),
        ([10.0, 20.0, 10.0, 60.0], LinkIssueReason::Empty),
        ([110.0, 20.0, 140.0, 60.0], LinkIssueReason::OutsidePage),
        ([10.0, 20.0, 10.0 + f64::EPSILON * 8.0, 60.0], LinkIssueReason::Unrepresentable),
    ] {
        assert_eq!(link_bounds(raw, info), Err(reason));
    }
    assert_eq!(link_bounds([-5.0, -10.0, 150.0, 240.0], info).unwrap(), info);
    let crop = PdfRect { left: 100.0, bottom: 200.0, right: 200.0, top: 400.0 };
    assert_eq!(
        link_bounds([90.0, 190.0, 150.0, 350.0], crop).unwrap(),
        PdfRect { left: 0.0, bottom: 0.0, right: 50.0, top: 150.0 }
    );
}

#[test]
fn unsupported_targets_consume_scan_budget_without_output_slots() {
    let limits = Limits { max_links_per_page: 1, ..Limits::default() };
    let mut scan = Scan { plan: LinkAllocationPlan::default(), fingerprint: Sha256::new() };
    scan.consume(1, limits).unwrap();
    scan.observe(
        LinkIdentity::Annotation { index: 0 },
        Ok(PdfRect::default()),
        None,
        limits,
        &mut |_| panic!("unsupported target emitted an item"),
    )
    .unwrap();
    assert_eq!(scan.plan.count, 0);
    assert!(matches!(
        scan.consume(1, limits),
        Err(Error::ResourceLimit { limit: "max_links_per_page", .. })
    ));
}

unsafe extern "C" fn failed_rect(_: Handle, _: *mut FsRectF) -> c_int {
    0
}
unsafe extern "C" fn nonfinite_rect(_: Handle, rect: *mut FsRectF) -> c_int {
    unsafe {
        *rect = FsRectF { left: f32::NAN, top: 20.0, right: 30.0, bottom: 10.0 };
    }
    1
}
unsafe extern "C" fn shifted_rect(_: Handle, rect: *mut FsRectF) -> c_int {
    unsafe {
        *rect = FsRectF { left: 20.0, top: 60.0, right: 70.0, bottom: 30.0 };
    }
    1
}
unsafe extern "C" fn failed_web_rect(
    _: Handle,
    _: c_int,
    _: c_int,
    _: *mut f64,
    _: *mut f64,
    _: *mut f64,
    _: *mut f64,
) -> c_int {
    0
}
unsafe extern "C" fn zero_rects(_: Handle, _: c_int) -> c_int {
    0
}
unsafe extern "C" fn no_progress(_: Handle, _: *mut c_int, _: *mut Handle) -> c_int {
    1
}
unsafe extern "C" fn null_handle(_: Handle, position: *mut c_int, link: *mut Handle) -> c_int {
    unsafe {
        *position += 1;
        *link = std::ptr::null_mut();
    }
    1
}
unsafe extern "C" fn long_uri(_: Handle, _: Handle, _: *mut c_void, _: c_ulong) -> c_ulong {
    1_000_001
}

#[test]
#[ignore = "requires PDFIUM_LIBRARY pointing to the pinned current-target runtime"]
fn native_issue334_link_faults_and_plan_changes_fail_safely() {
    let path = std::env::var_os("PDFIUM_LIBRARY").unwrap();
    let mut native = Native::load(Path::new(&path)).unwrap();
    let bytes = crate::tests::minimal_pdf();
    let document = native.load_document(&bytes, None).unwrap();
    let page = native.load_page(document, 0).unwrap();
    let text = native.load_text_page(page).unwrap();
    let request = LinkRequest { document, page, text, limits: Limits::default() };
    let original_rect = native.link_rect;
    let plan = native.plan_link_scan(request, LinkPolicy::BestEffort, &mut || true).unwrap();
    assert_eq!(plan.count, 2);
    let valid = native.extract_links(request, plan, &mut || true).unwrap();
    assert_eq!(valid.links.len(), 2);
    assert!(valid.diagnostics.is_empty());
    for (fault, reason) in [
        (failed_rect as GetAnnotRect, LinkIssueReason::ReadFailed),
        (nonfinite_rect as GetAnnotRect, LinkIssueReason::NonFinite),
    ] {
        native.link_rect = fault;
        assert!(native.extract_links(request, plan, &mut || true).is_err());
        assert!(
            matches!(native.plan_link_scan(request, LinkPolicy::Strict, &mut || true), Err(Error::Link { identity: LinkIdentity::Annotation { index: 0 }, reason: r }) if r == reason)
        );
        let degraded =
            native.plan_link_scan(request, LinkPolicy::BestEffort, &mut || true).unwrap();
        let result = native.extract_links(request, degraded, &mut || true).unwrap();
        assert_eq!(result.links.len(), 1);
        assert_eq!(
            result.diagnostics,
            [LinkDiagnostic { identity: LinkIdentity::Annotation { index: 0 }, reason }]
        );
        native.link_rect = original_rect;
        assert!(native.extract_links(request, degraded, &mut || true).is_err());
    }
    native.link_rect = shifted_rect;
    assert!(native.extract_links(request, plan, &mut || true).is_err());
    native.link_rect = original_rect;
    check_web_faults(&mut native, request);
    check_scan_guards(&mut native, request);
    native.close_text_page(text);
    native.close_page(page);
    native.close_document(document);
}

fn check_web_faults(native: &mut Native, request: LinkRequest) {
    let original = native.web_link_rect;
    native.web_link_rect = failed_web_rect;
    let plan = native.plan_link_scan(request, LinkPolicy::BestEffort, &mut || true).unwrap();
    let result = native.extract_links(request, plan, &mut || true).unwrap();
    assert_eq!(result.links.len(), 1);
    assert_eq!(
        result.diagnostics,
        [LinkDiagnostic {
            identity: LinkIdentity::Web { index: 0, rectangle: 0 },
            reason: LinkIssueReason::ReadFailed
        }]
    );
    assert!(matches!(
        native.plan_link_scan(request, LinkPolicy::Strict, &mut || true),
        Err(Error::Link { .. })
    ));
    native.web_link_rect = original;
    let original_count = native.web_link_rect_count;
    native.web_link_rect_count = zero_rects;
    let plan = native.plan_link_scan(request, LinkPolicy::BestEffort, &mut || true).unwrap();
    let result = native.extract_links(request, plan, &mut || true).unwrap();
    assert_eq!(result.diagnostics[0].reason, LinkIssueReason::MissingRectangle);
    native.web_link_rect_count = original_count;
}

fn check_scan_guards(native: &mut Native, request: LinkRequest) {
    let plan = native.plan_link_scan(request, LinkPolicy::BestEffort, &mut || true).unwrap();
    assert!(native.plan_link_scan(request, LinkPolicy::BestEffort, &mut || false).is_err());
    assert!(native.extract_links(request, plan, &mut || false).is_err());
    let mut checkpoints = 0;
    assert!(
        native
            .extract_links(request, plan, &mut || {
                checkpoints += 1;
                checkpoints < 5
            })
            .is_err()
    );
    assert_eq!(checkpoints, 5);
    let small =
        LinkRequest { limits: Limits { max_links_per_page: 1, ..request.limits }, ..request };
    assert!(matches!(
        native.plan_link_scan(small, LinkPolicy::BestEffort, &mut || true),
        Err(Error::ResourceLimit { limit: "max_links_per_page", .. })
    ));
    let original = native.enumerate_link;
    for invalid_enumerator in [no_progress as EnumerateLink, null_handle as EnumerateLink] {
        native.enumerate_link = invalid_enumerator;
        assert!(matches!(
            native.plan_link_scan(request, LinkPolicy::BestEffort, &mut || true),
            Err(Error::InvalidResult { operation: "enumerate_link", .. })
        ));
    }
    native.enumerate_link = original;
    native.link_rect = failed_rect;
    native.action_uri = long_uri;
    assert!(matches!(
        native.plan_link_scan(request, LinkPolicy::BestEffort, &mut || true),
        Err(Error::ResourceLimit { limit: "max_link_bytes", .. })
    ));
}
