#[unsafe(no_mangle)]
pub extern "C" fn into_markdown_allocator_bridge() -> usize {
    let values = Vec::from([2_usize, 3, 5, 7]);
    values.into_iter().sum()
}
