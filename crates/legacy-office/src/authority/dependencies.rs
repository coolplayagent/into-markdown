use super::{FileRole, RuntimeFile, Target, VerifiedRuntimeFile, unavailable};
use crate::authority::paths::{safe_relative, system_library_path};
use into_markdown_core::{ConversionError, ExecutionContext};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

const MAX_DEPENDENCY_BYTES: u64 = 512 * 1024 * 1024;
const MAX_DEPENDENCIES: usize = 4_096;

mod macho;
mod pe;

pub(super) fn validate(
    target: &Target,
    target_name: &str,
    root: &Path,
    context: &ExecutionContext,
) -> Result<Vec<VerifiedRuntimeFile>, ConversionError> {
    let inventory =
        target.files.iter().map(|entry| (entry.path.as_str(), entry)).collect::<BTreeMap<_, _>>();
    let declared_system = target
        .sandbox
        .system_libraries
        .iter()
        .map(|library| {
            system_library_path(library, target_name)?;
            Ok(library.identity.clone())
        })
        .collect::<Result<BTreeSet<_>, ConversionError>>()?;
    if declared_system.len() != target.sandbox.system_libraries.len() {
        return Err(unavailable("dependencyAuthority:duplicateSystemDeclaration"));
    }
    let mut used_system = BTreeSet::new();
    let _worker_files = closure(
        &target.worker,
        target,
        target_name,
        root,
        &inventory,
        &declared_system,
        &mut used_system,
        context,
    )?;
    let native_files = closure(
        &target.kit_library,
        target,
        target_name,
        root,
        &inventory,
        &declared_system,
        &mut used_system,
        context,
    )?;
    if used_system != declared_system {
        let missing =
            declared_system.difference(&used_system).cloned().collect::<Vec<_>>().join(",");
        let unexpected =
            used_system.difference(&declared_system).cloned().collect::<Vec<_>>().join(",");
        return Err(unavailable(format!(
            "dependencyAuthority:systemClosure:missing={missing}:unexpected={unexpected}"
        )));
    }
    Ok(native_files)
}

#[allow(clippy::too_many_arguments)]
fn closure(
    start: &str,
    target: &Target,
    target_name: &str,
    root: &Path,
    inventory: &BTreeMap<&str, &RuntimeFile>,
    declared_system: &BTreeSet<String>,
    used_system: &mut BTreeSet<String>,
    context: &ExecutionContext,
) -> Result<Vec<VerifiedRuntimeFile>, ConversionError> {
    let mut pending = vec![start.to_owned()];
    let mut visited = BTreeSet::new();
    let mut files = Vec::new();
    while let Some(relative) = pending.pop() {
        context.checkpoint()?;
        if !visited.insert(relative.clone()) {
            continue;
        }
        if visited.len() > MAX_DEPENDENCIES {
            return Err(unavailable("dependencyAuthority:closureLimit"));
        }
        let entry = inventory
            .get(relative.as_str())
            .copied()
            .ok_or_else(|| unavailable(format!("dependencyAuthority:missing:{relative}")))?;
        if !matches!(entry.role, FileRole::Worker | FileRole::KitLibrary | FileRole::Runtime) {
            return Err(unavailable(format!("dependencyAuthority:role:{relative}")));
        }
        let path = root.join(&relative);
        if entry.bytes == 0 || entry.bytes > MAX_DEPENDENCY_BYTES {
            return Err(unavailable(format!("dependencyAuthority:size:{relative}")));
        }
        let _memory = context.reserve_memory(entry.bytes)?;
        let bytes = std::fs::read(&path).map_err(|_| unavailable("dependencyIo"))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != entry.bytes {
            return Err(unavailable("dependencyIo"));
        }
        let specification = parse(&bytes, &target.abi.binary_format, target_name)
            .map_err(|_| unavailable(format!("dependencyAuthority:parse:{relative}")))?;
        for needed in specification.needed {
            context.checkpoint()?;
            if declared_system.contains(needed.as_str()) {
                used_system.insert(needed);
                continue;
            }
            let resolved =
                resolve(&relative, &needed, &specification.search, inventory, target_name)
                    .map_err(|_| {
                        unavailable(format!("dependencyAuthority:unresolved:{relative}:{needed}"))
                    })?;
            if !pending.contains(&resolved) && !visited.contains(&resolved) {
                pending.try_reserve(1).map_err(|_| unavailable("dependencyAuthority"))?;
                pending.push(resolved);
            }
        }
        files.try_reserve(1).map_err(|_| unavailable("dependencyAuthority"))?;
        files.push(VerifiedRuntimeFile {
            relative: relative.clone(),
            path,
            bytes: entry.bytes,
            sha256: entry.sha256.clone(),
            executable: relative == target.worker,
        });
    }
    files.sort_by(|left, right| left.relative.cmp(&right.relative));
    Ok(files)
}

struct Specification {
    needed: BTreeSet<String>,
    search: Vec<String>,
}

fn parse(bytes: &[u8], format: &str, target: &str) -> Result<Specification, ConversionError> {
    match format {
        "elf" => parse_elf(bytes),
        "mach-o" => macho::parse(bytes, target),
        "pe" => pe::parse(bytes),
        _ => Err(unavailable("dependencyAuthority")),
    }
}

fn malformed() -> ConversionError {
    unavailable("dependencyAuthority")
}

fn parse_elf(bytes: &[u8]) -> Result<Specification, ConversionError> {
    if bytes.get(..6) != Some(b"\x7fELF\x02") || bytes.get(5) != Some(&1) {
        return Err(unavailable("dependencyAuthority"));
    }
    let program_offset =
        usize::try_from(le64(bytes, 32)?).map_err(|_| unavailable("dependencyAuthority"))?;
    let program_size = usize::from(le16(bytes, 54)?);
    let program_count = usize::from(le16(bytes, 56)?);
    if program_size < 56 || program_count > MAX_DEPENDENCIES {
        return Err(unavailable("dependencyAuthority"));
    }
    let mut loads = Vec::new();
    let mut dynamic = None;
    for index in 0..program_count {
        let start = program_offset
            .checked_add(
                index
                    .checked_mul(program_size)
                    .ok_or_else(|| unavailable("dependencyAuthority"))?,
            )
            .ok_or_else(|| unavailable("dependencyAuthority"))?;
        let kind = le32(bytes, start)?;
        let offset = le64(bytes, start + 8)?;
        let virtual_address = le64(bytes, start + 16)?;
        let file_size = le64(bytes, start + 32)?;
        if kind == 1 {
            loads.push((virtual_address, offset, file_size));
        } else if kind == 2 {
            dynamic = Some((offset, file_size));
        }
    }
    let (dynamic_offset, dynamic_size) =
        dynamic.ok_or_else(|| unavailable("dependencyAuthority"))?;
    let start = usize::try_from(dynamic_offset).map_err(|_| unavailable("dependencyAuthority"))?;
    let size = usize::try_from(dynamic_size).map_err(|_| unavailable("dependencyAuthority"))?;
    let end = start.checked_add(size).ok_or_else(|| unavailable("dependencyAuthority"))?;
    if end > bytes.len() || size % 16 != 0 {
        return Err(unavailable("dependencyAuthority"));
    }
    let mut needed_offsets = Vec::new();
    let mut search_offsets = Vec::new();
    let mut string_address = None;
    let mut string_size = None;
    for cursor in (start..end).step_by(16) {
        let tag = le64(bytes, cursor)?;
        let value = le64(bytes, cursor + 8)?;
        match tag {
            0 => break,
            1 => needed_offsets.push(value),
            5 => string_address = Some(value),
            10 => string_size = Some(value),
            15 | 29 => search_offsets.push(value),
            _ => {}
        }
    }
    let address = string_address.ok_or_else(|| unavailable("dependencyAuthority"))?;
    let size = string_size.ok_or_else(|| unavailable("dependencyAuthority"))?;
    let string_start = loads
        .iter()
        .find_map(|(virtual_address, offset, file_size)| {
            let delta = address.checked_sub(*virtual_address)?;
            (delta < *file_size).then(|| offset.checked_add(delta)).flatten()
        })
        .and_then(|offset| usize::try_from(offset).ok())
        .ok_or_else(|| unavailable("dependencyAuthority"))?;
    let string_end = string_start
        .checked_add(usize::try_from(size).map_err(|_| unavailable("dependencyAuthority"))?)
        .ok_or_else(|| unavailable("dependencyAuthority"))?;
    if string_end > bytes.len() {
        return Err(unavailable("dependencyAuthority"));
    }
    let strings = &bytes[string_start..string_end];
    let needed = needed_offsets
        .into_iter()
        .map(|offset| elf_string(strings, offset))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let mut search = Vec::new();
    for offset in search_offsets {
        let value = elf_string(strings, offset)?;
        for path in value.split(':') {
            if path.is_empty() {
                return Err(unavailable("dependencyAuthority"));
            }
            search.push(path.to_owned());
        }
    }
    Ok(Specification { needed, search })
}

fn elf_string(strings: &[u8], offset: u64) -> Result<String, ConversionError> {
    let start = usize::try_from(offset).map_err(|_| unavailable("dependencyAuthority"))?;
    let rest = strings.get(start..).ok_or_else(|| unavailable("dependencyAuthority"))?;
    let end = rest
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| unavailable("dependencyAuthority"))?;
    let value =
        std::str::from_utf8(&rest[..end]).map_err(|_| unavailable("dependencyAuthority"))?;
    if value.is_empty() || value.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(unavailable("dependencyAuthority"));
    }
    Ok(value.to_owned())
}

fn resolve(
    owner: &str,
    needed: &str,
    search: &[String],
    inventory: &BTreeMap<&str, &RuntimeFile>,
    target: &str,
) -> Result<String, ConversionError> {
    if needed.starts_with('/') || needed.starts_with("@executable_path") {
        return Err(unavailable("dependencyAuthority"));
    }
    let owner_directory = Path::new(owner).parent().unwrap_or_else(|| Path::new(""));
    let mut candidates = Vec::new();
    if let Some(suffix) = needed.strip_prefix("@loader_path/") {
        push_candidate(&mut candidates, owner_directory, suffix, inventory, target)?;
    } else if let Some(suffix) = needed.strip_prefix("@rpath/") {
        for path in search {
            let base = expand_search(path, owner_directory, target)?;
            push_candidate(&mut candidates, Path::new(&base), suffix, inventory, target)?;
        }
    } else if needed.contains('/') {
        return Err(unavailable("dependencyAuthority"));
    } else {
        push_candidate(&mut candidates, owner_directory, needed, inventory, target)?;
        if target == "x86_64-pc-windows-msvc" {
            push_candidate(
                &mut candidates,
                Path::new("libreoffice/System64"),
                needed,
                inventory,
                target,
            )?;
        }
        for path in search {
            let base = expand_search(path, owner_directory, target)?;
            push_candidate(&mut candidates, Path::new(&base), needed, inventory, target)?;
        }
    }
    candidates.sort();
    candidates.dedup();
    if candidates.len() != 1 {
        return Err(unavailable("dependencyAuthority"));
    }
    Ok(candidates.remove(0))
}

fn expand_search(value: &str, owner: &Path, target: &str) -> Result<String, ConversionError> {
    let relative = value
        .strip_prefix("$ORIGIN/")
        .or_else(|| value.strip_prefix("${ORIGIN}/"))
        .or_else(|| value.strip_prefix("@loader_path/"));
    if value == "$ORIGIN" || value == "${ORIGIN}" || value == "@loader_path" {
        return normalize(owner);
    }
    if let Some(relative) = relative {
        return normalize(&owner.join(relative));
    }
    if target == "x86_64-pc-windows-msvc" || !value.starts_with('/') {
        return normalize(&owner.join(value));
    }
    Err(unavailable("dependencyAuthority"))
}

fn push_candidate(
    candidates: &mut Vec<String>,
    base: &Path,
    suffix: &str,
    inventory: &BTreeMap<&str, &RuntimeFile>,
    target: &str,
) -> Result<(), ConversionError> {
    let candidate = normalize(&base.join(suffix))?;
    if let Some(path) = inventory.keys().find(|path| {
        if target == "x86_64-pc-windows-msvc" {
            path.eq_ignore_ascii_case(&candidate)
        } else {
            **path == candidate
        }
    }) {
        candidates.push((*path).to_string());
    }
    Ok(())
}

fn normalize(path: &Path) -> Result<String, ConversionError> {
    if path.is_absolute() || path.components().any(|part| !matches!(part, Component::Normal(_))) {
        return Err(unavailable("dependencyAuthority"));
    }
    let value = path
        .components()
        .map(|component| match component {
            Component::Normal(component) => {
                component.to_str().ok_or_else(|| unavailable("dependencyAuthority"))
            }
            _ => Err(unavailable("dependencyAuthority")),
        })
        .collect::<Result<Vec<_>, _>>()?
        .join("/");
    if !safe_relative(&value) {
        return Err(unavailable("dependencyAuthority"));
    }
    Ok(value)
}

fn le16(bytes: &[u8], offset: usize) -> Result<u16, ConversionError> {
    let value = bytes.get(offset..offset + 2).ok_or_else(|| unavailable("dependencyAuthority"))?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn le32(bytes: &[u8], offset: usize) -> Result<u32, ConversionError> {
    let value = bytes.get(offset..offset + 4).ok_or_else(|| unavailable("dependencyAuthority"))?;
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn le64(bytes: &[u8], offset: usize) -> Result<u64, ConversionError> {
    let value = bytes.get(offset..offset + 8).ok_or_else(|| unavailable("dependencyAuthority"))?;
    Ok(u64::from_le_bytes(value.try_into().map_err(|_| unavailable("dependencyAuthority"))?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use object::Object as _;

    #[test]
    fn platform_dependency_parser_matches_object_import_inventory() {
        let executable = std::env::current_exe().unwrap();
        let bytes = std::fs::read(executable).unwrap();
        let format = if cfg!(target_os = "macos") {
            "mach-o"
        } else if cfg!(target_os = "linux") {
            "elf"
        } else {
            "pe"
        };
        let target = if cfg!(target_os = "macos") {
            "aarch64-apple-darwin"
        } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
            "aarch64-unknown-linux-gnu"
        } else if cfg!(target_os = "linux") {
            "x86_64-unknown-linux-gnu"
        } else {
            "x86_64-pc-windows-msvc"
        };
        let parsed = parse(&bytes, format, target).unwrap().needed;
        let object = object::File::parse(bytes.as_slice()).unwrap();
        let imported = object
            .imports()
            .unwrap()
            .filter_map(Result::ok)
            .map(|import| {
                let identity = std::str::from_utf8(import.library()).unwrap();
                if cfg!(windows) { identity.to_ascii_lowercase() } else { identity.to_owned() }
            })
            .collect::<BTreeSet<_>>();
        assert!(imported.is_subset(&parsed));
        assert!(!parsed.is_empty());
    }

    #[test]
    fn loader_resolution_rejects_absolute_escape_unlisted_and_ambiguous_rpaths() {
        let direct = RuntimeFile {
            path: "runtime/libdirect.so".into(),
            bytes: 1,
            sha256: "a".repeat(64),
            role: FileRole::Runtime,
        };
        let alternate = RuntimeFile {
            path: "runtime/alt/libdirect.so".into(),
            bytes: 1,
            sha256: "b".repeat(64),
            role: FileRole::Runtime,
        };
        let inventory = BTreeMap::from([
            (direct.path.as_str(), &direct),
            (alternate.path.as_str(), &alternate),
        ]);
        assert!(
            resolve(
                "runtime/kit.so",
                "/tmp/constructor-canary.so",
                &[],
                &inventory,
                "x86_64-unknown-linux-gnu",
            )
            .is_err()
        );
        assert!(
            resolve(
                "runtime/kit.so",
                "unlisted.so",
                &["$ORIGIN".into()],
                &inventory,
                "x86_64-unknown-linux-gnu",
            )
            .is_err()
        );
        assert!(
            resolve(
                "runtime/kit.so",
                "libdirect.so",
                &["$ORIGIN/alt".into()],
                &inventory,
                "x86_64-unknown-linux-gnu",
            )
            .is_err()
        );
    }

    #[test]
    fn arbitrary_system_named_library_is_not_authorized() {
        let fake = super::super::SystemLibraryAuthority {
            identity: "/tmp/libSystem.B.dylib".into(),
            path: "/tmp/libSystem.B.dylib".into(),
        };
        assert!(system_library_path(&fake, "aarch64-apple-darwin").is_err());
        let fake = super::super::SystemLibraryAuthority {
            identity: "libconstructor-canary.so.6".into(),
            path: "/usr/lib/libconstructor-canary.so.6".into(),
        };
        assert!(system_library_path(&fake, "x86_64-unknown-linux-gnu").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_resolution_is_case_insensitive_and_returns_manifest_spelling() {
        let runtime = RuntimeFile {
            path: "libreoffice/program/Argon2OptDll.dll".into(),
            bytes: 1,
            sha256: "a".repeat(64),
            role: FileRole::Runtime,
        };
        let vcruntime = RuntimeFile {
            path: "libreoffice/System64/vcruntime140.dll".into(),
            bytes: 1,
            sha256: "b".repeat(64),
            role: FileRole::Runtime,
        };
        let inventory = BTreeMap::from([
            (runtime.path.as_str(), &runtime),
            (vcruntime.path.as_str(), &vcruntime),
        ]);
        assert_eq!(
            resolve(
                "libreoffice/program/mergedlo.dll",
                "argon2optdll.dll",
                &[],
                &inventory,
                "x86_64-pc-windows-msvc",
            )
            .unwrap(),
            runtime.path
        );
        assert_eq!(
            resolve(
                "legacy-office-worker.exe",
                "VCRUNTIME140.DLL",
                &[],
                &inventory,
                "x86_64-pc-windows-msvc",
            )
            .unwrap(),
            vcruntime.path
        );
        assert_eq!(
            normalize(&Path::new("libreoffice/program").join("mergedlo.dll")).unwrap(),
            "libreoffice/program/mergedlo.dll"
        );
    }
}
