use super::directory::*;
use super::ownership::*;
use super::stream::*;
use super::*;
use into_markdown_core::ErrorCode;
use std::cell::Cell;

const XLS: &[u8] = include_bytes!("../../../../../tools/macos-release/fixtures/normal.xls");

fn stream(size: u64) -> DirectoryEntry {
    DirectoryEntry {
        name: "stream".into(),
        kind: EntryKind::Stream,
        left: NONE,
        right: NONE,
        child: NONE,
        start: 0,
        size,
        parent: None,
    }
}

struct RejectExpandedBudget {
    expanded_calls: Vec<u64>,
    context: into_markdown_core::ExecutionContext,
}

impl CompoundBudget for RejectExpandedBudget {
    fn cfb_memory(&self, bytes: u64) -> Result<ResourceReservation, ConversionError> {
        self.context.reserve_memory(bytes)
    }

    fn cfb_entry(&mut self) -> Result<(), ConversionError> {
        Ok(())
    }

    fn cfb_expanded(&mut self, bytes: u64) -> Result<(), ConversionError> {
        self.expanded_calls.push(bytes);
        Err(limit("max_decompressed_bytes", "test budget rejected stream"))
    }

    fn cfb_depth(&self, _depth: u16, _part: &str) -> Result<(), ConversionError> {
        Ok(())
    }

    fn cfb_work(&mut self, _units: u64) -> Result<(), ConversionError> {
        Ok(())
    }
}

struct OpenBudget {
    context: into_markdown_core::ExecutionContext,
    peak: Cell<u64>,
}

struct WorkBudget {
    context: into_markdown_core::ExecutionContext,
    work: u64,
}

impl CompoundBudget for WorkBudget {
    fn cfb_memory(&self, bytes: u64) -> Result<ResourceReservation, ConversionError> {
        self.context.reserve_memory(bytes)
    }

    fn cfb_entry(&mut self) -> Result<(), ConversionError> {
        Ok(())
    }

    fn cfb_expanded(&mut self, _bytes: u64) -> Result<(), ConversionError> {
        Ok(())
    }

    fn cfb_depth(&self, _depth: u16, _part: &str) -> Result<(), ConversionError> {
        Ok(())
    }

    fn cfb_work(&mut self, units: u64) -> Result<(), ConversionError> {
        self.work = self.work.checked_add(units).unwrap();
        Ok(())
    }
}

impl CompoundBudget for OpenBudget {
    fn cfb_memory(&self, bytes: u64) -> Result<ResourceReservation, ConversionError> {
        let reservation = self.context.reserve_memory(bytes)?;
        self.peak.set(self.peak.get().max(self.context.reserved_memory_bytes()));
        Ok(reservation)
    }

    fn cfb_entry(&mut self) -> Result<(), ConversionError> {
        Ok(())
    }

    fn cfb_expanded(&mut self, _bytes: u64) -> Result<(), ConversionError> {
        Ok(())
    }

    fn cfb_depth(&self, _depth: u16, _part: &str) -> Result<(), ConversionError> {
        Ok(())
    }

    fn cfb_work(&mut self, _units: u64) -> Result<(), ConversionError> {
        Ok(())
    }
}

fn open_budget(max_memory_bytes: u64) -> OpenBudget {
    OpenBudget {
        context: into_markdown_core::ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits {
                max_memory_bytes,
                ..into_markdown_core::ResourceLimits::default()
            },
        ),
        peak: Cell::new(0),
    }
}

fn directory_name_raw(name: &str, declared_units: usize) -> [u8; 128] {
    let mut raw = [0_u8; 128];
    for (index, unit) in name.encode_utf16().chain([0]).enumerate() {
        raw[index * 2..index * 2 + 2].copy_from_slice(&unit.to_le_bytes());
    }
    raw[64..66].copy_from_slice(&u16::try_from(declared_units * 2).unwrap().to_le_bytes());
    raw
}

#[test]
fn best_effort_directory_name_recovery_requires_zero_redundancy() {
    let mut recoveries = CompoundRecoveries::default();
    let zero_redundancy = directory_name_raw("Workbook", 12);
    assert_eq!(
        parse_directory_name(
            &zero_redundancy,
            CompoundCompatibility::LegacyOfficeBestEffort,
            &mut recoveries,
        )
        .unwrap(),
        "Workbook"
    );

    let mut disguised = directory_name_raw("Workbook", 12);
    disguised[18..26].copy_from_slice(&[b'E', 0, b'v', 0, b'i', 0, b'l', 0]);
    assert!(
        parse_directory_name(
            &disguised,
            CompoundCompatibility::LegacyOfficeBestEffort,
            &mut CompoundRecoveries::default(),
        )
        .is_err()
    );

    let mut book_alias = directory_name_raw("Book", 8);
    book_alias[10..16].copy_from_slice(&[b'E', 0, b'v', 0, b'l', 0]);
    assert!(
        parse_directory_name(
            &book_alias,
            CompoundCompatibility::LegacyOfficeBestEffort,
            &mut CompoundRecoveries::default(),
        )
        .is_err()
    );
}

#[test]
fn regular_stream_budget_is_checked_before_materialization() {
    let mut budget = RejectExpandedBudget {
        expanded_calls: Vec::new(),
        context: into_markdown_core::ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits::default(),
        ),
    };
    let declared = 64 * 1024 * 1024;
    let error = materialize_regular_stream(&[], 512, &[0], declared, None, "cfb/root", &mut budget)
        .unwrap_err();

    assert_eq!(error.code(), ErrorCode::ResourceLimit);
    assert_eq!(budget.expanded_calls, vec![declared]);
}

#[test]
fn high_cardinality_directory_name_validation_has_deterministic_bounded_work() {
    const COUNT: usize = 10_000;
    let entries = (0..COUNT)
        .map(|index| DirectoryEntry {
            name: format!("stream-{index:05}"),
            parent: Some(0),
            ..stream(0)
        })
        .collect::<Vec<_>>();
    let context = into_markdown_core::ExecutionContext::new(
        into_markdown_core::ExecutionOptions::default(),
        into_markdown_core::ResourceLimits::default(),
    );
    let mut budget = WorkBudget { context, work: 0 };
    validate_unique_names(&entries, &mut budget).unwrap();

    let levels = usize::BITS - COUNT.saturating_sub(1).leading_zeros();
    let expected = u64::try_from(COUNT).unwrap() * (u64::from(levels) + 1);
    assert_eq!(budget.work, expected);

    let mut duplicate = entries;
    duplicate[COUNT - 1].name = duplicate[0].name.to_ascii_uppercase();
    budget.work = 0;
    assert!(matches!(
        validate_unique_names(&duplicate, &mut budget),
        Err(ConversionError::Malformed { .. })
    ));
    assert_eq!(budget.work, u64::try_from(COUNT).unwrap() * u64::from(levels));
}

#[test]
fn compound_open_has_exact_incremental_boundary_and_releases_on_drop() {
    for compatibility in
        [CompoundCompatibility::Strict, CompoundCompatibility::LegacyOfficeBestEffort]
    {
        let mut measuring = open_budget(u64::MAX);
        let compound =
            CompoundFile::open_with_compatibility(XLS, &mut measuring, compatibility).unwrap();
        let peak = measuring.peak.get();
        assert!(peak > 0);
        drop(compound);
        assert_eq!(measuring.context.reserved_memory_bytes(), 0);

        let mut exact = open_budget(peak);
        let compound =
            CompoundFile::open_with_compatibility(XLS, &mut exact, compatibility).unwrap();
        drop(compound);
        assert_eq!(exact.context.reserved_memory_bytes(), 0);
        assert!(exact.context.reserve_memory(peak).is_ok());

        let mut below = open_budget(peak - 1);
        let error =
            CompoundFile::open_with_compatibility(XLS, &mut below, compatibility).unwrap_err();
        assert_eq!(error.code(), ErrorCode::ResourceLimit);
        assert_eq!(below.context.reserved_memory_bytes(), 0);
    }
}

#[test]
fn compound_open_observes_cancellation_without_leaking() {
    for compatibility in
        [CompoundCompatibility::Strict, CompoundCompatibility::LegacyOfficeBestEffort]
    {
        let cancellation = into_markdown_core::CancellationToken::new();
        let context = into_markdown_core::ExecutionContext::new(
            into_markdown_core::ExecutionOptions {
                cancellation: cancellation.clone(),
                ..into_markdown_core::ExecutionOptions::default()
            },
            into_markdown_core::ResourceLimits::default(),
        );
        cancellation.cancel();
        let mut budget = OpenBudget { context, peak: Cell::new(0) };
        let error =
            CompoundFile::open_with_compatibility(XLS, &mut budget, compatibility).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Cancelled);
        assert_eq!(budget.context.reserved_memory_bytes(), 0);
    }
}

#[test]
fn mini_stream_reader_rejects_a_chain_longer_than_declared_data() {
    let mut owners = vec![false, false];
    let mut pending_tails = Vec::new();
    let mut context = MiniStreamContext {
        minifat: &[1, END],
        root: &[0; 128],
        owners: &mut owners,
        mini_size: 64,
        compatibility: CompoundCompatibility::Strict,
        pending_tails: &mut pending_tails,
    };
    let error = read_mini_stream(&stream(1), "mini", &mut context).unwrap_err();
    assert_eq!(error.code(), ErrorCode::Malformed);
}

#[test]
fn regular_stream_reader_rejects_a_chain_longer_than_declared_data() {
    let error = regular_stream_chain(
        &stream(1),
        &[1, END],
        2,
        512,
        "regular",
        CompoundCompatibility::Strict,
    )
    .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Malformed);
}

#[test]
fn declared_tail_target_must_be_unowned_and_free() {
    assert!(validate_pending_tails(&[1], &[true, true, false], &[END, END, FREE], "tail").is_err());
    assert!(
        validate_pending_tails(&[2], &[true, false, false], &[END, FREE, END], "tail").is_err()
    );
    validate_pending_tails(&[2], &[true, false, false], &[END, END, FREE], "tail").unwrap();
}

#[test]
fn partial_terminal_sector_uses_only_the_declared_stream_prefix() {
    let mut bytes = vec![0; 2 * 512];
    bytes.extend_from_slice(&[1, 2, 3]);
    let (stream, recovered) =
        concatenate_regular_stream(&bytes, 512, &[0, 1], 515, Some(1), "xls/Workbook").unwrap();
    assert_eq!(stream.len(), 515);
    assert_eq!(&stream[512..], &[1, 2, 3]);
    assert_eq!(recovered, Some(true));

    let mut bytes_with_trailer = bytes.clone();
    bytes_with_trailer.extend_from_slice(&[4, 5]);
    let (stream, recovered) =
        concatenate_regular_stream(&bytes_with_trailer, 512, &[0, 1], 515, Some(1), "xls/Workbook")
            .unwrap();
    assert_eq!(&stream[512..], &[1, 2, 3]);
    assert_eq!(recovered, Some(false));

    assert!(
        concatenate_regular_stream(&bytes, 512, &[0, 1], 516, Some(1), "xls/Workbook").is_err()
    );
}
