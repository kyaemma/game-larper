use std::path::{Path, PathBuf};

use crate::error::Error;

const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Turn a Discord executable rule into a relative Windows path.
///
/// A leading `>` is a shared-host rule (paired with command-line arguments in the catalog),
/// not a filename. Those rules are rejected here so they can never be joined onto disk.
pub fn normalize_executable(remote_name: &str) -> Option<PathBuf> {
    if remote_name.trim().is_empty() || remote_name.chars().count() > 512 {
        return None;
    }
    if remote_name.starts_with(['/', '\\', '>']) || remote_name.contains(':') {
        return None;
    }
    let segments: Vec<&str> = remote_name.split(['/', '\\']).collect();
    if segments.len() > 16 {
        return None;
    }
    for segment in &segments {
        if !segment_is_safe(segment) {
            return None;
        }
    }
    let last = segments.last()?;
    if !last.to_ascii_lowercase().ends_with(".exe") {
        return None;
    }
    let mut path = PathBuf::new();
    for segment in segments {
        path.push(segment);
    }
    Some(path)
}

fn segment_is_safe(segment: &str) -> bool {
    if segment.is_empty() || segment.chars().count() > 120 || segment == "." || segment == ".." {
        return false;
    }
    if segment.ends_with([' ', '.']) {
        return false;
    }
    if segment
        .chars()
        .any(|character| (character as u32) < 32 || "<>:\"/\\|?*".contains(character))
    {
        return false;
    }
    let stem = segment.split('.').next().unwrap_or(segment);
    !RESERVED
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
}

/// Resolve `remote_name` under `{runtime_root}/{application_id}` and prove it stays there.
pub fn resolve_executable(
    runtime_root: &Path,
    application_id: &str,
    remote_name: &str,
) -> Result<PathBuf, Error> {
    if application_id.is_empty() || !application_id.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Error::InvalidApplicationId);
    }
    let relative =
        normalize_executable(remote_name).ok_or(Error::UnsafePath("Unsafe executable path."))?;
    let root = std::path::absolute(runtime_root)?;
    let application_root = std::path::absolute(root.join(application_id))?;
    let target = std::path::absolute(application_root.join(relative))?;
    if !is_strict_child(&target, &application_root) {
        return Err(Error::UnsafePath(
            "Executable escapes the runtime directory.",
        ));
    }
    Ok(target)
}

pub(crate) fn is_strict_child(target: &Path, parent: &Path) -> bool {
    let mut prefix = parent.to_string_lossy().into_owned();
    if !prefix.ends_with(['\\', '/']) {
        prefix.push(std::path::MAIN_SEPARATOR);
    }
    let target = target.to_string_lossy();
    target.len() > prefix.len()
        && target.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

pub(crate) fn separator_count(path: &Path) -> usize {
    path.components().count().saturating_sub(1)
}
