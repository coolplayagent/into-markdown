use super::*;

impl Engine {
    pub(crate) async fn convert_and_enrich(
        &self,
        attempt: &Attempt,
        input: &into_markdown_core::ResolvedInput,
        options: &ConversionOptions,
        context: &ExecutionContext,
        consumer: StreamConsumerKind,
    ) -> Result<ConverterOutput, ConversionError> {
        if let Some(error) = &attempt.probe_error {
            return source_recovery::converter_failure(
                input,
                options,
                context,
                error.clone(),
                attempt.candidate.format,
            );
        }
        let native_stream = attempt.converter.stream_support().filter(|stream| {
            stream.stream_mode_for(input, &attempt.candidate, options, consumer)
                == ConverterStreamMode::Native
        });
        let converted = async {
            Ok::<_, ConversionError>(if let Some(stream) = native_stream {
                let native = match consumer {
                    StreamConsumerKind::Collecting => {
                        stream_execution::invoke_native_collecting(
                            stream,
                            input,
                            &attempt.candidate,
                            options,
                            &self.services,
                            &self.enrichers,
                            context,
                        )
                        .await?
                    }
                    StreamConsumerKind::Immediate => {
                        stream_execution::invoke_native_immediate(
                            stream,
                            input,
                            &attempt.candidate,
                            options,
                            &self.services,
                            &self.enrichers,
                            context,
                        )
                        .await?
                    }
                };
                (native.output, native.completed_page_ocr)
            } else {
                (
                    invoke_converter_preflighted(
                        attempt.converter.as_ref(),
                        input,
                        &attempt.candidate,
                        options,
                        &self.services,
                        context,
                        |_| Ok(()),
                    )
                    .await?,
                    false,
                )
            })
        }
        .await;
        let (output, completed_page_ocr) = match converted {
            Ok(pair) => pair,
            Err(error) => {
                return source_recovery::converter_failure(
                    input,
                    options,
                    context,
                    error,
                    attempt.candidate.format,
                );
            }
        };
        let output = source_recovery::retain_empty_source(
            input,
            options,
            context,
            output,
            attempt.candidate.format,
        )?;
        invoke_enrichers_skipping(
            &self.enrichers,
            output,
            EnricherInvocation::after_page_enrichment(
                attempt.converter.id(),
                attempt.candidate.format,
                options,
                &self.services,
                context,
                completed_page_ocr.then_some(page_enrichment::EMBEDDED_OCR),
            ),
        )
        .await
    }
}
