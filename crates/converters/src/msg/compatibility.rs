use super::ole::CompoundRecovery;
use into_markdown_core::{Diagnostic, DiagnosticSeverity, SourceLocator};

pub(super) fn compound_diagnostic(recovery: CompoundRecovery) -> Diagnostic {
    let (code, message, part) = match recovery {
        CompoundRecovery::StorageStreamMetadata => (
            "msg.cfb.storageMetadataIgnored",
            "stream-only metadata on a storage directory entry was ignored",
            "cfb/directory",
        ),
        CompoundRecovery::TrailingFileBytes => (
            "msg.cfb.trailingBytesIgnored",
            "unaddressable bytes after the final complete CFB sector were ignored",
            "cfb/header",
        ),
        CompoundRecovery::FatSectorMarker => (
            "msg.cfb.fatMarkerRecovered",
            "a FAT sector marker disagreed with the bounded DIFAT and was recovered",
            "cfb/fat",
        ),
        CompoundRecovery::UnreachableFatTarget => (
            "msg.cfb.unreachableFatTargetIgnored",
            "an out-of-bounds FAT target belonging to no reachable chain was ignored",
            "cfb/fat",
        ),
        CompoundRecovery::DirectoryNameTerminator => (
            "msg.cfb.directoryNameRecovered",
            "a directory name used a non-canonical but unambiguous NUL terminator",
            "cfb/directory",
        ),
        CompoundRecovery::RootStorageName => (
            "msg.cfb.rootNameRecovered",
            "the type-5 root storage used a non-canonical display name",
            "cfb/directory",
        ),
        CompoundRecovery::StreamChainTail => (
            "msg.cfb.streamTailIgnored",
            "a stale allocation pointer after the declared end of a complete stream was ignored",
            "cfb/stream",
        ),
        CompoundRecovery::PartialStreamSector => (
            "msg.cfb.partialStreamSectorRecovered",
            "the available prefix of a terminal partial sector satisfied the declared stream size",
            "cfb/stream",
        ),
    };
    Diagnostic {
        code: code.into(),
        severity: DiagnosticSeverity::Warning,
        message: message.into(),
        locator: Some(SourceLocator { part: Some(part.into()), ..SourceLocator::default() }),
    }
}
