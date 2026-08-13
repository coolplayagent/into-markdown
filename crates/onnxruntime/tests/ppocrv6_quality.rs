//! Explicit official-model OCR corpus quality gate.

#![allow(unexpected_cfgs)]

#[cfg(not(ppocrv6_quality))]
#[test]
#[ignore = "run the explicit Bazel ppocrv6_quality target with pinned model and ONNX Runtime artifacts"]
fn explicit_quality_target_requires_pinned_artifacts() {}

#[cfg(ppocrv6_quality)]
mod quality {
    use into_markdown_core::ExecutionContext;
    use into_markdown_ocr::{
        AcquiredModelArtifact, ManifestModelResolver, ModelAcquisition, ModelFetcher, ModelManager,
        ModelManagerError, RuntimeArtifact,
    };
    use into_markdown_onnxruntime::{OrtSessionFactory, RuntimeLibrary};
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

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
    fn official_ppocrv6_recognizer_meets_checked_corpus_cer() {
        let runfiles = PathBuf::from(std::env::var_os("TEST_SRCDIR").expect("Bazel runfiles"));
        let repository = env!("OCR_QUALITY_MODEL_REPOSITORY");
        assert_ne!(repository, "unsupported", "quality target requires an audited platform");
        let archive = fs::read(runfiles.join(repository).join("runtime-model.tar")).unwrap();
        assert_eq!(archive.len(), 4_526_080);
        assert_eq!(
            format!("{:x}", Sha256::digest(&archive)),
            "1e13b22717b1edd89d4cde4fda272b6c17d5b505c97c2baea99da1a3a2d54b29"
        );
        let dictionary = fs::read(runfiles.join("_main/models/ppocrv6_tiny_dict.txt")).unwrap();
        let install_context = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits::default(),
        );
        let model_root = tempfile::tempdir().unwrap();
        let manager =
            Arc::new(ModelManager::embedded(model_root.path().to_path_buf(), None).unwrap());
        manager
            .install(
                "pp-ocrv6-tiny-recognizer-onnx",
                &QualityFetcher { archive, dictionary },
                &install_context,
            )
            .unwrap();

        let library = env!("OCR_QUALITY_ORT_LIBRARY");
        let library_runfile =
            runfiles.join(env!("OCR_QUALITY_ORT_REPOSITORY")).join(library).canonicalize().unwrap();
        let component_count = Path::new(library).components().count();
        let trusted_root = library_runfile.ancestors().nth(component_count).unwrap().to_path_buf();
        let loaded = Arc::new(RuntimeLibrary::load(&trusted_root, &library_runfile).unwrap());
        let worker = runfiles.join(env!("OCR_QUALITY_WORKER")).canonicalize().unwrap();
        let factory = Arc::new(OrtSessionFactory::new(loaded, worker).unwrap());
        let tensor_runtime = Arc::new(
            into_markdown_ocr::OnnxRuntime::new(
                Arc::new(ManifestModelResolver::new(Arc::clone(&manager))),
                factory,
                into_markdown_ocr::RuntimeConfig::default(),
            )
            .unwrap(),
        );
        let recognizer = into_markdown_ocr::PpOcrTextRecognizer::from_installed(
            tensor_runtime,
            &manager,
            into_markdown_ocr::RecognitionConfig::default(),
            &install_context,
        )
        .unwrap();
        let manifest: serde_json::Value = serde_json::from_slice(
            &fs::read(runfiles.join("_main/fixtures/manifest.json")).unwrap(),
        )
        .unwrap();
        let goldens = manifest["ocr_quality"]["goldens"].as_array().unwrap();
        let limits = into_markdown_core::ResourceLimits::default();
        let mut group_totals = std::collections::BTreeMap::<String, (usize, usize, f64)>::new();

        for golden in goldens {
            let id = golden["fixture_id"].as_str().unwrap();
            let bytes =
                fs::read(runfiles.join(format!("_main/fixtures/small/ocr/{id}.png"))).unwrap();
            let image = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
                .unwrap()
                .to_rgb8();
            let (width, height) = image.dimensions();
            let crop = into_markdown_ocr::CropDescriptor {
                polygon: [
                    (0.0, 0.0),
                    ((width - 1) as f32, 0.0),
                    ((width - 1) as f32, (height - 1) as f32),
                    (0.0, (height - 1) as f32),
                ],
                width: width - 1,
                height: height - 1,
            };
            let context = ExecutionContext::new(
                into_markdown_core::ExecutionOptions::default(),
                limits.clone(),
            );
            let view = into_markdown_ocr::PixelView {
                width: image.width() as usize,
                height: image.height() as usize,
                row_stride: image.width() as usize * 3,
                format: into_markdown_ocr::PixelFormat::Rgb8,
                orientation: into_markdown_ocr::ImageOrientation::Normal,
                bytes: image.as_raw(),
            };
            let result = futures::executor::block_on(recognizer.recognize(
                view,
                std::slice::from_ref(&crop),
                None,
                &context,
            ))
            .unwrap();
            let expected = cer_chars(golden["ground_truth_nfc"].as_str().unwrap());
            let actual = cer_chars(&result.regions[0].text);
            let edits = edit_distance(&expected, &actual);
            eprintln!(
                "{id} actual={:?} edits={edits} chars={} confidence={}",
                result.regions[0].text,
                expected.len(),
                result.regions[0].confidence
            );
            let group = golden["group"].as_str().unwrap().to_owned();
            let maximum = golden["maximum_cer"].as_f64().unwrap();
            let total = group_totals.entry(group).or_insert((0, 0, maximum));
            assert_eq!(total.2, maximum, "group threshold drift");
            total.0 += edits;
            total.1 += expected.len();
        }
        let expected = std::collections::BTreeMap::from([
            ("english".to_owned(), (1, 185, 0.05_f64)),
            ("mixed".to_owned(), (1, 116, 0.08_f64)),
            ("simplified".to_owned(), (0, 65, 0.05_f64)),
            ("traditional".to_owned(), (6, 65, 0.10_f64)),
        ]);
        assert_eq!(group_totals, expected, "checked corpus authority drift");
        for (group, (edits, characters, maximum)) in group_totals {
            let cer = edits as f64 / characters as f64;
            assert!(cer <= maximum, "{group} CER {cer:.6} exceeds {maximum:.6}");
        }
    }

    fn cer_chars(value: &str) -> Vec<char> {
        value.chars().filter(|character| !character.is_whitespace()).collect()
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
}
