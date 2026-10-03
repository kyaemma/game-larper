use std::{env, error::Error};

#[cfg(target_os = "linux")]
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[cfg(target_os = "linux")]
const APP: &str = "game-larper";
#[cfg(target_os = "linux")]
const RUNNER: &str = "game-larper-runner";
#[cfg(target_os = "linux")]
const DESKTOP_FILE: &str = "game-larper.desktop";
#[cfg(target_os = "linux")]
const ICON_FILE: &str = "game-larper.png";

fn main() -> Result<(), Box<dyn Error>> {
    match env::args().nth(1).as_deref() {
        Some("install") => install(),
        _ => Err("usage: cargo xtask install".into()),
    }
}

#[cfg(not(target_os = "linux"))]
fn install() -> Result<(), Box<dyn Error>> {
    Err("cargo xtask install is currently only supported on Linux".into())
}

#[cfg(target_os = "linux")]
fn install() -> Result<(), Box<dyn Error>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("xtask is not inside the workspace")?
        .to_path_buf();
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());

    let status = Command::new(&cargo)
        .args(["build", "--release", "-p", APP, "-p", RUNNER])
        .current_dir(&root)
        .status()?;
    if !status.success() {
        return Err("release build failed".into());
    }

    let metadata = Command::new(&cargo)
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(&root)
        .output()?;
    if !metadata.status.success() {
        return Err("cargo metadata failed".into());
    }
    let metadata: serde_json::Value = serde_json::from_slice(&metadata.stdout)?;
    let target = metadata
        .get("target_directory")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .ok_or("cargo metadata did not report target_directory")?;

    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or("HOME must be an absolute path")?;
    let data_home = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".local/share"));

    let bin_dir = home.join(".local/bin");
    let apps_dir = data_home.join("applications");
    let icon_dir = data_home.join("icons/hicolor/256x256/apps");
    fs::create_dir_all(&bin_dir)?;
    fs::create_dir_all(&apps_dir)?;
    fs::create_dir_all(&icon_dir)?;

    let release = target.join("release");
    let app_dest = bin_dir.join(APP);
    let runner_dest = bin_dir.join(RUNNER);
    let icon_dest = icon_dir.join(ICON_FILE);

    install_file(&release.join(APP), &app_dest, 0o755)?;
    install_file(&release.join(RUNNER), &runner_dest, 0o755)?;
    install_file(
        &root.join("assets/branding/game-larper-icon.png"),
        &icon_dest,
        0o644,
    )?;

    let app_path = app_dest
        .to_str()
        .ok_or("the installed application path is not valid UTF-8")?;
    let exec =
        desktop_exec(&[app_path]).ok_or("the application path cannot be written to Exec=")?;
    let desktop = format!(
        "[Desktop Entry]
Type=Application
Version=1.0
Name=Game Larper
Comment=Simulate game activity on Discord
Exec={exec}
Icon=game-larper
Terminal=false
Categories=Utility;Game;
Keywords=Discord;Game;Activity;
StartupNotify=true
"
    );
    write_atomic(&apps_dir.join(DESKTOP_FILE), desktop.as_bytes(), 0o644)?;

    let _ = Command::new("update-desktop-database")
        .arg(&apps_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    println!("installed Game Larper to {}", app_dest.display());
    println!("installed runner to {}", runner_dest.display());
    println!(
        "installed desktop entry to {}",
        apps_dir.join(DESKTOP_FILE).display()
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_file(source: &Path, destination: &Path, mode: u32) -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::PermissionsExt;

    if !source.is_file() {
        return Err(format!("missing build/install source: {}", source.display()).into());
    }
    let parent = destination
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", destination.display()))?;
    fs::create_dir_all(parent)?;
    let staging = parent.join(format!(
        ".{}.{}.tmp",
        destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("game-larper"),
        std::process::id()
    ));
    if staging.exists() {
        fs::remove_file(&staging)?;
    }
    fs::copy(source, &staging)?;
    fs::set_permissions(&staging, fs::Permissions::from_mode(mode))?;
    if let Err(error) = fs::rename(&staging, destination) {
        let _ = fs::remove_file(&staging);
        return Err(error.into());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn write_atomic(destination: &Path, bytes: &[u8], mode: u32) -> Result<(), Box<dyn Error>> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let parent = destination
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", destination.display()))?;
    fs::create_dir_all(parent)?;
    let staging = parent.join(format!(
        ".{}.{}.tmp",
        destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("game-larper.desktop"),
        std::process::id()
    ));
    if staging.exists() {
        fs::remove_file(&staging)?;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&staging)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::set_permissions(&staging, fs::Permissions::from_mode(mode))?;
    if let Err(error) = fs::rename(&staging, destination) {
        let _ = fs::remove_file(&staging);
        return Err(error.into());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
const RESERVED: &[char] = &[
    ' ', '\t', '\n', '"', '\'', '\\', '>', '<', '~', '|', '&', ';', '$', '*', '?', '#', '(', ')',
    '`',
];

#[cfg(target_os = "linux")]
fn desktop_exec(arguments: &[&str]) -> Option<String> {
    let mut quoted = Vec::with_capacity(arguments.len());
    for argument in arguments {
        if argument.chars().any(char::is_control) {
            return None;
        }
        let argument = argument.replace('%', "%%");
        if argument.is_empty() || argument.contains(RESERVED) {
            let mut text = String::from("\"");
            for character in argument.chars() {
                if matches!(character, '"' | '`' | '$' | '\\') {
                    text.push('\\');
                }
                text.push(character);
            }
            text.push('"');
            quoted.push(text);
        } else {
            quoted.push(argument);
        }
    }
    Some(quoted.join(" ").replace('\\', "\\\\"))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::desktop_exec;

    #[test]
    fn desktop_exec_quotes_reserved_characters() {
        assert_eq!(
            desktop_exec(&["/home/user/My Games/game-larper"]).as_deref(),
            Some("\"/home/user/My Games/game-larper\"")
        );
        assert_eq!(
            desktop_exec(&["/opt/100%/game-larper"]).as_deref(),
            Some("/opt/100%%/game-larper")
        );
    }
}
