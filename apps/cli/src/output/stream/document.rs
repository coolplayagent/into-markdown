//! Incremental semantic-document JSON and final inventories.

use super::*;

impl StructuredSpool {
    pub(super) fn write_document_event_inner(
        &mut self,
        event: &DocumentStreamEvent<'_>,
    ) -> Result<(), CliError> {
        self.context.checkpoint().map_err(CliError::from)?;
        match (self.document_phase, event) {
            (Some(DocumentPhase::AwaitMetadata), DocumentStreamEvent::Metadata(metadata)) => {
                self.write_ir_bytes(b"{\n  \"schemaVersion\": 1,\n  \"metadata\": ")?;
                self.write_ir_value(metadata, b"  ")?;
                self.write_ir_bytes(b",\n  \"blocks\": [")?;
                self.document_phase = Some(DocumentPhase::Blocks);
                Ok(())
            }
            (Some(DocumentPhase::Blocks), DocumentStreamEvent::RootBlock(block)) => {
                if self.first_block {
                    self.write_ir_bytes(b"\n    ")?;
                    self.first_block = false;
                } else {
                    self.write_ir_bytes(b",\n    ")?;
                }
                self.write_ir_value(block, b"    ")
            }
            (Some(DocumentPhase::AwaitMetadata), _) => {
                Err(CliError::internal("document block arrived before metadata"))
            }
            (Some(DocumentPhase::Blocks), DocumentStreamEvent::Metadata(_)) => {
                Err(CliError::internal("document metadata was repeated"))
            }
            (Some(DocumentPhase::Finished), _) => {
                Err(CliError::internal("document event arrived after finalization"))
            }
            (None, _) => Err(CliError::internal(
                "semantic event arrived without a document IR representation",
            )),
        }
    }

    pub(super) fn finish_document_inner(
        &mut self,
        diagnostics: &[into_markdown::Diagnostic],
        provenance: &[into_markdown::Provenance],
    ) -> Result<(), CliError> {
        if self.document_phase != Some(DocumentPhase::Blocks) {
            return Err(CliError::internal("semantic document is not ready to finalize"));
        }
        if self.first_block {
            self.write_ir_bytes(b"]\n}\n")?;
        } else {
            self.write_ir_bytes(b"\n  ]\n}\n")?;
        }
        match (self.diagnostics.as_mut(), self.provenance.as_mut()) {
            (Some(diagnostics_spool), Some(provenance_spool)) if self.plan.inventories() => {
                DiagnosticsDto::write_bundle_json_from_diagnostics(
                    diagnostics,
                    &mut *diagnostics_spool,
                )
                .map_err(|error| {
                    CliError::internal(format!("serialize diagnostics DTO: {error}"))
                })?;
                diagnostics_spool.write_all_checked(b"\n").map_err(CliError::from)?;
                ProvenanceListDto::write_bundle_json_from_provenance(
                    provenance,
                    &mut *provenance_spool,
                )
                .map_err(|error| {
                    CliError::internal(format!("serialize provenance DTO: {error}"))
                })?;
                provenance_spool.write_all_checked(b"\n").map_err(CliError::from)?;
            }
            (None, None) if !self.plan.inventories() => {}
            _ => return Err(CliError::internal("semantic inventory representation is incomplete")),
        }
        self.document_phase = Some(DocumentPhase::Finished);
        Ok(())
    }

    fn write_ir_bytes(&mut self, bytes: &[u8]) -> Result<(), CliError> {
        let destination = self
            .ir
            .as_mut()
            .ok_or_else(|| CliError::internal("document IR representation is absent"))?;
        let lease = self
            .ir_chunk_lease
            .as_mut()
            .ok_or_else(|| CliError::internal("document IR chunk lease is absent"))?;
        let mut writer =
            ChunkRecordingWriter { destination, chunks: &mut self.ir_write_chunks, lease };
        writer.write_all(bytes).map_err(CliError::from)
    }

    fn write_ir_value<T: Serialize + ?Sized>(
        &mut self,
        value: &T,
        indent: &'static [u8],
    ) -> Result<(), CliError> {
        let destination = self
            .ir
            .as_mut()
            .ok_or_else(|| CliError::internal("document IR representation is absent"))?;
        let lease = self
            .ir_chunk_lease
            .as_mut()
            .ok_or_else(|| CliError::internal("document IR chunk lease is absent"))?;
        let writer = ChunkRecordingWriter { destination, chunks: &mut self.ir_write_chunks, lease };
        serde_json::to_writer_pretty(IndentingWriter::new(writer, indent), value)
            .map_err(|error| map_json_error(&error, "serialize document IR event"))
    }
}
