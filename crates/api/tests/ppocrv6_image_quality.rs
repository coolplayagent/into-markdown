//! Explicit official full-pipeline product API quality gate.

#![allow(unexpected_cfgs)]

#[cfg(not(ppocrv6_image_quality))]
#[test]
#[ignore = "run the explicit Bazel ppocrv6_image_quality target with pinned artifacts"]
fn explicit_quality_target_requires_pinned_artifacts() {}

#[cfg(ppocrv6_image_quality)]
mod quality {
    use into_markdown::{
        AcquiredModelArtifact, Block, ConversionError, ConversionOptions, ConversionRequest,
        ErrorCode, ExecutionContext, ExecutionOptions, Inline, InputRef, InstalledOcrConfig,
        ModelAcquisition, ModelFetcher, ModelManager, ModelManagerError, OcrEvidenceStage,
        OcrPolicy, ResourceLimits, RuntimeArtifact, Services,
    };
    use sha2::{Digest, Sha256};
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use unicode_normalization::UnicodeNormalization as _;

    struct QualityFetcher {
        detector: Vec<u8>,
        recognizer: Vec<u8>,
        dictionary: Vec<u8>,
    }

    impl ModelFetcher for QualityFetcher {
        fn open(
            &self,
            artifact: &RuntimeArtifact,
            context: &ExecutionContext,
        ) -> Result<AcquiredModelArtifact, ModelManagerError> {
            context.checkpoint()?;
            let bytes = match artifact.id.as_str() {
                "ppocrv6-tiny-detector-onnx-model" => self.detector.clone(),
                "ppocrv6-tiny-recognizer-onnx-model" => self.recognizer.clone(),
                "ppocrv6-tiny-recognizer-character-table" => self.dictionary.clone(),
                id => panic!("unexpected quality artifact {id}"),
            };
            let acquisition = match (
                artifact.archive_sha256.as_ref(),
                artifact.archive_size,
                artifact.archive_member.as_ref(),
            ) {
                (Some(hash), Some(size), Some(member)) => ModelAcquisition::ArchiveMember {
                    archive_sha256: hash.clone(),
                    archive_size: size,
                    member: member.clone(),
                },
                (None, None, None) => ModelAcquisition::Direct,
                _ => panic!("incomplete quality acquisition authority"),
            };
            Ok(AcquiredModelArtifact { acquisition, bytes: Box::new(std::io::Cursor::new(bytes)) })
        }
    }

    #[test]
    fn official_full_pipeline_meets_product_api_corpus_quality_and_identity() {
        let runfiles = PathBuf::from(std::env::var_os("TEST_SRCDIR").expect("Bazel runfiles"));
        let detector = read_archive(&runfiles, env!("OCR_DETECTOR_REPOSITORY"));
        let recognizer = read_archive(&runfiles, env!("OCR_RECOGNIZER_REPOSITORY"));
        assert_eq!(detector.len(), 1_792_000);
        assert_eq!(
            format!("{:x}", Sha256::digest(&detector)),
            "ff6ab415b0a6e0c488550f2fb5d5046f1719848df220b2dc21b56402a65bc05d"
        );
        assert_eq!(recognizer.len(), 4_526_080);
        assert_eq!(
            format!("{:x}", Sha256::digest(&recognizer)),
            "1e13b22717b1edd89d4cde4fda272b6c17d5b505c97c2baea99da1a3a2d54b29"
        );
        let dictionary = fs::read(runfiles.join("_main/models/ppocrv6_tiny_dict.txt")).unwrap();
        let model_root = tempfile::tempdir().unwrap();
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let manager = ModelManager::embedded(model_root.path().to_path_buf(), None).unwrap();
        let fetcher = QualityFetcher { detector, recognizer, dictionary };
        let status = manager.install("pp-ocrv6-tiny-zh-en", &fetcher, &context).unwrap();
        assert_eq!(status.state, "installed");

        let runtime_relative = env!("OCR_QUALITY_ORT_LIBRARY");
        let runtime_library = runfiles
            .join(env!("OCR_QUALITY_ORT_REPOSITORY"))
            .join(runtime_relative)
            .canonicalize()
            .unwrap();
        let runtime_root = runtime_library
            .ancestors()
            .nth(Path::new(runtime_relative).components().count())
            .unwrap()
            .to_path_buf();
        let worker = runfiles.join(env!("OCR_QUALITY_WORKER")).canonicalize().unwrap();
        let mut options = ConversionOptions::default();
        options.ocr.policy = OcrPolicy::Always;
        options.ocr.minimum_confidence = 0.0;
        let service_config = InstalledOcrConfig {
            writable_model_root: model_root.path().to_path_buf(),
            bundled_model_root: None,
            runtime_trusted_root: runtime_root,
            runtime_library,
            worker_executable: worker,
            model_bundle: "pp-ocrv6-tiny-zh-en".into(),
        };
        for component in ["pp-ocrv6-tiny-detector-onnx", "pp-ocrv6-tiny-recognizer-onnx"] {
            let artifact = manager.path(component).unwrap().join("inference.onnx");
            let exact = fs::read(&artifact).unwrap();
            let mut corrupt = exact.clone();
            corrupt[0] ^= 0xff;
            fs::write(&artifact, corrupt).unwrap();
            let error = service_error(&service_config, &options, &context);
            assert_eq!(error.code(), ErrorCode::ComponentUnavailable);
            assert!(error.to_string().contains("models install pp-ocrv6-tiny-zh-en"));
            assert_eq!(context.reserved_memory_bytes(), 0);
            fs::write(&artifact, exact).unwrap();
            manager.verify("pp-ocrv6-tiny-zh-en").unwrap();
        }
        let low_limits = ResourceLimits { max_memory_bytes: 1024, ..ResourceLimits::default() };
        let low_context = ExecutionContext::new(ExecutionOptions::default(), low_limits.clone());
        let mut low_options = options.clone();
        low_options.limits = low_limits;
        let error = service_error(&service_config, &low_options, &low_context);
        assert_eq!(error.code(), ErrorCode::ResourceLimit);
        assert_eq!(low_context.reserved_memory_bytes(), 0);

        let ocr =
            into_markdown::installed_ocr_service(&service_config, &options, &context).unwrap();
        let engine = into_markdown::default_engine_with_services(Services {
            ocr: Some(ocr),
            ..Services::default()
        })
        .unwrap();

        let manifest: serde_json::Value = serde_json::from_slice(
            &fs::read(runfiles.join("_main/fixtures/manifest.json")).unwrap(),
        )
        .unwrap();
        let mut totals = BTreeMap::<String, (usize, usize)>::new();
        for golden in manifest["ocr_quality"]["goldens"].as_array().unwrap() {
            let id = golden["fixture_id"].as_str().unwrap();
            let bytes =
                fs::read(runfiles.join(format!("_main/fixtures/small/ocr/{id}.png"))).unwrap();
            assert_eq!(
                format!("{:x}", Sha256::digest(&bytes)),
                golden["fixture_sha256"].as_str().unwrap()
            );
            let mut request =
                ConversionRequest::new(InputRef::bytes(bytes, Some(format!("{id}.png"))));
            request.options = options.clone();
            let result = futures::executor::block_on(engine.convert(request)).unwrap();
            let (actual, evidence_count) = document_text_and_validate(&result.document);
            assert!(evidence_count > 0, "{id} must retain bound OCR evidence");
            assert!(!result.markdown.contains("pdf-page-"));
            let expected = canonical(golden["ground_truth_nfc"].as_str().unwrap());
            let actual = canonical(&actual);
            let edits = edit_distance(&expected, &actual);
            let total = totals.entry(golden["group"].as_str().unwrap().into()).or_default();
            total.0 += edits;
            total.1 += expected.len();
        }
        let authority = BTreeMap::from([
            ("english", (1, 185)),
            ("mixed", (1, 116)),
            ("simplified", (0, 65)),
            ("traditional", (6, 65)),
        ]);
        for (group, (maximum_errors, exact_characters)) in authority {
            let (actual_errors, actual_characters) = totals[group];
            assert_eq!(actual_characters, exact_characters, "{group} character authority drifted");
            assert!(
                actual_errors <= maximum_errors,
                "{group} added errors relative to the hash-bound #29 authority: \
                 {actual_errors} > {maximum_errors}"
            );
        }
        assert!(totals["traditional"].0 as f64 / totals["traditional"].1 as f64 <= 0.10);
        assert!(totals["simplified"].0 as f64 / totals["simplified"].1 as f64 <= 0.05);
        assert!(totals["english"].0 as f64 / totals["english"].1 as f64 <= 0.05);
        assert!(totals["mixed"].0 as f64 / totals["mixed"].1 as f64 <= 0.08);
    }

    fn document_text_and_validate(document: &into_markdown::Document) -> (String, usize) {
        fn visit(blocks: &[into_markdown::BlockNode], output: &mut String, evidence: &mut usize) {
            for node in blocks {
                match &node.block {
                    Block::Page { blocks, .. } => visit(blocks, output, evidence),
                    Block::Paragraph(inlines) => {
                        for inline in inlines {
                            if let Inline::OcrText { value, evidence: bound, .. } = inline {
                                output.push_str(value);
                                *evidence += 1;
                                assert_eq!(bound.chain.len(), 3);
                                assert_eq!(bound.chain[0].stage, OcrEvidenceStage::Detection);
                                assert_eq!(
                                    bound.chain[0].model.as_deref(),
                                    Some("pp-ocrv6-tiny-detector-onnx")
                                );
                                assert_eq!(bound.chain[1].stage, OcrEvidenceStage::Recognition);
                                assert_eq!(
                                    bound.chain[1].model.as_deref(),
                                    Some("pp-ocrv6-tiny-recognizer-onnx")
                                );
                                assert_eq!(bound.chain[2].stage, OcrEvidenceStage::Merge);
                                for region in &bound.regions {
                                    assert!(region.polygon.iter().all(|point| {
                                        point.x.is_finite()
                                            && point.y.is_finite()
                                            && point.x >= 0.0
                                            && point.y >= 0.0
                                    }));
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        let mut output = String::new();
        let mut evidence = 0;
        visit(&document.blocks, &mut output, &mut evidence);
        (output, evidence)
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

    fn read_archive(runfiles: &Path, repository: &str) -> Vec<u8> {
        assert_ne!(repository, "unsupported", "quality target requires an audited platform");
        fs::read(runfiles.join(repository).join("file/runtime-model.tar")).unwrap()
    }

    fn service_error(
        config: &InstalledOcrConfig,
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> ConversionError {
        match into_markdown::installed_ocr_service(config, options, context) {
            Ok(_) => panic!("corrupt or resource-starved OCR service unexpectedly assembled"),
            Err(error) => error,
        }
    }
}
