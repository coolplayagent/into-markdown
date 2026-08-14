thread_local! {
    pub(super) static PART_MATERIALIZATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
