//! Repository-level license inventory test.

#[test]
fn repository_license_inventory_is_consistent() {
    if let Err(errors) = license_check::run(false) {
        panic!("license audit failed:\n{}", errors.join("\n"));
    }
}
