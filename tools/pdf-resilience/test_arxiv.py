"""Coverage and artifact integrity checks for the opt-in PDF corpus runner."""
import importlib.util
import pathlib
import tempfile
import unittest
import json
import zipfile
import hashlib
from types import SimpleNamespace
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("arxiv_corpus", pathlib.Path(__file__).with_name("arxiv.py"))
corpus = importlib.util.module_from_spec(spec)
spec.loader.exec_module(corpus)
audit_spec = importlib.util.spec_from_file_location("arxiv_audit", pathlib.Path(__file__).with_name("audit_arxiv.py"))
audit_module = importlib.util.module_from_spec(audit_spec)
audit_spec.loader.exec_module(audit_module)

web_spec = importlib.util.spec_from_file_location("web_parity", pathlib.Path(__file__).with_name("web_cli.py"))
web_module = importlib.util.module_from_spec(web_spec)
web_spec.loader.exec_module(web_module)


class CoverageTests(unittest.TestCase):
    def test_whole_source_pdf_recovery_covers_pages_only_after_hash_verification(self):
        with tempfile.TemporaryDirectory() as directory:
            source = pathlib.Path(directory) / "wrapped.pdf"
            source.write_bytes(b"complete original PDF container")
            digest = corpus.digest(source)
            paper = dict(format="pdf", sha256=digest)
            case = dict(items=[dict(diagnostics=[dict(code="conversion.recovery.originalFile", locator=None)])],
                        originalAttachmentPaths=["original.pdf"],
                        assets=[dict(path="original.pdf", sha256=digest)])
            corpus.validate_originals(source, paper, case)
            self.assertTrue(case['documentOriginalFallback'])
            case['assets'][0]['sha256'] = 'modified'
            corpus.validate_originals(source, paper, case)
            self.assertFalse(case['documentOriginalFallback'])
            self.assertEqual(case['originalHashMismatch'], ['original.pdf'])

    def test_web_parity_downloads_in_fixed_chunks_and_compares_visible_body(self):
        payload = b"source bytes" * 100_000
        import io
        class Response(io.BytesIO):
            def read(self, size=-1):
                if not 0 < size <= 64 * 1024:
                    raise AssertionError("download requested an unbounded read")
                return super().read(size)
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            downloaded = root / "artifact"
            result = web_module.copy_response(Response(payload), downloaded)
            self.assertEqual(result, (hashlib.sha256(payload).hexdigest(), len(payload)))
            self.assertEqual(downloaded.read_bytes(), payload)
            self.assertEqual(web_module.copy_response(Response(payload)), result)
            a, b = root / "a.md", root / "b.md"
            a.write_text("Body ![Image](<cli_assets/asset-sha.png>)\n")
            b.write_text("Body ![Image](<opaque/asset-sha.png>)\n")
            self.assertTrue(web_module.markdown_equal(a, b))
            b.write_text("Missing body ![Image](<opaque/asset-sha.png>)\n")
            self.assertFalse(web_module.markdown_equal(a, b))

    def test_empty_delivery_requires_engine_evidence_and_zero_expected_pages(self):
        item = dict(status="success", outcome="complete", reasonCode="emptySource",
                    diagnostics=[dict(code="emptySource", severity="info")])
        case = dict(expectedPages=0, exitCode=0, items=[item])
        self.assertTrue(corpus.certified_empty(case))
        for field, value in [("outcome", "degraded"), ("reasonCode", None),
                             ("diagnostics", []), ("warnings", ["lost body"])]:
            self.assertFalse(corpus.certified_empty({**case, "items": [{**item, field: value}]}))
        self.assertFalse(corpus.certified_empty({**case, "expectedPages": 1}))
        self.assertFalse(corpus.certified_empty({**case, "exitCode": 10}))

    def test_nested_original_binds_to_member_and_ignores_cover_image(self):
        with tempfile.TemporaryDirectory() as directory:
            source = pathlib.Path(directory) / "book.epub"
            member = b"<html>Chapter body</html>"
            with zipfile.ZipFile(source, "w") as archive:
                archive.writestr("OEBPS/chapter.xhtml", member)
            case = dict(items=[dict(diagnostics=[dict(code="conversion.recovery.originalFile",
                locator=dict(part="OEBPS/chapter.xhtml"))])], originalAttachmentPaths=["chapter.xhtml"],
                assets=[dict(path="cover.jpg", sha256="unrelated-image"),
                        dict(path="chapter.xhtml", sha256=hashlib.sha256(member).hexdigest())])
            self.assertTrue(corpus.nested_original_checks(source, case)[0]["matched"])
            case["assets"][1]["sha256"] = "corrupt"
            self.assertFalse(corpus.nested_original_checks(source, case)[0]["matched"])
            case["originalAttachmentPaths"] = []
            self.assertFalse(corpus.nested_original_checks(source, case)[0]["matched"])

    def test_failed_conversion_is_missing_in_quality_audit(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            manifest = root / "manifest.json"
            manifest.write_text(json.dumps(dict(papers=[dict(id="failed", pages=1, features=dict(reviewPages=[1]))])))
            report = root / "report.json"
            report.write_text(json.dumps(dict(manifestSha256=corpus.digest(manifest), binarySha256="fixture",
                cases=[dict(id="failed", ocr="off", passed=False)])))
            (root / "failed").mkdir()
            (root / "failed/text.json").write_text(json.dumps(["Original body remains unavailable in output."]))
            result = audit_module.audit(manifest, report, root)
            self.assertEqual(result['cases'][0]['counts'], {'missing': 1})
            self.assertFalse(result['manualReviewComplete'])

    def inspect(self, content, pages=2, assets=()):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            for asset in assets:
                (root / asset).write_bytes(b"asset payload")
            document = root / "document.md"
            document.write_text(content, encoding="utf-8")
            return corpus.inspect(document, pages)

    def test_header_alone_does_not_certify_page_coverage(self):
        result = self.inspect('<a id="pdf-page-1"></a>\n## Page 1\n')
        self.assertEqual(result["emptyPages"], [1])
        self.assertEqual(result["missingPages"], [2])

    def test_internal_page_destinations_must_exist(self):
        result = self.inspect('<a id="pdf-page-1"></a>Body [valid](<#pdf-page-1>) [missing](<#pdf-page-2>)')
        self.assertEqual(result['brokenInternalLinks'], ['#pdf-page-2'])

    def test_source_hyperlinks_are_separate_from_missing_delivered_assets(self):
        result = self.inspect('[Source](</news/item>) [Asset](<document_assets/asset-a.txt>) ![Image](<photo.jpg>)')
        self.assertEqual(result['unresolvedSourceLinks'], ['/news/item'])
        self.assertEqual(result['brokenAssets'], ['document_assets/asset-a.txt', 'photo.jpg'])

    def test_original_attachment_and_render_are_checked(self):
        result = self.inspect(
            '<a id="pdf-page-1"></a>\n## Page 1\n![Page](<page.png>)\n'
            '<a id="pdf-page-2"></a>\n## Page 2\n[Original](<original.pdf#page=2>)\n',
            assets=("page.png", "original.pdf"))
        self.assertFalse(result["missingPages"])
        self.assertFalse(result["emptyPages"])
        self.assertFalse(result["brokenAssets"])
        self.assertEqual(result["pageEvidence"][1]["originalPdfReferences"], 1)

    def test_failed_input_keeps_later_inputs_and_both_ocr_modes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            papers = []
            for name in ("bad", "good"):
                source = root / (name + "v1.pdf")
                source.write_bytes(b"%PDF-source")
                papers.append(dict(id=name, version=1, pages=1, bytes=source.stat().st_size, sha256=corpus.digest(source)))
            manifest = root / "manifest.json"
            manifest.write_text(json.dumps(dict(papers=papers)))
            binary = root / "into-md"
            binary.write_bytes(b"fake isolated executable authority")
            def process(command, **kwargs):
                output = pathlib.Path(command[command.index("--output") + 1])
                good = "good" in output.parts
                if good:
                    output.write_text('<a id="pdf-page-1"></a>\n## Page 1\nRetained body\n')
                return SimpleNamespace(wait=lambda **kwargs: 0 if good else 10)
            args = SimpleNamespace(manifest=manifest, sources=root, into_md=binary, output=root / "results", resume=False, modes=["auto", "off"], watchdog_seconds=1)
            with patch.object(corpus.subprocess, "check_output", return_value=b'{"version":"fixture"}'), patch.object(corpus.subprocess, "Popen", side_effect=process), patch.object(corpus.sys, "platform", "linux"):
                self.assertEqual(corpus.run(args), 1)
            report = json.loads((args.output / "report.json").read_text())
            self.assertTrue(report["complete"])
            self.assertEqual((report["passed"], report["failed"]), (2, 2))
            self.assertEqual([c["id"] for c in report["cases"]], ["bad", "bad", "good", "good"])

    def test_broken_assets_and_duplicate_pages_are_reported(self):
        result = self.inspect('<a id="pdf-page-1"></a>text\n![Page](<missing.png>)\n'
                              '<a id="pdf-page-1"></a>text')
        self.assertEqual(result["brokenAssets"], ["missing.png"])
        self.assertTrue(result["duplicatePages"])

    def test_whole_document_recovery_requires_matching_original_bytes(self):
        for payload in (b"%PDF-source", b"%PDF-different", None):
            with self.subTest(payload=payload), tempfile.TemporaryDirectory() as directory:
                root = pathlib.Path(directory)
                source = root / "samplev1.pdf"
                source.write_bytes(b"%PDF-source")
                manifest = root / "manifest.json"
                manifest.write_text(json.dumps(dict(papers=[dict(
                    id="sample", version=1, pages=9, bytes=source.stat().st_size,
                    sha256=corpus.digest(source))])))
                binary = root / "into-md"
                binary.write_bytes(b"fixture executable")

                def process(command, **kwargs):
                    output = pathlib.Path(command[command.index("--output") + 1])
                    output.write_text('[Original PDF — all pages](<original.pdf>)\n')
                    if payload is not None:
                        output.with_name("original.pdf").write_bytes(payload)
                    report = pathlib.Path(command[command.index("--report") + 1])
                    report.write_text(json.dumps(dict(items=[dict(diagnostics=[dict(
                        code="pdf.recovery.originalPdf", locator=dict(page=None))])])))
                    return SimpleNamespace(wait=lambda **kwargs: 0)

                args = SimpleNamespace(manifest=manifest, sources=root, into_md=binary,
                    output=root / "results", resume=False, modes=["off"], watchdog_seconds=1)
                with patch.object(corpus.subprocess, "check_output", return_value=b'{}'), patch.object(corpus.subprocess, "Popen", side_effect=process), patch.object(corpus.sys, "platform", "linux"):
                    result = corpus.run(args)
                case = json.loads((args.output / "report.json").read_text())["cases"][0]
                self.assertEqual(result == 0, payload == b"%PDF-source")
                self.assertEqual(case["documentOriginalFallback"], payload == b"%PDF-source")



quality_spec = importlib.util.spec_from_file_location('conversion_quality', pathlib.Path(__file__).with_name('quality_gate.py'))
quality = importlib.util.module_from_spec(quality_spec)
quality_spec.loader.exec_module(quality)

import sys
with patch.dict(sys.modules, {'quality_gate': quality}):
    comparison_spec = importlib.util.spec_from_file_location('compare_ocr', pathlib.Path(__file__).with_name('compare_ocr.py'))
    comparison = importlib.util.module_from_spec(comparison_spec)
    comparison_spec.loader.exec_module(comparison)
    availability_spec = importlib.util.spec_from_file_location('classify_inputs', pathlib.Path(__file__).with_name('classify_inputs.py'))
    availability = importlib.util.module_from_spec(availability_spec)
    availability_spec.loader.exec_module(availability)
    reconcile_spec = importlib.util.spec_from_file_location('reconcile_reports', pathlib.Path(__file__).with_name('reconcile_reports.py'))
    reconciliation = importlib.util.module_from_spec(reconcile_spec)
    reconcile_spec.loader.exec_module(reconciliation)
    compact_spec = importlib.util.spec_from_file_location('compact_report', pathlib.Path(__file__).with_name('compact_report.py'))
    compaction = importlib.util.module_from_spec(compact_spec)
    compact_spec.loader.exec_module(compaction)



class OcrComparisonTests(unittest.TestCase):
    def test_encrypted_source_exception_requires_hash_bound_independent_evidence(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            report, proof = root / 'report.json', root / 'proof.json'
            case = dict(id='protected', sourceSha256='source', passed=False, exitCode=10,
                        items=[dict(errorCode='unsupported')], sourceClass='upstream_negative_or_malformed')
            report.write_text(json.dumps(dict(complete=True, binarySha256='binary', cases=[case])))
            proof.write_text(json.dumps(dict(records=[])))
            self.assertEqual(availability.classify(report, proof)['conversionFailureCount'], 1)
            entry = dict(id='protected', sourceSha256='source', encrypted=True,
                         evidence=['Independent reader confirms encrypted source content.'])
            proof.write_text(json.dumps(dict(records=[entry])))
            result = availability.classify(report, proof)
            self.assertTrue(result['readableConversionAcceptancePassed'])
            self.assertEqual(result['unreadableInputCount'], 1)
            self.assertEqual(result['deliveredInputs'], 0)
            self.assertEqual(result['failedDeliveryCount'], 1)
            entry['sourceSha256'] = 'different'
            proof.write_text(json.dumps(dict(records=[entry])))
            self.assertEqual(availability.classify(report, proof)['conversionFailureCount'], 1)

    def test_empty_recognition_on_text_image_counts_as_failure_and_blank_is_excluded(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            manifest = root / 'manifest.json'
            papers = [dict(id=str(i), sha256='source'+str(i), eligibleText=i < 20,
                           eligibility='Independent source inspection') for i in range(21)]
            manifest.write_text(json.dumps(dict(papers=papers)))
            reports = []
            for label in ('baseline', 'candidate'):
                folder = root / label
                folder.mkdir()
                cases = []
                for i in range(21):
                    document = folder / str(i) / 'auto/document.md'
                    document.parent.mkdir(parents=True)
                    text = i < 19
                    document.write_text('Recognized sentence' if text else '## Image frame 1\n![](<asset.png>)')
                    cases.append(dict(id=str(i), ocr='auto', passed=True, sourceSha256='source'+str(i),
                        markdownSha256=quality.digest(document), resourceUsage=dict(ocrRuntime=dict(
                            imageSources=1, imagesAttempted=1, imagesCompleted=1,
                            imagesWithText=int(text), imagesFailed=0, imagesSkipped=0))))
                report = folder / 'report.json'
                report.write_text(json.dumps(dict(complete=True, modes=['auto'],
                    manifestSha256=quality.digest(manifest), binarySha256=label, cases=cases)))
                reports.append(report)
            result = comparison.compare(manifest, *reports)
            self.assertTrue(result['passed'], result['failures'])
            self.assertEqual(result['eligibleImages'], 20)
            self.assertEqual(result['eligibleRecognized'], 19)
            self.assertEqual(result['excludedImages'], 1)
            self.assertEqual(result['eligibleSuccessRate'], .95)
            changed = json.loads(reports[1].read_text())
            document = reports[1].parent / '0/auto/document.md'
            document.write_text('Recognized nonsense')
            changed['cases'][0]['markdownSha256'] = quality.digest(document)
            reports[1].write_text(json.dumps(changed))
            review = comparison.compare(manifest, *reports)
            self.assertEqual(review['eligibleSuccessRate'], .95)
            self.assertFalse(review['passed'])
            self.assertTrue(any('source text changed' in error for error in review['failures']))
            changed = json.loads(reports[1].read_text())
            changed['cases'][0]['resourceUsage']['ocrRuntime']['imagesWithText'] = 0
            reports[1].write_text(json.dumps(changed))
            self.assertFalse(comparison.compare(manifest, *reports)['passed'])

    def test_image_assets_and_frame_headings_cannot_establish_ocr_text(self):
        self.assertEqual(comparison.words('<a id="image-frame-1"></a>\n## Image frame 1\n![](<asset.png>)'), [])


class QualityGateTests(unittest.TestCase):
    def check(self, text='Native body\nIMAGE WORDS', counters=None, reviews=(), expectation=None, diagnostics=(), review_policy='all-recovery'):
        with tempfile.TemporaryDirectory() as directory:
            document = pathlib.Path(directory) / 'document.md'
            document.write_text(text)
            usage = dict(imageSources=1, imagesAttempted=1, imagesCompleted=1,
                         imagesWithText=1, imagesFailed=0, imagesSkipped=0)
            usage.update(counters or {})
            case = dict(id='sample', ocr='auto', passed=True, sourceSha256='source',
                        markdownSha256=quality.digest(document), resourceUsage=dict(ocrRuntime=usage),
                        items=[dict(diagnostics=list(diagnostics))])
            policy = dict(sourceSha256='source', requiredText=['Native body', 'IMAGE WORDS'],
                          ocrMinimums=dict(imagesCompleted=1, imagesWithText=1),
                          ocrMaximums=dict(imagesSkipped=0, imagesFailed=0))
            policy.update(expectation or {})
            return quality.check_case(case, policy, document, reviews, 'binary', review_policy)

    def test_verified_content_and_completed_ocr_pass(self):
        self.assertEqual(self.check(), [])

    def test_source_content_policy_keeps_ocr_and_content_requirements(self):
        options = dict(expectation=dict(reviewPages=[1]), review_policy='source-content')
        self.assertEqual(self.check(**options), [])
        self.assertTrue(self.check(text='Native body', **options))
        self.assertTrue(self.check(counters=dict(imagesAttempted=0, imagesCompleted=0,
                                               imagesWithText=0, imagesSkipped=1), **options))

    def test_typographic_ligatures_preserve_words_and_spaces(self):
        expected = dict(requiredText=['fine tune'])
        self.assertEqual(self.check(text='ﬁne tune', expectation=expected), [])
        self.assertEqual(self.check(text='fine tune', expectation=dict(requiredText=['ﬁne tune'])), [])
        for text in ['finetune', 'fine tube', 'ﬁne tunes'.replace(' ', '')]:
            self.assertTrue(self.check(text=text, expectation=expected))

    def test_unicode_superscripts_preserve_numeric_relationships(self):
        expected = dict(requiredText=['10²'])
        self.assertEqual(self.check(text='10²', expectation=expected), [])
        self.assertTrue(self.check(text='102', expectation=expected))

    def test_script_markup_preserves_source_text_and_explicit_notation_checks(self):
        expected = dict(requiredText=['Author1'], requiredTextPatterns=[r'10<sup>4</sup>'])
        self.assertEqual(self.check(text='Author<sup>1</sup> 10<sup>4</sup>', expectation=expected), [])
        self.assertTrue(self.check(text='Author<sup>2</sup> 10<sup>4</sup>', expectation=expected))
        self.assertTrue(self.check(text='Author<sup>1</sup> 104', expectation=expected))

    def test_source_expression_preserves_math_operators_across_font_variants(self):
        pattern = r'\|[𝑖i]\s*−\s*[𝑗j]\s*\|\s*≥\s*[𝑤w]'
        for expression in ['|𝑖 − 𝑗 | ≥ 𝑤', '|i − j| ≥ w']:
            self.assertEqual(self.check(text='Native body IMAGE WORDS ' + expression,
                expectation=dict(requiredTextPatterns=[pattern])), [])
        for expression in ['|i + j| ≥ w', '|i − j| < w', '|i − j| ≥ v']:
            self.assertTrue(self.check(text='Native body IMAGE WORDS ' + expression,
                expectation=dict(requiredTextPatterns=[pattern])))

    def test_successful_exit_with_ocr_bypassed_fails(self):
        self.assertTrue(self.check(counters=dict(imagesAttempted=0, imagesCompleted=0,
                                                imagesWithText=0, imagesSkipped=1)))

    def test_duplicate_ocr_text_fails_even_with_successful_recognition(self):
        self.assertTrue(self.check(text='Native body\nIMAGE WORDS\nIMAGE WORDS',
            expectation={'exactTextCounts': {'IMAGE WORDS': 1}}))
        self.assertEqual(self.check(expectation={'exactTextCounts': {'IMAGE WORDS': 1}}), [])

    def test_empty_ocr_and_missing_image_words_fail(self):
        self.assertTrue(self.check(text='Native body', counters=dict(imagesWithText=0)))

    def test_native_words_cannot_mask_unavailable_ocr(self):
        self.assertTrue(self.check(counters=dict(imagesCompleted=0, imagesWithText=0, imagesFailed=1)))

    def test_counter_conservation_is_required(self):
        for field in quality.COUNTERS:
            usage = {name: 0 for name in quality.COUNTERS}
            usage[field] = 1
            self.assertTrue(quality.check_ocr(usage), field)
        self.assertTrue(quality.check_ocr({}))

    def test_old_visual_review_cannot_approve_changed_output(self):
        old = dict(id='sample', ocr='auto', sourceSha256='source', binarySha256='binary',
                   markdownSha256='old-output', page=1, verdict='pass', notes='source compared')
        self.assertTrue(self.check(reviews=[old], expectation=dict(reviewPages=[1])))

    def test_source_change_requires_new_expectation(self):
        self.assertTrue(self.check(expectation=dict(sourceSha256='different-source')))

    def test_each_recovery_image_requires_completed_ocr(self):
        diagnostics = [dict(code='pdf.recovery.pageImage', locator=dict(page=page)) for page in (1, 2)]
        errors = self.check(expectation=dict(requireRecoveryOcr=True), diagnostics=diagnostics)
        self.assertIn('recovery page images lack completed OCR', errors)

    def test_markdown_escaping_preserves_source_text_checks(self):
        self.assertEqual(self.check(text='Native body\nIMAGE WORDS\\.',
                                   expectation=dict(requiredText=['IMAGE WORDS.'])), [])

    def test_changed_asset_fails_even_when_markdown_is_unchanged(self):
        with tempfile.TemporaryDirectory() as directory:
            document = pathlib.Path(directory) / 'document.md'
            document.write_text('Native body')
            image = document.with_name('image.png')
            image.write_bytes(b'changed image')
            case = dict(id='sample', ocr='off', passed=True, sourceSha256='source',
                        markdownSha256=quality.digest(document), assets=[dict(path='image.png', sha256='old-image')])
            errors = quality.check_case(case, dict(sourceSha256='source', requiredText=['Native body']),
                                        document, [], 'binary')
            self.assertIn('delivered asset differs from conversion report: image.png', errors)



class ReplayReconciliationTests(unittest.TestCase):
    def test_replays_keep_executable_identity_and_cannot_hide_a_new_regression(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            base = dict(complete=True, binarySha256='old', manifestSha256='manifest',
                        cases=[dict(id='a', ocr='auto', sourceSha256='source', passed=True)])
            replay = {**base, 'binarySha256': 'new',
                      'cases': [{**base['cases'][0], 'passed': False}]}
            a, b = root/'base.json', root/'replay.json'
            a.write_text(json.dumps(base)); b.write_text(json.dumps(replay))
            result = reconciliation.reconcile(a, [b])
            self.assertIsNone(result['binarySha256'])
            self.assertEqual(result['failed'], 1)
            self.assertEqual(result['cases'][0]['binarySha256'], 'new')
            self.assertTrue(result['replacements'][0]['previousPassed'])
            for field, value in [('sourceSha256', 'different'), ('id', 'unknown')]:
                changed = {**replay, 'cases': [{**replay['cases'][0], field: value}]}
                b.write_text(json.dumps(changed))
                with self.assertRaises(ValueError): reconciliation.reconcile(a, [b])
            b.write_text(json.dumps({**replay, 'complete': False}))
            with self.assertRaises(ValueError): reconciliation.reconcile(a, [b])


    def test_partial_runs_require_full_manifest_coverage(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            manifest = root/'manifest.json'
            manifest.write_text(json.dumps({'papers': [dict(id=i, sha256=i) for i in ['a', 'b']]}))
            authority = quality.digest(manifest)
            base = dict(complete=False, binarySha256='old', manifestSha256=authority, modes=['auto'],
                        cases=[dict(id='a', ocr='auto', sourceSha256='a', passed=True)])
            replay = {**base, 'complete': True, 'binarySha256': 'new',
                      'cases': [dict(id='b', ocr='auto', sourceSha256='b', passed=True)]}
            a, b = root/'base.json', root/'replay.json'
            a.write_text(json.dumps(base)); b.write_text(json.dumps(replay))
            self.assertTrue(reconciliation.reconcile(a, [b], manifest)['complete'])
            with self.assertRaises(ValueError): reconciliation.reconcile(a, [b])
            replay['cases'] = []
            b.write_text(json.dumps(replay))
            self.assertFalse(reconciliation.reconcile(a, [b], manifest)['complete'])


    def test_same_executable_shards_retain_the_shared_authority(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            report = dict(complete=True, binarySha256='same', manifestSha256='manifest',
                          cases=[dict(id='a', ocr='auto', sourceSha256='source', passed=True)])
            a, b = root/'base.json', root/'replay.json'
            a.write_text(json.dumps(report)); b.write_text(json.dumps(report))
            result = reconciliation.reconcile(a, [b])
            self.assertEqual(result['binarySha256'], 'same')
            self.assertEqual(len(result['constituentRuns']), 2)
            self.assertTrue(result['complete'])


class CompactOcrModeTests(unittest.TestCase):
    def test_explicit_off_skips_are_separate_from_default_ocr(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory)/'report.json'
            common = dict(id='image', sourceSha256='source', passed=True, exitCode=0, seconds=1)
            auto = dict(imageSources=1, imagesAttempted=1, imagesCompleted=1,
                        imagesWithText=1, imagesFailed=0, imagesSkipped=0)
            off = dict(imageSources=1, imagesAttempted=0, imagesCompleted=0,
                       imagesWithText=0, imagesFailed=0, imagesSkipped=1)
            path.write_text(json.dumps(dict(binarySha256='binary', manifestSha256='manifest',
                modes=['auto','off'], complete=True, cases=[
                    dict(**common, ocr=mode, resourceUsage=dict(ocrRuntime=counts))
                    for mode,counts in [('auto',auto),('off',off)]])))
            result = compaction.compact(path)
            self.assertEqual(result['ocrTotalsByMode']['auto'], auto)
            self.assertEqual(result['ocrTotalsByMode']['off'], off)


if __name__ == "__main__":
    unittest.main()
