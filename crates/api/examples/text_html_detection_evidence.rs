//! Emit repeatable candidate, probe, route, and effective-option evidence for a pinned corpus.
use into_markdown::*;
use into_markdown_converters::{
    DelimitedTextConverter, FeedConverter, HtmlConverter, MarkdownConverter, NotebookConverter,
    StructuredDataConverter, TextConverter,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    future::Future,
    path::Path,
    sync::Arc,
    task::{Context, Poll, Waker},
};

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(result) => return result,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

fn inspect(bytes: Vec<u8>, name: &str) -> Value {
    let engine = default_engine().unwrap();
    let input = InputRef::bytes(bytes.clone(), Some(name.to_owned()));
    let mut request = ConversionRequest::new(input.clone());
    request.options.ocr.policy = OcrPolicy::Off;
    let context = ExecutionContext::new(request.execution.clone(), request.options.limits.clone());
    let options = json!(request.options);
    let detection = block_on(engine.detect(DetectionRequest {
        input,
        hint: request.hint.clone(),
        options: request.options.clone(),
        execution: request.execution.clone(),
    }));
    let Ok(detection) = detection else {
        let error = detection.unwrap_err();
        return json!({"options": options, "errorCode": error.code().as_str(), "error": error.to_string(), "probes": []});
    };
    let source = ResolvedInput { bytes: Arc::from(bytes), metadata: detection.source };
    let probes = probe_candidates(&source, &detection.candidates, &context);
    let converted = block_on(engine.convert_with_context(request, context.clone()));
    let result = match converted {
        Ok(output) => {
            output.document.validate().unwrap();
            json!({"validIr": true, "markdownBytes": output.markdown.len(), "markdownSha256": format!("{:x}", Sha256::digest(output.markdown.as_bytes())), "diagnostics": output.diagnostics, "providers": output.document.blocks.iter().map(|block| block.provenance.provider.as_str()).collect::<std::collections::BTreeSet<_>>()})
        }
        Err(error) => json!({"errorCode": error.code().as_str(), "error": error.to_string()}),
    };
    json!({"options": options, "candidates": detection.candidates, "probes": probes, "selectedFormat": context.detected_format(), "result": result})
}

fn probe_candidates(
    source: &ResolvedInput,
    candidates: &[FormatCandidate],
    context: &ExecutionContext,
) -> Vec<Value> {
    let converters: Vec<Box<dyn Converter>> = vec![
        Box::new(TextConverter),
        Box::new(StructuredDataConverter),
        Box::new(HtmlConverter),
        Box::new(MarkdownConverter),
        Box::new(DelimitedTextConverter),
        Box::new(NotebookConverter),
        Box::new(FeedConverter),
    ];
    let mut probes = Vec::new();
    for candidate in candidates {
        for converter in &converters {
            if !converter.supported_formats().contains(&candidate.format) {
                continue;
            }
            let result = match block_on(converter.probe(source, candidate, context)) {
                Ok(ProbeOutcome::NotApplicable) => json!({"outcome":"notApplicable"}),
                Ok(ProbeOutcome::Match { confidence }) => {
                    json!({"outcome":"match", "confidence":confidence})
                }
                Err(error) => {
                    json!({"errorCode": error.code().as_str(), "error":error.to_string()})
                }
            };
            probes.push(
                json!({"format":candidate.format, "converter":converter.id(), "probe":result}),
            );
        }
    }
    probes
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(args.len(), 4, "usage: text_html_detection_evidence MANIFEST CACHE REPORT");
    let manifest: Value = serde_json::from_slice(&std::fs::read(&args[1]).unwrap()).unwrap();
    let records: Vec<_> = manifest["samples"].as_array().unwrap().iter().map(|sample| {
        let path = sample["path"].as_str().unwrap();
        let file = Path::new(&args[2]).join(sample["kind"].as_str().unwrap()).join(path);
        json!({"kind":sample["kind"], "path":path, "sha256":sample["sha256"], "observation":inspect(std::fs::read(file).unwrap(), path)})
    }).collect();
    std::fs::write(
        &args[3],
        serde_json::to_vec_pretty(&json!({"schemaVersion":1,"records":records})).unwrap(),
    )
    .unwrap();
}
