use super::path::{BasePath, Reference};

#[test]
fn container_path_resolution_normalizes_percent_and_confines_parent_segments() {
    let base = BasePath::document("OPS/text/chapter.xhtml").unwrap();
    assert_eq!(
        base.resolve("../images/cover%20art.png#view").unwrap(),
        Reference::Internal {
            path: "OPS/images/cover art.png".into(),
            fragment: Some("view".into()),
        }
    );
    assert!(base.resolve("../../../escape.xhtml").is_err());
    assert!(base.resolve("..%2fescape.xhtml").is_err());
    assert!(base.resolve("chapter.xhtml?active=true").is_err());
}

#[test]
fn xml_base_is_inherited_without_broadening_container_authority() {
    let document = BasePath::document("OPS/package.opf").unwrap();
    let section = document.apply("Text/").unwrap();
    let nested = section.apply("Nested/base.xhtml").unwrap();
    assert_eq!(
        nested.resolve("../chapter.xhtml").unwrap(),
        Reference::Internal { path: "OPS/Text/chapter.xhtml".into(), fragment: None }
    );
    assert!(document.apply("https://example.invalid/base/").is_err());
}

#[test]
fn xml_base_dot_segments_preserve_directory_semantics() {
    let document = BasePath::document("OPS/content.opf").unwrap();
    for base in [".", "./", "nested/..", "nested/../"] {
        assert_eq!(
            document.apply(base).unwrap().resolve("chapter.xhtml").unwrap(),
            Reference::Internal { path: "OPS/chapter.xhtml".into(), fragment: None },
            "base {base:?}"
        );
    }
    assert_eq!(
        document.apply("").unwrap().resolve("chapter.xhtml").unwrap(),
        Reference::Internal { path: "OPS/chapter.xhtml".into(), fragment: None }
    );
    assert!(document.apply("%2e").is_err());
    assert!(document.apply("nested%2fescape/").is_err());
    assert!(document.apply("nested%5cescape/").is_err());
}

#[test]
fn fragments_decode_once_normalize_unicode_and_reject_invalid_scalars() {
    let base = BasePath::document("OPS/text/one.xhtml").unwrap();
    assert_eq!(
        base.resolve("#caf%C3%A9").unwrap(),
        Reference::Internal { path: "OPS/text/one.xhtml".into(), fragment: Some("café".into()) }
    );
    assert_eq!(base.resolve("#cafe%CC%81").unwrap(), base.resolve("#caf%C3%A9").unwrap());
    assert_eq!(base.resolve("#%2525").unwrap().canonical_target(), "OPS/text/one.xhtml#%2525");
    for invalid in ["#%", "#%GG", "#%C3", "#%00", "#%0A"] {
        assert!(base.resolve(invalid).is_err(), "accepted {invalid}");
    }
}

#[test]
fn external_links_remain_data_and_active_schemes_are_rejected() {
    let base = BasePath::document("OPS/chapter.xhtml").unwrap();
    assert_eq!(
        base.resolve("https://example.invalid/reference").unwrap(),
        Reference::External("https://example.invalid/reference".into())
    );
    assert!(base.resolve("javascript:alert(1)").is_err());
    assert!(base.resolve("file:///etc/passwd").is_err());
}

#[test]
fn unicode_logical_paths_resolve_exactly_without_renaming() {
    for name in ["报告（最终）.xhtml", "café.xhtml", "e\u{301}.xhtml", "straße.xhtml", "😀.xhtml"]
    {
        let base = BasePath::document("OPS/package.opf").unwrap();
        assert_eq!(
            base.resolve(name).unwrap(),
            Reference::Internal { path: format!("OPS/{name}"), fragment: None }
        );
        BasePath::document(&format!("OPS/{name}")).unwrap();
    }
}
