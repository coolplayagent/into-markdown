//! Installed, manifest-bound Agent Skill contract.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

const SKILL_RELATIVE: &str = "share/into-markdown/skills/into-markdown";
const EXPECTED: [&str; 6] = [
    "LICENSE",
    "SKILL.md",
    "agents",
    "agents/openai.yaml",
    "references",
    "references/cli-workflows.md",
];

pub(crate) fn verify(install_root: &Path) -> Result<(), String> {
    let root = install_root.join(SKILL_RELATIVE);
    let metadata = fs::symlink_metadata(&root)
        .map_err(|error| format!("installed Agent Skill is unavailable: {error}"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("installed Agent Skill root is not a trusted directory".into());
    }
    let mut entries = BTreeSet::new();
    collect(&root, &root, &mut entries)?;
    if entries != EXPECTED.into_iter().map(str::to_owned).collect() {
        return Err("installed Agent Skill does not contain the exact reviewed file set".into());
    }

    let skill = read(&root.join("SKILL.md"), "SKILL.md")?;
    if !skill.starts_with("---\nname: into-markdown\ndescription:")
        || !skill.contains("references/cli-workflows.md")
        || skill.contains("TODO")
    {
        return Err("installed Agent Skill entrypoint is invalid".into());
    }
    let metadata = read(&root.join("agents/openai.yaml"), "agents/openai.yaml")?;
    if !metadata.contains("$into-markdown")
        || !metadata.contains("allow_implicit_invocation: true")
        || metadata.contains("dependencies:")
    {
        return Err("installed Agent Skill metadata is invalid".into());
    }
    if fs::read(root.join("LICENSE"))
        .map_err(|error| format!("cannot read skill license: {error}"))?
        != fs::read(install_root.join("LICENSE"))
            .map_err(|error| format!("cannot read Core license: {error}"))?
    {
        return Err("installed Agent Skill license differs from the Core license".into());
    }
    Ok(())
}

fn collect(root: &Path, directory: &Path, entries: &mut BTreeSet<String>) -> Result<(), String> {
    for item in fs::read_dir(directory)
        .map_err(|error| format!("cannot inspect installed Agent Skill: {error}"))?
    {
        let item =
            item.map_err(|error| format!("cannot inspect installed Agent Skill: {error}"))?;
        let path = item.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("cannot inspect installed Agent Skill entry: {error}"))?;
        if metadata.file_type().is_symlink() {
            return Err("installed Agent Skill contains a symbolic link".into());
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| "installed Agent Skill path escaped its root".to_owned())?
            .components()
            .map(|part| {
                part.as_os_str()
                    .to_str()
                    .ok_or_else(|| "installed Agent Skill path is not Unicode".to_owned())
            })
            .collect::<Result<Vec<_>, _>>()?
            .join("/");
        entries.insert(relative);
        if metadata.is_dir() {
            collect(root, &path, entries)?;
        } else if !metadata.is_file() {
            return Err("installed Agent Skill contains an unsupported file type".into());
        }
    }
    Ok(())
}

fn read(path: &Path, label: &str) -> Result<String, String> {
    fs::read_to_string(path).map_err(|error| format!("cannot read installed {label}: {error}"))
}

#[cfg(test)]
pub(crate) fn create_fixture(install_root: &Path) {
    let root = install_root.join(SKILL_RELATIVE);
    fs::create_dir_all(root.join("agents")).unwrap();
    fs::create_dir_all(root.join("references")).unwrap();
    fs::write(install_root.join("LICENSE"), b"license").unwrap();
    fs::write(root.join("LICENSE"), b"license").unwrap();
    fs::write(
        root.join("SKILL.md"),
        b"---\nname: into-markdown\ndescription: convert files\n---\nreferences/cli-workflows.md\n",
    )
    .unwrap();
    fs::write(
        root.join("agents/openai.yaml"),
        b"interface:\n  default_prompt: \"Use $into-markdown.\"\npolicy:\n  allow_implicit_invocation: true\n",
    )
    .unwrap();
    fs::write(root.join("references/cli-workflows.md"), b"# Workflows\n").unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_fixture_passes_and_extra_file_fails() {
        let temporary = tempfile::tempdir().unwrap();
        create_fixture(temporary.path());
        verify(temporary.path()).unwrap();
        fs::write(temporary.path().join(SKILL_RELATIVE).join("README.md"), b"unexpected").unwrap();
        assert!(verify(temporary.path()).unwrap_err().contains("exact reviewed file set"));
    }
}
