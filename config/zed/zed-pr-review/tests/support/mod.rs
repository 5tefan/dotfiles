//! Installation shared by integration fixtures and the native test helper.
#![cfg(unix)]

use anyhow::{Context, Result};
use serde_json::Value;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

pub fn configuration_path(executable: &Path) -> PathBuf {
    let mut name: OsString = executable.as_os_str().to_owned();
    name.push(".json");
    PathBuf::from(name)
}

// The binary installs additional fixtures with its own executable; integration
// crates use Cargo's path to the same already-built binary.
#[allow(dead_code)]
pub fn install(destination: &Path, config: &Value) -> Result<()> {
    let executable = option_env!("CARGO_BIN_EXE_zpr-test-helper")
        .context("Cargo did not provide the native test helper executable")?;
    install_with(Path::new(executable), destination, config)
}

pub fn install_with(executable: &Path, destination: &Path, config: &Value) -> Result<()> {
    let parent = destination
        .parent()
        .context("Helper destination has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(temporary.as_file(), config)?;
    temporary
        .persist(configuration_path(destination))
        .map_err(|error| error.error)?;
    // Never write through an existing symlink to the shared Cargo binary.
    match fs::remove_file(destination) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("Cannot replace fixture executable"),
    }
    symlink(executable, destination).context("Cannot install native fixture executable")
}
