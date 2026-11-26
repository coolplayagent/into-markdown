use super::support::{convert, convert_strict, picture_fixture, rewrite_part};
use into_markdown_core::{AssetMode, ConversionOptions};
use into_markdown_render_markdown::render;
use std::io::{Cursor, Read};

fn emf_bytes() -> Vec<u8> {
    let mut bytes = vec![0; 88];
    for (offset, value) in [
        (0, 1_u32),
        (4, 88),
        (40, 0x464D_4520),
        (44, 0x10000),
        (48, 120),
        (52, 3),
        (56, 1),
        (72, 800),
        (76, 600),
        (80, 200),
        (84, 150),
    ] {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    for word in [18_u32, 12, 1, 14, 20, 0, 0, 20] {
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    bytes
}

fn emf_deck(bytes: &[u8], media_type: &str) -> Vec<u8> {
    let deck = picture_fixture(&[("drawing", "figure.emf", bytes.into())]);
    let mut types = String::new();
    zip::ZipArchive::new(Cursor::new(&deck))
        .unwrap()
        .by_name("[Content_Types].xml")
        .unwrap()
        .read_to_string(&mut types)
        .unwrap();
    let types = types.replace(
        "</Types>",
        &format!(r#"<Default Extension="emf" ContentType="{media_type}"/></Types>"#),
    );
    rewrite_part(&deck, "[Content_Types].xml", types.as_bytes())
}

#[test]
fn emf_preserves_original_bytes_and_renders_a_downloadable_link() {
    let bytes = emf_bytes();
    for declared in ["image/emf", "image/x-emf"] {
        let parsed = convert_strict(&emf_deck(&bytes, declared)).unwrap();
        assert_eq!(parsed.assets.len(), 1);
        assert_eq!(parsed.assets[0].bytes, bytes);
        assert_eq!(parsed.assets[0].media_type, "image/emf");
        let mut options = ConversionOptions::default();
        for mode in [AssetMode::Extract, AssetMode::Embed] {
            options.output.asset_mode = mode;
            let markdown = render(&parsed.document, &parsed.assets, &options).unwrap();
            assert!(
                markdown.lines().any(|line| line.starts_with('[')
                    && match mode {
                        AssetMode::Extract => line.ends_with(".emf>)"),
                        _ => line.contains("(<data:image/emf;base64,"),
                    }),
                "{markdown}"
            );
        }
    }
}

#[test]
fn malformed_or_mislabeled_emf_uses_existing_media_recovery_policy() {
    let mut invalid = emf_bytes();
    invalid[88 + 4..88 + 8].copy_from_slice(&0_u32.to_le_bytes());
    for deck in [emf_deck(&invalid, "image/emf"), emf_deck(&emf_bytes(), "image/png")] {
        assert!(convert_strict(&deck).is_err());
        let best_effort = convert(&deck).unwrap();
        assert!(best_effort.assets.is_empty());
        assert!(
            best_effort
                .diagnostics
                .iter()
                .any(|d| d.code == "presentation.unsupportedMediaOmitted")
        );
    }
}
