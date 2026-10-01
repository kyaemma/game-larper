use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let mut file_name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    file_name.push(format!(".{}.{nonce}.tmp", std::process::id()));
    let temporary = path.with_file_name(file_name);
    let write_result = (|| -> io::Result<()> {
        let mut file = File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = replace_with_retry(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

fn replace_with_retry(from: &Path, to: &Path) -> io::Result<()> {
    let mut last = None;
    for attempt in 0..3 {
        match replace_file(from, to) {
            Ok(()) => return Ok(()),
            Err(error) if attempt < 2 && is_sharing_error(&error) => {
                last = Some(error);
                std::thread::sleep(std::time::Duration::from_millis(30));
            }
            Err(error) => return Err(error),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("atomic replace failed")))
}

fn is_sharing_error(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(5 | 32))
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let from_wide: Vec<u16> = from
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let to_wide: Vec<u16> = to
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: both buffers are NUL-terminated UTF-16 paths that outlive the call.
    let replaced = unsafe {
        windows_sys::Win32::Storage::FileSystem::MoveFileExW(
            from_wide.as_ptr(),
            to_wide.as_ptr(),
            windows_sys::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING
                | windows_sys::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH,
        )
    };
    if replaced == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    // The temporary file is created beside the destination, so both paths are on the same
    // filesystem. On Unix, rename replaces an existing regular file atomically: readers see
    // either the old file or the new one, never a gap where the destination is missing.
    fs::rename(from, to)
}

pub fn quarantine_corrupt(path: &Path) {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let target = path.with_extension(format!("json.bad-{nonce}"));
    let _ = fs::rename(path, target);
}
