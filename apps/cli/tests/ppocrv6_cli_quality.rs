//! Explicit installed-product CLI OCR quality and identity gate.

#![allow(unexpected_cfgs)]

#[cfg(not(ppocrv6_cli_quality))]
#[test]
#[ignore = "run the explicit Bazel ppocrv6_cli_quality target with pinned artifacts"]
fn explicit_quality_target_requires_pinned_artifacts() {}

#[cfg(ppocrv6_cli_quality)]
mod quality {
    use into_markdown::{
        AcquiredModelArtifact, Block, ExecutionContext, ExecutionOptions, Inline, ModelAcquisition,
        ModelFetcher, ModelManager, ModelManagerError, OcrEvidenceStage, ResourceLimits,
        RuntimeArtifact,
    };
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
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
                id => panic!("unexpected CLI quality artifact {id}"),
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
                _ => panic!("incomplete CLI quality acquisition authority"),
            };
            Ok(AcquiredModelArtifact { acquisition, bytes: Box::new(std::io::Cursor::new(bytes)) })
        }
    }

    #[test]
    fn installed_cli_converts_the_authority_corpus_with_bound_identity() {
        let runfiles = PathBuf::from(std::env::var_os("TEST_SRCDIR").expect("Bazel runfiles"));
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let distribution = root.join("distribution");
        let inputs = root.join("inputs");
        let output = root.join("output");
        fs::create_dir_all(&distribution).unwrap();
        fs::create_dir_all(&inputs).unwrap();
        fs::create_dir_all(&output).unwrap();

        let cli = distribution.join(executable_name("into-md"));
        let worker = distribution.join(executable_name("onnxruntime-worker"));
        copy_executable(resolve_runfile(&runfiles, env!("OCR_CLI_BINARY")), &cli);
        copy_executable(resolve_runfile(&runfiles, env!("OCR_CLI_WORKER")), &worker);
        let runtime_relative = Path::new(env!("OCR_CLI_ORT_LIBRARY"));
        let runtime_source = runfiles.join(env!("OCR_CLI_ORT_REPOSITORY")).join(runtime_relative);
        let runtime_destination = distribution.join("onnxruntime").join(runtime_relative);
        fs::create_dir_all(runtime_destination.parent().unwrap()).unwrap();
        fs::copy(runtime_source, runtime_destination).unwrap();

        let manifest: serde_json::Value = serde_json::from_slice(
            &fs::read(runfiles.join("_main/fixtures/manifest.json")).unwrap(),
        )
        .unwrap();
        let first_id = manifest["ocr_quality"]["goldens"][0]["fixture_id"].as_str().unwrap();
        let first_input = inputs.join(format!("{first_id}.png"));
        fs::copy(runfiles.join(format!("_main/fixtures/small/ocr/{first_id}.png")), &first_input)
            .unwrap();
        let unavailable = Command::new(&cli)
            .args([
                "--no-config",
                first_input.to_str().unwrap(),
                "--ocr",
                "always",
                "--ocr-model",
                "pp-ocrv6-tiny-zh-en",
                "--emit",
                "ir-json",
            ])
            .output()
            .unwrap();
        assert_eq!(
            unavailable.status.code(),
            Some(9),
            "unexpected missing-model CLI failure: {}",
            String::from_utf8_lossy(&unavailable.stderr)
        );
        assert!(
            String::from_utf8_lossy(&unavailable.stderr)
                .contains("models install pp-ocrv6-tiny-zh-en")
        );
        assert!(!distribution.join("models").exists(), "conversion must never auto-download");

        let detector = read_archive(&runfiles, env!("OCR_CLI_DETECTOR_REPOSITORY"));
        let recognizer = read_archive(&runfiles, env!("OCR_CLI_RECOGNIZER_REPOSITORY"));
        let dictionary = fs::read(runfiles.join("_main/models/ppocrv6_tiny_dict.txt")).unwrap();
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let manager = ModelManager::embedded(distribution.join("models"), None).unwrap();
        manager
            .install(
                "pp-ocrv6-tiny-zh-en",
                &QualityFetcher { detector, recognizer, dictionary },
                &context,
            )
            .unwrap();
        assert_eq!(context.reserved_memory_bytes(), 0);

        let mut command = Command::new(&cli);
        command.arg("--no-config");
        for golden in manifest["ocr_quality"]["goldens"].as_array().unwrap() {
            let id = golden["fixture_id"].as_str().unwrap();
            let destination = inputs.join(format!("{id}.png"));
            if !destination.exists() {
                fs::copy(runfiles.join(format!("_main/fixtures/small/ocr/{id}.png")), &destination)
                    .unwrap();
            }
            command.arg(destination);
        }
        let converted = command
            .args([
                "--output-dir",
                output.to_str().unwrap(),
                "--emit",
                "ir-json",
                "--ocr",
                "always",
                "--ocr-model",
                "pp-ocrv6-tiny-zh-en",
                "--jobs",
                "2",
            ])
            .output()
            .unwrap();
        assert!(
            converted.status.success(),
            "CLI failed: {}",
            String::from_utf8_lossy(&converted.stderr)
        );

        let mut totals = BTreeMap::<String, (usize, usize)>::new();
        for golden in manifest["ocr_quality"]["goldens"].as_array().unwrap() {
            let id = golden["fixture_id"].as_str().unwrap();
            let document: into_markdown::Document =
                serde_json::from_slice(&fs::read(output.join(format!("{id}.json"))).unwrap())
                    .unwrap();
            let (actual, evidence_count) = document_text_and_validate(&document);
            assert!(evidence_count > 0, "{id} must retain bound OCR evidence through the CLI");
            assert!(document.blocks.iter().all(|node| !node.id.0.contains("pdf-page")));
            let expected = canonical(golden["ground_truth_nfc"].as_str().unwrap());
            let actual = canonical(&actual);
            let total = totals.entry(golden["group"].as_str().unwrap().into()).or_default();
            total.0 += edit_distance(&expected, &actual);
            total.1 += expected.len();
        }
        for (group, (maximum_errors, exact_characters)) in BTreeMap::from([
            ("english", (1, 185)),
            ("mixed", (1, 116)),
            ("simplified", (0, 65)),
            ("traditional", (6, 65)),
        ]) {
            assert_eq!(totals[group].1, exact_characters, "{group} character authority drifted");
            assert!(
                totals[group].0 <= maximum_errors,
                "{group} CLI OCR added errors: {} > {maximum_errors}",
                totals[group].0
            );
        }
    }

    fn document_text_and_validate(document: &into_markdown::Document) -> (String, usize) {
        fn visit(blocks: &[into_markdown::BlockNode], output: &mut String, evidence: &mut usize) {
            for node in blocks {
                match &node.block {
                    Block::Page { blocks, .. } => {
                        assert!(node.id.0.starts_with("image-page-"));
                        visit(blocks, output, evidence);
                    }
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

    fn resolve_runfile(runfiles: &Path, value: &str) -> PathBuf {
        let value = PathBuf::from(value);
        if value.is_absolute() { value } else { runfiles.join(value) }
    }

    fn executable_name(stem: &str) -> String {
        if cfg!(windows) { format!("{stem}.exe") } else { stem.into() }
    }

    fn copy_executable(source: PathBuf, destination: &Path) {
        fs::copy(source, destination).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(destination, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
}
