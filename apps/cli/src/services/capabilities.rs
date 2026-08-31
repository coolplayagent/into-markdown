//! Reachable optional services, selected after content detection and probing.
use super::*;
use into_markdown::InputFormat;

/// Optional capabilities that can actually be reached by the current input set.
/// Keeping this separate from installation state prevents an unrelated broken
/// plugin from blocking formats that do not use it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct InvocationCapabilities {
    pub(crate) ocr: bool,
    pub(crate) transcription: bool,
    pub(crate) diarization: bool,
    pub(crate) legacy_office: bool,
}

impl InvocationCapabilities {
    pub(crate) fn for_format(format: Option<InputFormat>, options: &ConversionOptions) -> Self {
        use InputFormat::*;
        // Archives can invoke any installed member converter recursively.
        if matches!(format, None | Some(Zip)) {
            return Self { ocr: true, transcription: true, diarization: true, legacy_office: true };
        }
        let legacy_office = matches!(format, Some(Doc | Ppt | Xls));
        let visual = matches!(
            format,
            Some(
                Pdf | Doc
                    | Docx
                    | Ppt
                    | Pptx
                    | Xls
                    | Xlsx
                    | Odt
                    | Ods
                    | Odp
                    | Epub
                    | Html
                    | Image
                    | OutlookMsg
                    | Feed
            )
        );
        let media = matches!(format, Some(Audio | Video | YouTube));
        let ocr_policy = effective_ocr_policy(options);
        Self {
            ocr: visual && !(ocr_policy == OcrPolicy::Auto && legacy_office),
            transcription: media,
            diarization: media,
            legacy_office,
        }
    }
}

pub(crate) fn effective_ocr_policy(options: &ConversionOptions) -> OcrPolicy {
    match options.ai.vision_ocr {
        AiMode::Only => OcrPolicy::Always,
        AiMode::Fallback | AiMode::Prefer if options.ocr.policy == OcrPolicy::Off => {
            OcrPolicy::Auto
        }
        _ => options.ocr.policy,
    }
}

pub(super) fn assemble_at(
    loaded: &LoadedConfig,
    execution: &ExecutionOptions,
    cwd: &Path,
    needs: InvocationCapabilities,
) -> Result<Services, CliError> {
    let context = ExecutionContext::new(execution.clone(), loaded.options.limits.clone());
    assemble_with_context(loaded, &context, cwd, needs)
}

pub(crate) fn assemble_with_context(
    loaded: &LoadedConfig,
    context: &ExecutionContext,
    cwd: &Path,
    needs: InvocationCapabilities,
) -> Result<Services, CliError> {
    context.checkpoint().map_err(CliError::from)?;
    let mut services = Services::default();
    if needs.ocr
        && (loaded.options.ocr.policy != OcrPolicy::Off
            || loaded.options.ai.vision_ocr != AiMode::Off
            || configured_route_is_active(&loaded.effective.capability_routes.ocr))
    {
        match assemble_ocr(loaded, context, cwd) {
            Ok(engine) => services.ocr = Some(engine),
            Err(error) if can_degrade_ocr(loaded.options.ocr.policy, &error) => {}
            Err(error) => return Err(CliError::from(error)),
        }
    }
    if ai_provider_service_enabled(&loaded.options) {
        services.ai = assemble_ai_provider(loaded)?;
    }
    if needs.transcription
        && (loaded.options.ai.audio_transcription != AiMode::Off
            || configured_route_is_active(&loaded.effective.capability_routes.transcription)
            || loaded
                .effective
                .plugins
                .get("official.media.whisper")
                .is_some_and(|plugin| plugin.enabled))
    {
        services.transcriber = Some(assemble_asr(loaded, context, cwd)?);
    }
    if needs.diarization && loaded.options.diarization.enabled {
        services.diarizer = Some(
            assemble_diarization_config(loaded, &loaded.options, context, cwd)
                .map_err(CliError::from)?,
        );
    }
    Ok(services)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn services_follow_selected_format_and_archive_members() {
        let options = ConversionOptions::default();
        let text = InvocationCapabilities::for_format(Some(InputFormat::Markdown), &options);
        assert_eq!(text, InvocationCapabilities::default());
        let image = InvocationCapabilities::for_format(Some(InputFormat::Image), &options);
        assert!(image.ocr && !image.transcription);
        let audio = InvocationCapabilities::for_format(Some(InputFormat::Audio), &options);
        assert!(audio.transcription && audio.diarization && !audio.ocr);
        let office = InvocationCapabilities::for_format(Some(InputFormat::Ppt), &options);
        assert!(office.legacy_office && !office.ocr);
        let zip = InvocationCapabilities::for_format(Some(InputFormat::Zip), &options);
        assert!(zip.ocr && zip.transcription && zip.diarization && zip.legacy_office);
    }
}
