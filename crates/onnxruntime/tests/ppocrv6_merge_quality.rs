//! Explicit official-model degraded-scan OCR-to-IR quality gate.

#![allow(unexpected_cfgs)]

#[cfg(not(ppocrv6_merge_quality))]
#[test]
#[ignore = "run the explicit Bazel ppocrv6_merge_quality target with pinned model and runtime"]
fn explicit_quality_target_requires_pinned_artifacts() {}

#[cfg(ppocrv6_merge_quality)]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::similar_names,
    clippy::too_many_lines,
    reason = "the hash-bound quality authority uses small fixture dimensions and exact numeric parameters"
)]
mod quality {
    use into_markdown_core::{ExecutionContext, ExecutionOptions, OcrPolicy, ResourceLimits};
    use into_markdown_ocr::{
        AcquiredModelArtifact, ManifestModelResolver, MergeConfig, ModelAcquisition, ModelFetcher,
        ModelManager, ModelManagerError, OcrPageInput, PpOcrTextRecognizer, RecognitionConfig,
        RuntimeArtifact, merge_document,
    };
    use into_markdown_onnxruntime::{OrtSessionFactory, RuntimeLibrary};
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use unicode_normalization::UnicodeNormalization as _;

    struct QualityFetcher {
        archive: Vec<u8>,
        dictionary: Vec<u8>,
    }

    impl ModelFetcher for QualityFetcher {
        fn open(
            &self,
            artifact: &RuntimeArtifact,
            context: &ExecutionContext,
        ) -> Result<AcquiredModelArtifact, ModelManagerError> {
            context.checkpoint()?;
            let (acquisition, bytes) = match artifact.role.as_str() {
                "recognizer" => (
                    ModelAcquisition::ArchiveMember {
                        archive_sha256: artifact.archive_sha256.clone().unwrap(),
                        archive_size: artifact.archive_size.unwrap(),
                        member: artifact.archive_member.clone().unwrap(),
                    },
                    self.archive.clone(),
                ),
                "character-table" => (ModelAcquisition::Direct, self.dictionary.clone()),
                role => panic!("unexpected quality artifact role {role}"),
            };
            Ok(AcquiredModelArtifact { acquisition, bytes: Box::new(std::io::Cursor::new(bytes)) })
        }
    }

    #[test]
    fn official_recognizer_and_real_merge_meet_hash_bound_degraded_scan_cer() {
        let runfiles = PathBuf::from(std::env::var_os("TEST_SRCDIR").expect("Bazel runfiles"));
        let authority: serde_json::Value = serde_json::from_slice(
            &fs::read(runfiles.join("_main/models/ocr-merge-quality-authority.json")).unwrap(),
        )
        .unwrap();
        let manifest_bytes = fs::read(runfiles.join("_main/fixtures/manifest.json")).unwrap();
        assert_eq!(
            format!("{:x}", Sha256::digest(&manifest_bytes)),
            authority["fixture_manifest_sha256"].as_str().unwrap()
        );
        let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes).unwrap();
        let repository = env!("OCR_QUALITY_MODEL_REPOSITORY");
        assert_ne!(repository, "unsupported", "quality target requires an audited platform");
        let archive = read_model_archive(&runfiles, repository).unwrap();
        assert_eq!(
            format!("{:x}", Sha256::digest(&archive)),
            "1e13b22717b1edd89d4cde4fda272b6c17d5b505c97c2baea99da1a3a2d54b29"
        );
        let dictionary = fs::read(runfiles.join("_main/models/ppocrv6_tiny_dict.txt")).unwrap();
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let model_root = tempfile::tempdir().unwrap();
        let manager =
            Arc::new(ModelManager::embedded(model_root.path().to_path_buf(), None).unwrap());
        manager
            .install(
                "pp-ocrv6-tiny-recognizer-onnx",
                &QualityFetcher { archive, dictionary },
                &context,
            )
            .unwrap();
        let library = env!("OCR_QUALITY_ORT_LIBRARY");
        let library_runfile =
            runfiles.join(env!("OCR_QUALITY_ORT_REPOSITORY")).join(library).canonicalize().unwrap();
        let trusted_root = library_runfile
            .ancestors()
            .nth(Path::new(library).components().count())
            .unwrap()
            .to_path_buf();
        let loaded = Arc::new(RuntimeLibrary::load(&trusted_root, &library_runfile).unwrap());
        let factory = Arc::new(
            OrtSessionFactory::new(
                loaded,
                runfiles.join(env!("OCR_QUALITY_WORKER")).canonicalize().unwrap(),
            )
            .unwrap(),
        );
        let runtime = Arc::new(
            into_markdown_ocr::OnnxRuntime::new(
                Arc::new(ManifestModelResolver::new(Arc::clone(&manager))),
                factory,
                into_markdown_ocr::RuntimeConfig::default(),
            )
            .unwrap(),
        );
        let recognizer = PpOcrTextRecognizer::from_installed(
            runtime,
            &manager,
            RecognitionConfig::default(),
            &context,
        )
        .unwrap();

        let mut edits = 0_usize;
        let mut characters = 0_usize;
        for golden in manifest["ocr_quality"]["goldens"].as_array().unwrap() {
            let id = golden["fixture_id"].as_str().unwrap();
            let bytes =
                fs::read(runfiles.join(format!("_main/fixtures/small/ocr/{id}.png"))).unwrap();
            assert_eq!(
                format!("{:x}", Sha256::digest(&bytes)),
                golden["fixture_sha256"].as_str().unwrap()
            );
            let image = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
                .unwrap()
                .to_rgb8();
            let degraded = degrade(image.as_raw(), &authority);
            let width = image.width() as usize;
            let height = image.height() as usize;
            let crop = into_markdown_ocr::CropDescriptor {
                polygon: [
                    (0.0, 0.0),
                    ((width - 1) as f32, 0.0),
                    ((width - 1) as f32, (height - 1) as f32),
                    (0.0, (height - 1) as f32),
                ],
                width: (width - 1) as u32,
                height: (height - 1) as u32,
            };
            let view = into_markdown_ocr::PixelView {
                width,
                height,
                row_stride: width * 3,
                format: into_markdown_ocr::PixelFormat::Rgb8,
                orientation: into_markdown_ocr::ImageOrientation::Normal,
                bytes: &degraded,
            };
            let recognized = futures::executor::block_on(recognizer.recognize(
                view,
                std::slice::from_ref(&crop),
                None,
                &context,
            ))
            .unwrap();
            let detection = into_markdown_ocr::DetectionResult {
                regions: vec![into_markdown_ocr::DetectedTextRegion {
                    polygon: [
                        (0.0, 0.0),
                        ((width - 1) as f32, 0.0),
                        ((width - 1) as f32, (height - 1) as f32),
                        (0.0, (height - 1) as f32),
                    ],
                    angle_degrees: 0.0,
                    confidence: 1.0,
                    crop,
                }],
                provider: "quality.authoritative-full-image-region".into(),
            };
            let output = merge_document(
                into_markdown_core::Document::default(),
                &[OcrPageInput {
                    page: 1,
                    page_width: width as f32,
                    page_height: height as f32,
                    detection: &detection,
                    recognition: &recognized,
                    detector_model: "authority-full-image-polygon",
                    recognizer_model: "pp-ocrv6-tiny-recognizer-onnx",
                }],
                &MergeConfig {
                    policy: OcrPolicy::Always,
                    minimum_confidence: 0.0,
                    ..MergeConfig::default()
                },
                &context,
            )
            .unwrap();
            let actual = canonical(&document_text(&output.document));
            let expected = canonical(golden["ground_truth_nfc"].as_str().unwrap());
            edits += edit_distance(&expected, &actual);
            characters += expected.len();
        }
        assert_eq!(
            characters,
            authority["expected_evaluated_characters"].as_u64().unwrap() as usize
        );
        let cer = edits as f64 / characters as f64;
        assert!(
            cer <= authority["maximum_aggregate_cer"].as_f64().unwrap(),
            "degraded merge CER {cer:.6}"
        );
    }

    fn degrade(bytes: &[u8], authority: &serde_json::Value) -> Vec<u8> {
        let values = &authority["degradation"];
        assert_eq!(values["algorithm"], "contrast-compress-and-sparse-background-speckle");
        let black = values["black_level"].as_u64().unwrap() as u16;
        let white = values["white_level"].as_u64().unwrap() as u16;
        let seed = values["seed"].as_u64().unwrap() as usize;
        let modulus = values["speckle_modulus"].as_u64().unwrap() as usize;
        let remainder = values["speckle_remainder"].as_u64().unwrap() as usize;
        let source_minimum = values["speckle_source_minimum"].as_u64().unwrap() as u8;
        let speckle = values["speckle_level"].as_u64().unwrap() as u8;
        bytes
            .iter()
            .enumerate()
            .map(|(index, value)| {
                if (index % modulus + seed % modulus) % modulus == remainder
                    && *value >= source_minimum
                {
                    speckle
                } else {
                    (black + u16::from(*value) * (white - black) / 255) as u8
                }
            })
            .collect()
    }

    fn document_text(document: &into_markdown_core::Document) -> String {
        fn visit(blocks: &[into_markdown_core::BlockNode], output: &mut String) {
            for node in blocks {
                match &node.block {
                    into_markdown_core::Block::Page { blocks, .. } => visit(blocks, output),
                    into_markdown_core::Block::Paragraph(inlines) => {
                        for inline in inlines {
                            if let into_markdown_core::Inline::OcrText { value, .. } = inline {
                                output.push_str(value);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        let mut output = String::new();
        visit(&document.blocks, &mut output);
        output
    }

    fn canonical(value: &str) -> Vec<char> {
        value.nfc().filter(|character| !character.is_whitespace()).collect()
    }

    fn edit_distance(left: &[char], right: &[char]) -> usize {
        let mut row: Vec<usize> = (0..=right.len()).collect();
        for (left_index, left_character) in left.iter().enumerate() {
            let mut diagonal = row[0];
            row[0] = left_index + 1;
            for (right_index, right_character) in right.iter().enumerate() {
                let above = row[right_index + 1];
                row[right_index + 1] = if left_character == right_character {
                    diagonal
                } else {
                    diagonal.min(above).min(row[right_index]) + 1
                };
                diagonal = above;
            }
        }
        row[right.len()]
    }

    fn read_model_archive(runfiles: &Path, repository: &str) -> Result<Vec<u8>, String> {
        let relative = Path::new(repository).join("file/runtime-model.tar");
        fs::read(runfiles.join(&relative)).map_err(|error| {
            format!("missing pinned model runfile {}: {error}", relative.display())
        })
    }
}
