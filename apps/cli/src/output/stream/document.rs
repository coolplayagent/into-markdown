//! Incremental semantic-document JSON and final inventories.

use super::*;

impl StructuredSpool {
    pub(super) fn write_document_event_inner(
        &mut self,
        event: &DocumentStreamEvent<'_>,
    ) -> Result<(), CliError> {
        self.context.checkpoint().map_err(CliError::from)?;
        match (self.document_phase, event) {
            (DocumentPhase::AwaitMetadata, DocumentStreamEvent::Metadata(metadata)) => {
                self.write_ir_bytes(b"{\n  \"schemaVersion\": 1,\n  \"metadata\": ")?;
                self.write_ir_value(metadata, b"  ")?;
                self.write_ir_bytes(b",\n  \"blocks\": [")?;
                self.document_phase = DocumentPhase::Blocks;
                Ok(())
            }
            (DocumentPhase::Blocks, DocumentStreamEvent::RootBlock(block)) => {
                if self.first_block {
                    self.write_ir_bytes(b"\n    ")?;
                    self.first_block = false;
                } else {
                    self.write_ir_bytes(b",\n    ")?;
                }
                self.write_ir_value(block, b"    ")
            }
            (DocumentPhase::AwaitMetadata, _) => {
                Err(CliError::internal("document block arrived before metadata"))
            }
            (DocumentPhase::Blocks, DocumentStreamEvent::Metadata(_)) => {
                Err(CliError::internal("document metadata was repeated"))
            }
            (DocumentPhase::Finished, _) => {
                Err(CliError::internal("document event arrived after finalization"))
            }
        }
    }

    pub(super) fn finish_document_inner(
        &mut self,
        diagnostics: &[into_markdown::Diagnostic],
        provenance: &[into_markdown::Provenance],
    ) -> Result<(), CliError> {
        if self.document_phase != DocumentPhase::Blocks {
            return Err(CliError::internal("semantic document is not ready to finalize"));
        }
        if self.first_block {
            self.write_ir_bytes(b"]\n}\n")?;
        } else {
            self.write_ir_bytes(b"\n  ]\n}\n")?;
        }
        DiagnosticsDto::write_bundle_json_from_diagnostics(diagnostics, &mut self.diagnostics)
            .map_err(|error| CliError::internal(format!("serialize diagnostics DTO: {error}")))?;
        self.diagnostics.write_all_checked(b"\n").map_err(CliError::from)?;
        ProvenanceListDto::write_bundle_json_from_provenance(provenance, &mut self.provenance)
            .map_err(|error| CliError::internal(format!("serialize provenance DTO: {error}")))?;
        self.provenance.write_all_checked(b"\n").map_err(CliError::from)?;
        self.document_phase = DocumentPhase::Finished;
        Ok(())
    }

    fn write_ir_bytes(&mut self, bytes: &[u8]) -> Result<(), CliError> {
        let mut writer = ChunkRecordingWriter {
            destination: &mut self.ir,
            chunks: &mut self.ir_write_chunks,
            lease: &mut self._ir_chunk_lease,
        };
        writer.write_all(bytes).map_err(CliError::from)
    }

    fn write_ir_value<T: Serialize + ?Sized>(
        &mut self,
        value: &T,
        indent: &'static [u8],
    ) -> Result<(), CliError> {
        let writer = ChunkRecordingWriter {
            destination: &mut self.ir,
            chunks: &mut self.ir_write_chunks,
            lease: &mut self._ir_chunk_lease,
        };
        serde_json::to_writer_pretty(IndentingWriter::new(writer, indent), value)
            .map_err(|error| map_json_error(&error, "serialize document IR event"))
    }
}
