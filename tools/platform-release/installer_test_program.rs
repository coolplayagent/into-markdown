//! Tiny test-only executable used to exercise the native install transaction.

fn main() {
    let first = std::env::args_os().nth(1);
    if first.as_deref().is_some_and(|value| std::path::Path::new(value).is_dir()) {
        let root = first.expect("archive root");
        if std::path::Path::new(&root).join("reject-install").exists() {
            std::process::exit(23);
        }
        return;
    }
    println!("fixture:{}", std::env::args().skip(1).collect::<Vec<_>>().join("|"));
}
