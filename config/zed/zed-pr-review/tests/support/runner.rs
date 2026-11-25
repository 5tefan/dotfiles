//! Native, offline subprocesses and terminal drivers for integration tests.

#[cfg(unix)]
mod cli_tools;
#[cfg(unix)]
#[path = "mod.rs"]
mod fixture;
#[cfg(unix)]
mod gh;
#[cfg(unix)]
mod log_terminal;
#[cfg(unix)]
mod pty;
#[cfg(unix)]
mod review_terminal;
#[cfg(unix)]
mod stack_terminal;
#[cfg(unix)]
mod transport;

#[cfg(unix)]
pub fn install(destination: &std::path::Path, config: &serde_json::Value) -> anyhow::Result<()> {
    fixture::install_with(&std::env::current_exe()?, destination, config)
}

#[cfg(unix)]
fn invocation_path(argument: &std::ffi::OsStr) -> anyhow::Result<std::path::PathBuf> {
    use anyhow::Context;
    use std::path::{Path, PathBuf};
    let path = Path::new(argument);
    if path.is_absolute() {
        return Ok(path.to_owned());
    }
    if path.components().count() > 1 {
        return Ok(std::env::current_dir()?.join(path));
    }
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join(path))
        .find(|candidate| candidate.is_file())
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                PathBuf::from(".").join(path)
            }
        })
        .context("Cannot locate fixture invocation in PATH")
}

#[cfg(unix)]
fn run() -> anyhow::Result<i32> {
    use anyhow::{Context, bail};
    let mut args = std::env::args_os();
    let executable = invocation_path(&args.next().context("Missing executable name")?)?;
    let args: Vec<_> = args.collect();
    let config_path = fixture::configuration_path(&executable);
    match std::fs::read(&config_path) {
        Ok(bytes) => {
            let config: serde_json::Value = serde_json::from_slice(&bytes)?;
            match config["tool"].as_str().context("Missing fixture tool")? {
                "gh" | "stack-gh" | "transport-gh" => gh::run(&config, args),
                "cli-git" | "cli-jj" | "editor" | "zed" | "gate" => cli_tools::run(&config, args),
                "transport-git" | "transport-jj" | "transport-hook" => {
                    transport::run(&config, args)
                }
                "deny" => {
                    eprintln!(
                        "{}",
                        config["message"]
                            .as_str()
                            .unwrap_or("Unexpected fixture command")
                    );
                    Ok(99)
                }
                tool => bail!("Unknown fixture tool: {tool}"),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let (driver, args) = args.split_first().context("Missing terminal driver")?;
            match driver.to_str() {
                Some("log-terminal") => log_terminal::run(args.to_vec()),
                Some("stack-terminal") => stack_terminal::run(args.to_vec()),
                Some("review-terminal") => review_terminal::run(args.to_vec()),
                _ => bail!("Unknown terminal driver or missing fixture configuration"),
            }
        }
        Err(error) => Err(error).context("Cannot read fixture configuration"),
    }
}

#[cfg(unix)]
fn main() {
    let code = match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("zpr-test-helper: {error:#}");
            97
        }
    };
    std::process::exit(code);
}

#[cfg(not(unix))]
fn main() {
    eprintln!("The integration test helper requires Unix");
    std::process::exit(97);
}
