use super::*;
use into_markdown_core::{ExecutionOptions, ResourceLimits};

fn target() -> Target {
    let name = current_target().unwrap();
    let (binary_format, architecture) = match name {
        "aarch64-apple-darwin" => ("mach-o", "aarch64"),
        "aarch64-unknown-linux-gnu" => ("elf", "aarch64"),
        "x86_64-unknown-linux-gnu" => ("elf", "x86_64"),
        "x86_64-pc-windows-msvc" => ("pe", "x86_64"),
        _ => unreachable!(),
    };
    Target {
        container: None,
        artifact_url: "https://example.invalid/runtime".into(),
        artifact_bytes: 1,
        artifact_sha256: "a".repeat(64),
        install_root: "runtime".into(),
        kit_library: "runtime/kit".into(),
        worker: "worker".into(),
        files: vec![RuntimeFile {
            path: "worker".into(),
            bytes: 1,
            sha256: "b".repeat(64),
            role: FileRole::Worker,
        }],
        licenses: vec![RuntimeLicense {
            id: "fixture".into(),
            spdx: Some("Apache-2.0".into()),
            notice_path: "LICENSE".into(),
            notice_sha256: "c".repeat(64),
        }],
        abi: Abi {
            binary_format: binary_format.into(),
            architecture: architecture.into(),
            library_identity: "kit".into(),
            required_export: "libreofficekit_hook_2".into(),
        },
        limits: WorkerLimits {
            address_space_overhead_bytes: 256 * 1024 * 1024,
            file_size_limit_bytes: 1024,
            open_file_limit: 64,
            process_limit: 1,
        },
        sandbox: SandboxAuthority {
            system_libraries: Vec::new(),
            network: "deny".into(),
            child_processes: "deny".into(),
            compatibility_child: None,
            app_container: if name == "x86_64-pc-windows-msvc" {
                Some(AppContainerAuthority {
                    profile_name: "into-markdown.legacy-office".into(),
                    sid: "S-1-15-2-1".into(),
                    capabilities: Vec::new(),
                    forbidden_capabilities: FORBIDDEN_WINDOWS_CAPABILITIES
                        .iter()
                        .map(ToString::to_string)
                        .collect(),
                })
            } else {
                None
            },
        },
    }
}

#[test]
fn target_policy_rejects_network_children_and_abi_confusion() {
    let name = current_target().unwrap();
    assert!(validate_target(&target(), name).is_ok());
    let mut candidate = target();
    candidate.sandbox.network = "allow".into();
    assert!(validate_target(&candidate, name).is_err());
    let mut candidate = target();
    candidate.limits.process_limit = 2;
    assert!(validate_target(&candidate, name).is_err());
    let mut candidate = target();
    candidate.abi.architecture = "wrong".into();
    assert!(validate_target(&candidate, name).is_err());
    let mut candidate = target();
    candidate.sandbox.system_libraries = vec![
        SystemLibraryAuthority { identity: "duplicate".into(), path: "/one".into() },
        SystemLibraryAuthority { identity: "duplicate".into(), path: "/two".into() },
    ];
    assert!(validate_target(&candidate, name).is_err());
    let mut candidate = target();
    candidate.sandbox.system_libraries = (0..129)
        .map(|index| SystemLibraryAuthority {
            identity: format!("identity-{index}"),
            path: format!("/system/library-{index}"),
        })
        .collect();
    assert!(validate_target(&candidate, name).is_err());
    let mut candidate = target();
    candidate.kit_library = "../kit".into();
    assert!(validate_target(&candidate, name).is_err());
    let mut candidate = target();
    if let Some(app_container) = &mut candidate.sandbox.app_container {
        app_container.capabilities.push("internetClient".into());
    } else {
        candidate.sandbox.app_container = Some(AppContainerAuthority {
            profile_name: "into-markdown.legacy-office".into(),
            sid: "S-1-15-2-1".into(),
            capabilities: Vec::new(),
            forbidden_capabilities: FORBIDDEN_WINDOWS_CAPABILITIES
                .iter()
                .map(ToString::to_string)
                .collect(),
        });
    }
    assert!(validate_target(&candidate, name).is_err());
}

#[test]
fn url_hash_path_and_system_allowlists_are_strict() {
    assert!(https_url("https://example.invalid/runtime"));
    assert!(!https_url("https://user@example.invalid/runtime"));
    assert!(!https_url("http://example.invalid/runtime"));
    assert!(is_sha256(&"f".repeat(64)));
    assert!(!is_sha256(&"F".repeat(64)));
    assert!(safe_relative("runtime/program/lib.so"));
    assert!(safe_relative("runtime/License (third-party).txt"));
    assert!(!safe_relative("runtime/../lib.so"));
    assert!(!safe_relative("runtime//lib.so"));
    assert!(!safe_relative("runtime/lib.so/"));
    assert!(!safe_relative("runtime/new\nline"));
    assert!(!safe_relative("/absolute"));
    let fake = SystemLibraryAuthority {
        identity: "libconstructor-canary.so.6".into(),
        path: "/usr/lib/libconstructor-canary.so.6".into(),
    };
    assert!(system_library_path(&fake, current_target().unwrap()).is_err());
}

#[test]
fn memory_limit_fails_before_untrusted_authority_is_parsed() {
    let root = tempfile::tempdir().unwrap();
    let root_path = root.path().canonicalize().unwrap();
    let authority = root_path.join("authority.json");
    std::fs::write(&authority, vec![b'!'; 1024]).unwrap();
    let context = ExecutionContext::new(
        ExecutionOptions::default(),
        ResourceLimits { max_memory_bytes: 16 * 1024 - 1, ..ResourceLimits::default() },
    );
    let Err(error) = verify(
        &RuntimeConfig::new(authority, root_path.clone(), root_path.join("worker")),
        &context,
    ) else {
        panic!("low-memory authority unexpectedly succeeded");
    };
    assert!(matches!(error, ConversionError::ResourceLimit { limit: "max_memory_bytes", .. }));
    assert_eq!(context.reserved_memory_bytes(), 0);
}

#[test]
fn complete_inventory_rejects_unlisted_files() {
    let root = tempfile::tempdir().unwrap();
    let root_path = root.path().canonicalize().unwrap();
    let authority = root_path.join("authority.json");
    std::fs::write(&authority, b"{}").unwrap();
    std::fs::write(root_path.join("listed"), b"listed").unwrap();
    std::fs::write(root_path.join("extra"), b"extra").unwrap();
    let expected = BTreeSet::from(["listed"]);
    let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
    assert!(validate_inventory_complete(&root_path, &authority, &expected, &context).is_err());
    std::fs::remove_file(root_path.join("extra")).unwrap();
    assert!(validate_inventory_complete(&root_path, &authority, &expected, &context).is_ok());
    std::fs::create_dir(root_path.join("empty-extra-directory")).unwrap();
    assert!(validate_inventory_complete(&root_path, &authority, &expected, &context).is_err());
}

#[test]
fn checked_in_schema_is_valid_json_and_names_only_supported_targets() {
    let schema: serde_json::Value = serde_json::from_str(include_str!(
        "../../../third_party/legacy-office/authority.schema.json"
    ))
    .unwrap();
    let targets = schema["properties"]["targets"]["properties"].as_object().unwrap();
    assert_eq!(targets.len(), SUPPORTED_TARGETS.len());
    assert!(SUPPORTED_TARGETS.iter().all(|target| targets.contains_key(*target)));
    assert_eq!(
        schema["$defs"]["appContainer"]["properties"]["capabilities"],
        serde_json::json!({ "const": [] })
    );
}

#[cfg(unix)]
#[test]
fn complete_inventory_rejects_symlinks_even_when_names_are_listed() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let root_path = root.path().canonicalize().unwrap();
    let authority = root_path.join("authority.json");
    std::fs::write(&authority, b"{}").unwrap();
    std::fs::write(root_path.join("target"), b"target").unwrap();
    symlink(root_path.join("target"), root_path.join("link")).unwrap();
    let expected = BTreeSet::from(["target", "link"]);
    let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
    assert!(validate_inventory_complete(&root_path, &authority, &expected, &context).is_err());
}
