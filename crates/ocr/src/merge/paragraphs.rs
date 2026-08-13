use super::geometry::{paragraph_compatible, union_rect};
use super::lines::MergedLine;
use into_markdown_core::{ConversionError, Rect};

pub(crate) struct MergedParagraph {
    pub(crate) lines: Vec<MergedLine>,
    pub(crate) bounds: Rect,
}

pub(crate) fn merge_paragraphs(
    lines: Vec<MergedLine>,
) -> Result<Vec<MergedParagraph>, ConversionError> {
    let mut paragraphs = Vec::<MergedParagraph>::new();
    paragraphs.try_reserve_exact(lines.len()).map_err(|_| super::memory())?;
    for line in lines {
        let compatible = paragraphs
            .last()
            .and_then(|paragraph| paragraph.lines.last())
            .is_some_and(|previous| {
                angle_distance(previous.angle_degrees, line.angle_degrees) <= 15.0
                    && paragraph_compatible(
                        &previous.bounds,
                        &line.bounds,
                        previous.line_height.max(line.line_height),
                    )
            });
        if compatible && let Some(paragraph) = paragraphs.last_mut() {
            paragraph.bounds = union_rect(paragraph.bounds, line.bounds);
            paragraph.lines.try_reserve(1).map_err(|_| super::memory())?;
            paragraph.lines.push(line);
        } else {
            let mut paragraph_lines = Vec::new();
            paragraph_lines.try_reserve_exact(1).map_err(|_| super::memory())?;
            paragraph_lines.push(line);
            paragraphs.push(MergedParagraph {
                bounds: paragraph_lines[0].bounds,
                lines: paragraph_lines,
            });
        }
    }
    Ok(paragraphs)
}

fn angle_distance(left: f32, right: f32) -> f32 {
    let difference = (left - right).abs().rem_euclid(180.0);
    difference.min(180.0 - difference)
}
