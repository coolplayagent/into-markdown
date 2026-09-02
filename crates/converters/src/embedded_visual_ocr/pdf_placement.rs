//! PDF layout needs page coordinates, unlike other containers' local OCR evidence.

use super::{VisualRef, geometry};
use crate::pdf::working_visual::VisualRole;
use into_markdown_core::{
    AssetId, ConversionError, ConversionOptions, Diagnostic, DiagnosticSeverity, ErrorPolicy,
    ExecutionContext, OcrPolicy,
};
use std::collections::BTreeSet;

pub(super) fn select_page_sources(
    references: &mut Vec<VisualRef>,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let mut rendered_pages = BTreeSet::new();
    for reference in references.iter() {
        context.checkpoint()?;
        if reference.role == VisualRole::OcrPageRender {
            let page =
                reference.provenance.locator.page.ok_or_else(|| ConversionError::Internal {
                    detail: "PDF OCR page render has no page locator".into(),
                })?;
            rendered_pages.insert(page);
        }
    }
    references.retain(|reference| {
        reference.role == VisualRole::OcrPageRender
            || reference.provenance.locator.page.is_none_or(|page| !rendered_pages.contains(&page))
    });
    Ok(())
}

pub(super) fn filter_references(
    references: &mut Vec<VisualRef>,
    eligible_assets: &BTreeSet<AssetId>,
    diagnostics: &mut Vec<Diagnostic>,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    // Filter placements, not assets: the same image may also have a usable
    // placement on this page. Keep its recognition cached for that placement.
    for reference in references.iter() {
        context.checkpoint()?;
        if !eligible_assets.contains(&reference.asset)
            || geometry::source_coordinate_frame(&reference.provenance.locator).is_some()
        {
            continue;
        }
        let detail = "PDF image OCR omitted: its image-to-page coordinate transform is unavailable";
        if options.error_policy != ErrorPolicy::BestEffort
            || super::effective_ocr_policy(options) != OcrPolicy::Auto
        {
            return Err(ConversionError::Unsupported { detail: detail.into() });
        }
        diagnostics.try_reserve(1).map_err(|_| {
            super::resource("max_memory_bytes", "PDF OCR placement diagnostic allocation failed")
        })?;
        diagnostics.push(Diagnostic {
            code: "pdf.optionalOcrSkipped".into(),
            severity: DiagnosticSeverity::Warning,
            message: detail.into(),
            locator: Some(geometry::remapped_locator(&reference.provenance.locator)),
        });
    }
    references.retain(|reference| {
        !eligible_assets.contains(&reference.asset)
            || geometry::source_coordinate_frame(&reference.provenance.locator).is_some()
    });
    Ok(())
}
