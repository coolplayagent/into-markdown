use super::*;
use crate::epub::budget::{EpubBudget, set_navigation_url_test_hook};
use into_markdown_core::{
    CancellationToken, ConversionOptions, ErrorCode, ExecutionContext, ExecutionOptions,
    ResourceLimits,
};
use std::time::Duration;

struct ResetHook;

impl Drop for ResetHook {
    fn drop(&mut self) {
        set_navigation_url_test_hook(None);
    }
}

fn anchor() -> Name {
    Name { namespace: Some(XHTML_NS.to_vec()), local: b"a".to_vec() }
}

fn attribute(local: &[u8], value: impl Into<String>) -> Attribute {
    Attribute { namespace: None, local: local.to_vec(), value: value.into() }
}

fn base() -> BasePath {
    BasePath::document("OPS/nav.xhtml").unwrap()
}

#[test]
fn rdfa_iri_attributes_are_confined_and_unknown_url_names_fail_closed() {
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let mut budget = EpubBudget::new(&options, &context);
    for (name, value) in [
        (b"about".as_slice(), "https://example.invalid/about"),
        (b"resource".as_slice(), "javascript:bad"),
        (b"vocab".as_slice(), "https://example.invalid/vocab#"),
        (b"prefix".as_slice(), "schema: https://schema.org/"),
        (b"futurehref".as_slice(), "https://example.invalid/future"),
    ] {
        assert!(
            validate_element(&anchor(), &[attribute(name, value)], &base(), &mut budget).is_err(),
            "accepted {name:?}"
        );
    }
    validate_element(
        &anchor(),
        &[
            attribute(b"about", "text/one.xhtml#entry"),
            attribute(b"resource", "images/cover.png"),
            attribute(b"property", "title"),
        ],
        &base(),
        &mut budget,
    )
    .unwrap();
    assert_eq!(context.reserved_memory_bytes(), 0);
}

#[test]
fn mathml_altimg_is_a_container_confined_iri() {
    let math = Name { namespace: Some(MATHML_NS.to_vec()), local: b"math".to_vec() };
    for value in ["https://example.invalid/math.png", "javascript:bad"] {
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let mut budget = EpubBudget::new(&options, &context);
        assert!(
            validate_element(
                &math,
                &[attribute(b"alt", "equation"), attribute(b"altimg", value)],
                &base(),
                &mut budget,
            )
            .is_err(),
            "accepted {value:?}"
        );
        assert_eq!(context.reserved_memory_bytes(), 0);
    }

    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let mut budget = EpubBudget::new(&options, &context);
    validate_element(
        &math,
        &[attribute(b"alt", "equation"), attribute(b"altimg", "images/equation.png")],
        &base(),
        &mut budget,
    )
    .unwrap();
    assert_eq!(context.reserved_memory_bytes(), 0);
}

#[test]
fn navigation_url_bytes_checkpoint_across_attributes_and_release_scratch() {
    for timeout in [false, true] {
        let cancellation = CancellationToken::new();
        let hook_cancellation = cancellation.clone();
        set_navigation_url_test_hook(Some(Box::new(move |_| {
            if timeout {
                std::thread::sleep(Duration::from_millis(30));
            } else {
                hook_cancellation.cancel();
            }
        })));
        let _reset = ResetHook;
        let execution = if timeout {
            ExecutionOptions {
                timeout: Some(Duration::from_millis(10)),
                ..ExecutionOptions::default()
            }
        } else {
            ExecutionOptions { cancellation, ..ExecutionOptions::default() }
        };
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(execution, options.limits.clone());
        let mut budget = EpubBudget::new(&options, &context);
        let attributes =
            [attribute(b"title", "a".repeat(3_000)), attribute(b"lang", "b".repeat(3_000))];
        let error = validate_element(&anchor(), &attributes, &base(), &mut budget).unwrap_err();
        assert_eq!(error.code(), if timeout { ErrorCode::Timeout } else { ErrorCode::Cancelled });
        assert_eq!(context.reserved_memory_bytes(), 0);
    }
}

#[test]
fn navigation_url_resource_limits_fail_before_parse_without_a_lease() {
    let mut options = ConversionOptions::default();
    options.limits.max_field_bytes = 32;
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let mut budget = EpubBudget::new(&options, &context);
    let error =
        validate_element(&anchor(), &[attribute(b"href", "x".repeat(33))], &base(), &mut budget)
            .unwrap_err();
    assert_eq!(error.code(), ErrorCode::ResourceLimit);
    assert_eq!(context.reserved_memory_bytes(), 0);

    let limits = ResourceLimits { max_archive_entries: 1, ..ResourceLimits::default() };
    let options = ConversionOptions { limits: limits.clone(), ..ConversionOptions::default() };
    let context = ExecutionContext::new(ExecutionOptions::default(), limits);
    let mut budget = EpubBudget::new(&options, &context);
    let error = validate_element(
        &anchor(),
        &[attribute(b"ping", "one.xhtml two.xhtml")],
        &base(),
        &mut budget,
    )
    .unwrap_err();
    assert_eq!(error.code(), ErrorCode::ResourceLimit);
    assert_eq!(context.reserved_memory_bytes(), 0);
}
