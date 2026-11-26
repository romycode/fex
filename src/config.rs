//! Directorio de configuración del usuario.
//!
//! - Linux: `$XDG_CONFIG_HOME/fex` o `~/.config/fex`.
//! - macOS: `~/Library/Application Support/fex`.
//! - `FEX_CONFIG_DIR` lo sustituye en ambos (desarrollo y tests).

use std::{io, path::PathBuf};

pub fn dir() -> io::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("FEX_CONFIG_DIR").map(PathBuf::from).filter(|p| p.is_absolute()) {
        return Ok(dir);
    }
    platform_dir()
}

#[cfg(target_os = "linux")]
fn platform_dir() -> io::Result<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|config| config.join("fex"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_CONFIG_HOME / HOME"))
}

#[cfg(target_os = "macos")]
fn platform_dir() -> io::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Library/Application Support/fex"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME"))
}

pub fn file(name: &str) -> io::Result<PathBuf> {
    Ok(dir()?.join(name))
}
