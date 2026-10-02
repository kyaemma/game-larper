use std::{env, error::Error, fs, path::PathBuf, process::Command};

const PKG: &str = "game-larper";

fn main() -> Result<(), Box<dyn Error>> {
    match env::args().nth(1).as_deref() {
        Some("install") => install(),
        _ => Err("usage: cargo xtask install".into()),
    }
}

fn install() -> Result<(), Box<dyn Error>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("no workspace root")?
        .to_path_buf();

    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(cargo)
        .args(["build", "--release", "-p", PKG])
        .current_dir(&root)
        .status()?;
    if !status.success() {
        return Err("build failed".into());
    }

    let home = PathBuf::from(env::var("HOME")?);
    let bin_dir = home.join(".local/bin");
    let apps_dir = home.join(".local/share/applications");
    fs::create_dir_all(&bin_dir)?;
    fs::create_dir_all(&apps_dir)?;

    let dest = bin_dir.join(PKG);
    fs::copy(root.join("target/release").join(PKG), &dest)?;

    let desktop = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=GameLarper\n\
         Comment=Larp any game you want !\n\
         Exec={}\n\
         Terminal=false\n\
         Categories=Utility;\n",
        dest.display()
    );
    fs::write(apps_dir.join("game-larper.desktop"), desktop)?;

    let _ = Command::new("update-desktop-database").arg(&apps_dir).status();
    println!("installed to {}", dest.display());
    Ok(())
}
