use super::support::{NS, convert, package};
use into_markdown_core::{ConversionError, InputFormat, ResourceLimits};

#[test]
fn closed_xml_profile_rejects_empty_active_nodes_attributes_and_wrong_hierarchy() {
    let object = format!(
        "<office:document-content {NS}><office:body><office:text><draw:object/></office:text></office:body></office:document-content>"
    );
    let object = package(InputFormat::Odt, &object, &[]);
    assert!(matches!(
        convert(&object, InputFormat::Odt, ResourceLimits::default()),
        Err(ConversionError::Malformed { .. })
    ));

    let attribute = format!(
        "<office:document-content {NS}><office:body><office:text><text:p draw:foo=''>x</text:p></office:text></office:body></office:document-content>"
    );
    let attribute = package(InputFormat::Odt, &attribute, &[]);
    assert!(matches!(
        convert(&attribute, InputFormat::Odt, ResourceLimits::default()),
        Err(ConversionError::Malformed { .. })
    ));

    let misplaced_table_attribute = format!(
        "<office:document-content {NS}><office:body><office:spreadsheet><table:table><table:table-row><table:table-cell table:name='not-a-table'/></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"
    );
    let misplaced_table_attribute = package(InputFormat::Ods, &misplaced_table_attribute, &[]);
    assert!(matches!(
        convert(&misplaced_table_attribute, InputFormat::Ods, ResourceLimits::default()),
        Err(ConversionError::Malformed { .. })
    ));

    let content = format!(
        "<office:document-content {NS}><office:body><office:text><text:p>x</text:p></office:text></office:body></office:document-content>"
    );
    let styles = format!(
        "<office:document-styles {NS}><office:styles><office:event-listeners/></office:styles></office:document-styles>"
    );
    let styles =
        package(InputFormat::Odt, &content, &[("styles.xml", "text/xml", styles.as_bytes())]);
    assert!(matches!(
        convert(&styles, InputFormat::Odt, ResourceLimits::default()),
        Err(ConversionError::Malformed { part: Some(part), .. }) if part == "styles.xml"
    ));

    let meta = format!(
        "<office:document-meta {NS}><office:meta><office:scripts/></office:meta></office:document-meta>"
    );
    let meta = package(InputFormat::Odt, &content, &[("meta.xml", "text/xml", meta.as_bytes())]);
    assert!(matches!(
        convert(&meta, InputFormat::Odt, ResourceLimits::default()),
        Err(ConversionError::Malformed { part: Some(part), .. }) if part == "meta.xml"
    ));

    let settings = format!(
        "<office:document-settings {NS}><office:settings><office:event-listeners/></office:settings></office:document-settings>"
    );
    let settings =
        package(InputFormat::Odt, &content, &[("settings.xml", "text/xml", settings.as_bytes())]);
    assert!(matches!(
        convert(&settings, InputFormat::Odt, ResourceLimits::default()),
        Err(ConversionError::Malformed { part: Some(part), .. }) if part == "settings.xml"
    ));

    let wrong_parent = format!(
        "<office:document-content {NS}><office:body><office:text><table:table-cell/></office:text></office:body></office:document-content>"
    );
    let wrong_parent = package(InputFormat::Odt, &wrong_parent, &[]);
    assert!(matches!(
        convert(&wrong_parent, InputFormat::Odt, ResourceLimits::default()),
        Err(ConversionError::Malformed { .. })
    ));

    let nested_body = format!(
        "<office:document-content {NS}><office:body><office:text><text:p><office:body/></text:p></office:text></office:body></office:document-content>"
    );
    let nested_body = package(InputFormat::Odt, &nested_body, &[]);
    assert!(matches!(
        convert(&nested_body, InputFormat::Odt, ResourceLimits::default()),
        Err(ConversionError::Malformed { .. })
    ));
}
