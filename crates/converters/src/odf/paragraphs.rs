use super::model::DRAW_NS;
use super::xml::{XmlContent, XmlNode};

pub(super) enum ParagraphPart<'a> {
    Text(XmlNode),
    Drawing(&'a XmlNode),
}

// The IR has block images, not inline images. Split only at drawing anchors, preserving
// text wrappers/marks and source order on either side (including anchors inside spans).
pub(super) fn split_drawings(node: &XmlNode) -> Vec<ParagraphPart<'_>> {
    if node.name.ns == DRAW_NS && matches!(node.name.local.as_str(), "frame" | "a") {
        return vec![ParagraphPart::Drawing(node)];
    }
    let empty = || XmlNode { name: node.name.clone(), attrs: node.attrs.clone(), content: vec![] };
    let mut text = empty();
    let mut parts = Vec::new();
    for value in &node.content {
        match value {
            XmlContent::Text(value) => text.content.push(XmlContent::Text(value.clone())),
            XmlContent::Node(child) => {
                for part in split_drawings(child) {
                    match part {
                        ParagraphPart::Text(child) => text.content.push(XmlContent::Node(child)),
                        ParagraphPart::Drawing(drawing) => {
                            if !text.content.is_empty() {
                                parts.push(ParagraphPart::Text(std::mem::replace(
                                    &mut text,
                                    empty(),
                                )));
                            }
                            parts.push(ParagraphPart::Drawing(drawing));
                        }
                    }
                }
            }
        }
    }
    if !text.content.is_empty() || parts.is_empty() {
        parts.push(ParagraphPart::Text(text));
    }
    parts
}
