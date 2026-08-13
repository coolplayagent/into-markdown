use super::body::{BodyKind, SelectedBody};
use super::properties::Properties;
use super::recipients::{Mailbox, Recipient, RecipientKind};
use into_markdown_core::{
    Asset, Block, BlockNode, ConverterOutput, Document, DocumentMetadata, Inline, NodeId,
    Provenance, ProvenanceKind, SourceLocator,
};
use std::collections::BTreeMap;

const PR_SUBJECT: u16 = 0x0037;
const PR_CLIENT_SUBMIT_TIME: u16 = 0x0039;
const PR_TRANSPORT_HEADERS: u16 = 0x007d;
const PR_MESSAGE_DELIVERY_TIME: u16 = 0x0e06;

/// Owned attachment shape used after recursive storage borrows are consumed.
pub(super) struct AttachmentOutput {
    pub(super) asset: Option<Asset>,
    pub(super) content_id: Option<String>,
    pub(super) filename: String,
    pub(super) source: String,
    pub(super) nested: Option<ConverterOutput>,
}

#[allow(clippy::too_many_lines)] // Publication is transactional across all message sections.
pub(super) fn assemble(
    properties: &Properties,
    sender: Option<&Mailbox>,
    recipients: &[Recipient],
    mut body: SelectedBody,
    attachments: Vec<AttachmentOutput>,
    prefix: &str,
) -> ConverterOutput {
    prefix_output(&mut body.output, prefix);
    let subject = properties.text(PR_SUBJECT).map(str::to_owned);
    let mut metadata = metadata(properties, subject.clone(), sender, recipients, body.kind);
    let mut blocks = Vec::new();
    let mut diagnostics = body.output.diagnostics;
    let mut assets = body.output.assets;
    let mut next_id = 1_usize;

    if let Some(subject) = subject.as_deref().filter(|value| !value.trim().is_empty()) {
        blocks.push(node(
            prefix,
            &mut next_id,
            Block::Heading { level: 1, content: text(subject) },
            properties.source(PR_SUBJECT).unwrap_or("msg/__properties_version1.0"),
        ));
    }
    if let Some(sender) = sender {
        blocks.push(header_node(prefix, &mut next_id, "From", &sender.formatted(), &sender.source));
    }
    for kind in [RecipientKind::To, RecipientKind::Cc, RecipientKind::Bcc] {
        let selected =
            recipients.iter().filter(|recipient| recipient.kind == kind).collect::<Vec<_>>();
        if !selected.is_empty() {
            let value = selected
                .iter()
                .map(|recipient| recipient.mailbox.formatted())
                .collect::<Vec<_>>()
                .join(", ");
            blocks.push(header_node(
                prefix,
                &mut next_id,
                kind.label(),
                &value,
                &selected[0].mailbox.source,
            ));
        }
    }
    if let Some(time) =
        properties.time(PR_CLIENT_SUBMIT_TIME).or_else(|| properties.time(PR_MESSAGE_DELIVERY_TIME))
    {
        let source = properties
            .source(PR_CLIENT_SUBMIT_TIME)
            .or_else(|| properties.source(PR_MESSAGE_DELIVERY_TIME))
            .unwrap_or("msg/__properties_version1.0");
        blocks.push(header_node(prefix, &mut next_id, "Date", &format_filetime(time), source));
    }
    if !blocks.is_empty() {
        blocks.push(node(prefix, &mut next_id, Block::Rule, "msg/__properties_version1.0"));
    }
    blocks.append(&mut body.output.document.blocks);

    let mut attachment_heading = false;
    for (ordinal, mut attachment) in attachments.into_iter().enumerate() {
        let attachment_prefix = format!("{prefix}-attachment-{}", ordinal + 1);
        if let Some(asset) = &mut attachment.asset {
            prefix_asset(asset, &attachment_prefix);
        }
        if attachment.content_id.is_some() {
            if let Some(asset) = attachment.asset.as_ref() {
                blocks.push(node(
                    prefix,
                    &mut next_id,
                    Block::Image {
                        asset: asset.id.clone(),
                        alt: Some(attachment.filename.clone()),
                    },
                    &attachment.source,
                ));
            }
        } else {
            if !attachment_heading {
                blocks.push(node(
                    prefix,
                    &mut next_id,
                    Block::Heading { level: 2, content: text("Attachments") },
                    &attachment.source,
                ));
                attachment_heading = true;
            }
            blocks.push(node(
                prefix,
                &mut next_id,
                Block::Paragraph(text(&attachment.filename)),
                &attachment.source,
            ));
            if let Some(asset) =
                attachment.asset.as_ref().filter(|asset| asset.media_type.starts_with("image/"))
            {
                blocks.push(node(
                    prefix,
                    &mut next_id,
                    Block::Image {
                        asset: asset.id.clone(),
                        alt: Some(attachment.filename.clone()),
                    },
                    &attachment.source,
                ));
            }
        }
        if let Some(asset) = attachment.asset {
            metadata
                .properties
                .insert(format!("msg.attachment.{}.asset_id", ordinal + 1), asset.id.0.clone());
            metadata.properties.insert(
                format!("msg.attachment.{}.source", ordinal + 1),
                attachment.source.clone(),
            );
            if let Some(cid) = attachment.content_id {
                metadata
                    .properties
                    .insert(format!("msg.attachment.{}.content_id", ordinal + 1), cid);
            }
            assets.push(asset);
        }
        if let Some(mut nested) = attachment.nested {
            prefix_output(&mut nested, &attachment_prefix);
            blocks.push(node(
                prefix,
                &mut next_id,
                Block::Heading {
                    level: 3,
                    content: text(&format!("Attached message: {}", attachment.filename)),
                },
                &attachment.source,
            ));
            blocks.extend(nested.document.blocks);
            assets.extend(nested.assets);
            diagnostics.extend(nested.diagnostics);
        }
    }
    if let Some(headers) = properties.text(PR_TRANSPORT_HEADERS).filter(|value| !value.is_empty()) {
        let source =
            properties.source(PR_TRANSPORT_HEADERS).unwrap_or("msg/__properties_version1.0");
        blocks.push(node(
            prefix,
            &mut next_id,
            Block::Heading { level: 2, content: text("Transport headers") },
            source,
        ));
        blocks.push(node(
            prefix,
            &mut next_id,
            Block::Code { language: Some("rfc822".into()), text: headers.to_owned() },
            source,
        ));
    }
    let document = Document { metadata, blocks, ..Document::default() };
    ConverterOutput::new(document, assets, diagnostics)
}

fn metadata(
    properties: &Properties,
    subject: Option<String>,
    sender: Option<&Mailbox>,
    recipients: &[Recipient],
    body: BodyKind,
) -> DocumentMetadata {
    let mut values = BTreeMap::new();
    values.insert(
        "msg.body_kind".into(),
        match body {
            BodyKind::Html => "html",
            BodyKind::Rtf => "rtf",
            BodyKind::Plain => "plain",
            BodyKind::Empty => "empty",
        }
        .into(),
    );
    values.insert("msg.codepage".into(), properties.codepage().to_string());
    if let Some(sender) = sender {
        values.insert("msg.sender".into(), sender.formatted());
    }
    for kind in [RecipientKind::To, RecipientKind::Cc, RecipientKind::Bcc] {
        let joined = recipients
            .iter()
            .filter(|recipient| recipient.kind == kind)
            .map(|recipient| recipient.mailbox.formatted())
            .collect::<Vec<_>>()
            .join(", ");
        if !joined.is_empty() {
            values.insert(format!("msg.{}", kind.label().to_ascii_lowercase()), joined);
        }
    }
    if let Some(headers) = properties.text(PR_TRANSPORT_HEADERS) {
        values.insert("msg.transport_headers".into(), headers.to_owned());
    }
    let time = properties
        .time(PR_CLIENT_SUBMIT_TIME)
        .or_else(|| properties.time(PR_MESSAGE_DELIVERY_TIME));
    if let Some(time) = time {
        values.insert("msg.time".into(), format_filetime(time));
    }
    DocumentMetadata {
        title: subject,
        authors: sender.map(|mailbox| vec![mailbox.formatted()]).unwrap_or_default(),
        properties: values,
    }
}

fn header_node(
    prefix: &str,
    next: &mut usize,
    label: &str,
    value: &str,
    source: &str,
) -> BlockNode {
    node(
        prefix,
        next,
        Block::Paragraph(vec![
            Inline::Text {
                value: format!("{label}: "),
                marks: vec![into_markdown_core::InlineMark::Bold],
            },
            Inline::Text { value: value.to_owned(), marks: Vec::new() },
        ]),
        source,
    )
}

fn node(prefix: &str, next: &mut usize, block: Block, source: &str) -> BlockNode {
    let id = NodeId(format!("{prefix}-node-{next}"));
    *next += 1;
    BlockNode {
        id,
        block,
        provenance: Provenance {
            kind: ProvenanceKind::NativeParser,
            provider: "builtin.converter.msg".into(),
            locator: SourceLocator { part: Some(source.to_owned()), ..SourceLocator::default() },
            confidence: Some(1.0),
        },
    }
}

fn text(value: &str) -> Vec<Inline> {
    vec![Inline::Text { value: value.to_owned(), marks: Vec::new() }]
}

fn prefix_output(output: &mut ConverterOutput, prefix: &str) {
    for asset in &mut output.assets {
        prefix_asset(asset, prefix);
    }
    for block in &mut output.document.blocks {
        prefix_block(block, prefix);
    }
}

fn prefix_asset(asset: &mut Asset, prefix: &str) {
    asset.id.0 = format!("{prefix}-{}", asset.id.0);
}

fn prefix_block(block: &mut BlockNode, prefix: &str) {
    block.id.0 = format!("{prefix}-{}", block.id.0);
    match &mut block.block {
        Block::Image { asset, .. } => asset.0 = format!("{prefix}-{}", asset.0),
        Block::Page { blocks: children, .. }
        | Block::Slide { blocks: children, .. }
        | Block::Sheet { blocks: children, .. }
        | Block::Footnote { blocks: children, .. } => {
            for child in children {
                prefix_block(child, prefix);
            }
        }
        Block::List { items, .. } => {
            for item in items {
                for child in &mut item.blocks {
                    prefix_block(child, prefix);
                }
            }
        }
        Block::Table { rows, .. } => {
            for row in rows {
                for cell in &mut row.cells {
                    for child in &mut cell.blocks {
                        prefix_block(child, prefix);
                    }
                }
            }
        }
        _ => {}
    }
}

fn format_filetime(value: u64) -> String {
    const TICKS_PER_SECOND: u64 = 10_000_000;
    const UNIX_OFFSET: i128 = 11_644_473_600;
    let seconds = i128::from(value / TICKS_PER_SECOND) - UNIX_OFFSET;
    let fraction = value % TICKS_PER_SECOND;
    let days = seconds.div_euclid(86_400);
    let day_seconds = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(i64::try_from(days).unwrap_or(i64::MAX));
    let hour = day_seconds / 3600;
    let minute = day_seconds % 3600 / 60;
    let second = day_seconds % 60;
    if fraction == 0 {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
    } else {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{fraction:07}Z")
    }
}

fn civil_from_days(days_since_unix: i64) -> (i64, i64, i64) {
    let z = days_since_unix + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_is_stable_rfc3339() {
        assert_eq!(format_filetime(116_444_736_000_000_000), "1970-01-01T00:00:00Z");
        assert_eq!(format_filetime(116_444_736_001_234_567), "1970-01-01T00:00:00.1234567Z");
    }
}
