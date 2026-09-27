use crate::RewriteProfile;
use serde_json::Value as JsonValue;
use std::fs;
use std::io;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

pub(super) fn display_source_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

pub(super) fn ensure_migration_path(root: &Path, path: &Path) -> io::Result<()> {
    let relative = path.strip_prefix(root).map_err(|_| {
        invalid_data_error(format!(
            "migration path `{}` is outside migration root `{}`",
            path.display(),
            root.display()
        ))
    })?;
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(invalid_data_error(format!(
            "migration path `{}` is not normalized beneath `{}`",
            path.display(),
            root.display()
        )));
    }
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            unreachable!("migration path components were validated above");
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(invalid_data_error(format!(
                    "migration path `{}` contains symlink component `{}`",
                    path.display(),
                    current.display()
                )));
            }
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => break,
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

pub(crate) fn read_json_file(path: &Path) -> io::Result<Option<JsonValue>> {
    if !path.is_file() {
        return Ok(None);
    }

    let raw = fs::read_to_string(path)?;
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|err| invalid_data_error(err.to_string()))
}

pub(super) fn is_missing_or_empty_text_file(path: &Path) -> io::Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(err) => return Err(err),
    };
    if !metadata.is_file() {
        return Ok(false);
    }

    Ok(fs::read_to_string(path)?.trim().is_empty())
}

pub(crate) fn path_is_missing_without_follow(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(err) => Err(err),
    }
}

pub(super) fn invalid_data_error(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

pub(super) fn copy_dir_recursive(
    source: &Path,
    target: &Path,
    rewrite_profile: RewriteProfile,
) -> io::Result<()> {
    let source_metadata = fs::symlink_metadata(source)?;
    if !source_metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("migration source `{}` is not a directory", source.display()),
        ));
    }
    match fs::symlink_metadata(target) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("migration target `{}` is not a directory", target.display()),
            ));
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => fs::create_dir_all(target)?,
        Err(err) => return Err(err),
    }

    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        let file_type = entry.file_type()?;

        if file_type.is_dir() {
            copy_dir_recursive(&source_path, &target_path, rewrite_profile)?;
            continue;
        }

        if file_type.is_file() {
            if is_skill_md(&source_path) {
                rewrite_and_copy_text_file(&source_path, &target_path, rewrite_profile)?;
            } else {
                fs::copy(source_path, target_path)?;
            }
        }
    }

    Ok(())
}

pub(super) fn rewrite_external_agent_terms(
    content: &str,
    rewrite_profile: RewriteProfile,
) -> String {
    rewrite_profile.rewrite(content)
}

fn is_skill_md(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("SKILL.md"))
}

fn rewrite_and_copy_text_file(
    source: &Path,
    target: &Path,
    rewrite_profile: RewriteProfile,
) -> io::Result<()> {
    let source_contents = fs::read_to_string(source)?;
    let rewritten = rewrite_external_agent_terms(&source_contents, rewrite_profile);
    fs::write(target, rewritten)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    #[test]
    fn ensure_migration_path_rejects_parent_after_missing_component() {
        let root = TempDir::new().expect("create tempdir");
        let path = root.path().join("missing/../outside");

        let error = ensure_migration_path(root.path(), &path)
            .expect_err("reject parent component after missing path");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("not normalized"));
    }
}
