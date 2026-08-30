use super::*;

const PROCESS_HELPER_ENV: &str = "INTO_MD_TRANSACTION_PROCESS_HELPER";
const PROCESS_HELPER_ROOT_ENV: &str = "INTO_MD_TRANSACTION_PROCESS_ROOT";
const PROCESS_HELPER_TARGET_ENV: &str = "INTO_MD_TRANSACTION_PROCESS_TARGET";
const PROCESS_HELPER_TEST: &str = "transaction::tests::process_helper::transaction_process_helper";

pub(super) fn spawn_process_helper(
    mode: &str,
    root: &Path,
    target: Option<&Path>,
) -> std::process::Child {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .arg("--exact")
        .arg(PROCESS_HELPER_TEST)
        .arg("--nocapture")
        .env(PROCESS_HELPER_ENV, mode)
        .env(PROCESS_HELPER_ROOT_ENV, root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(target) = target {
        command.env(PROCESS_HELPER_TARGET_ENV, target);
    }
    command.spawn().unwrap()
}

pub(super) fn wait_for_process_signal(reader: &mut impl std::io::BufRead, expected: &str) {
    loop {
        let mut signal = String::new();
        assert_ne!(reader.read_line(&mut signal).unwrap(), 0, "helper closed before {expected}");
        if signal.trim() == expected {
            return;
        }
    }
}

pub(super) fn continue_process(child: &mut std::process::Child) {
    let stdin = child.stdin.as_mut().expect("helper stdin");
    stdin.write_all(b"CONTINUE\n").unwrap();
    stdin.flush().unwrap();
}

fn announce(signal: &str) {
    println!("{signal}");
    std::io::stdout().flush().unwrap();
}

fn wait_for_continue() {
    let mut command = String::new();
    std::io::stdin().read_line(&mut command).unwrap();
    assert_eq!(command.trim(), "CONTINUE");
}

fn registry_waiter(root: &Path) {
    let root_handle = SafeDir::open_absolute(root).unwrap();
    let mut announced = false;
    let epoch =
        super::super::registry::lock_registry_epoch_with_observer(&root_handle, true, |_| {
            if !announced {
                announced = true;
                announce("READY");
                wait_for_continue();
            }
        })
        .unwrap()
        .unwrap();
    announce("ACQUIRED");
    assert!(epoch.try_cleanup().unwrap());
}

fn prepared_owner(target: PathBuf) {
    let transaction =
        prepare(&[Target { path: target, bytes: b"child" }], true, &context()).unwrap();
    announce("READY");
    wait_for_continue();
    transaction.commit().unwrap();
}

fn commit_target_installed(root: &Path) {
    let targets = [
        Target { path: root.join("commit-first.md"), bytes: b"new-first" },
        Target { path: root.join("commit-second.md"), bytes: b"new-second" },
    ];
    let mut transaction = prepare(&targets, true, &context()).unwrap();
    let mut announced = false;
    transaction
        .commit_with_hook(|phase, index| {
            if phase == "targetInstalled" && index == 0 && !announced {
                announced = true;
                announce("READY");
                wait_for_continue();
            }
            Ok(HookDecision::Continue)
        })
        .unwrap();
}

fn expect_busy(target: PathBuf) {
    let error = prepare(&[Target { path: target, bytes: b"contender" }], true, &context())
        .err()
        .expect("same-parent contender must not prepare");
    assert_eq!(error.code(), "transactionBusy");
    announce("BUSY");
}

#[test]
fn transaction_process_helper() {
    let Ok(mode) = std::env::var(PROCESS_HELPER_ENV) else { return };
    let root = PathBuf::from(std::env::var_os(PROCESS_HELPER_ROOT_ENV).unwrap());
    match mode.as_str() {
        "registry-waiter" => registry_waiter(&root),
        "prepared-owner" => prepared_owner(process_target()),
        "commit-target-installed" => commit_target_installed(&root),
        "expect-busy" => expect_busy(process_target()),
        _ => panic!("unknown transaction process helper mode"),
    }
}

fn process_target() -> PathBuf {
    PathBuf::from(std::env::var_os(PROCESS_HELPER_TARGET_ENV).unwrap())
}
