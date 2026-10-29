//! User-confirmed updates delegate downloads, integrity and rollback to the installer.
use agent_bridge::{service, update};
use anyhow::{Context, Result, ensure};
use std::{
    io::{self, BufRead, IsTerminal, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const INSTALLER: &str = include_str!("../install.sh");

pub fn run(state: &Path, check: bool, disable_checks: bool, enable_checks: bool) -> Result<()> {
    if disable_checks || enable_checks {
        let status = update::set_automatic_checks(state, enable_checks)?;
        println!("{}", serde_json::to_string_pretty(&status)?);
        return Ok(());
    }
    if check {
        let status = update::check(state)?;
        println!("{}", serde_json::to_string_pretty(&status)?);
        return Ok(());
    }

    let home = service::home()?;
    let definition = service_definition(&home)?;
    validate_managed_installation(
        &std::env::current_exe().context("Cannot locate this executable")?,
        &service::installed_binary()?,
        &definition,
    )?;
    let state = state
        .canonicalize()
        .context("Cannot resolve the selected service state directory")?;
    let status = update::check(&state)?;
    println!("Current version: {}", status.current_version);
    let Some(version) = status.available_version else {
        println!("Agent Bridge is up to date.");
        return Ok(());
    };
    println!("Available version: {version}");
    if let Some(url) = status.release_notes_url {
        println!("Release notes: {url}");
    }
    println!("The update will restart the managed Agent Bridge service.");
    let terminal = io::stdin().is_terminal() && io::stdout().is_terminal();
    if !confirm_install(&mut io::stdin().lock(), &mut io::stdout().lock(), terminal)? {
        println!("Update cancelled.");
        return Ok(());
    }
    run_installer(installer_command(&state, &version)?)?;
    if let Err(error) = update::record_installed(&state, &version) {
        eprintln!("Update installed, but its cached notice could not be cleared: {error:#}");
    }
    println!(
        "Updated Agent Bridge to {version}. Reconnect the Agent Bridge MCP server or restart your agent client to load the updated executable and tool definitions."
    );
    Ok(())
}

fn service_definition(home: &Path) -> Result<PathBuf> {
    if cfg!(target_os = "linux") {
        Ok(home.join(".config/systemd/user/agent-bridge.service"))
    } else if cfg!(target_os = "macos") {
        Ok(home.join("Library/LaunchAgents/local.agent-bridge.plist"))
    } else {
        anyhow::bail!("Updates support managed Linux and macOS installations only")
    }
}

fn validate_managed_installation(
    current: &Path,
    installed: &Path,
    definition: &Path,
) -> Result<()> {
    const GUIDANCE: &str = "The update command requires the managed installation at ~/.local/bin/agent-bridge. For source builds, custom paths or binary-only installations, rerun the installer with your original options; see docs/install.md.";
    ensure!(
        std::fs::symlink_metadata(installed).is_ok_and(|meta| meta.is_file()),
        "{GUIDANCE}"
    );
    ensure!(
        current.canonicalize().ok() == installed.canonicalize().ok()
            && current.canonicalize().is_ok(),
        "{GUIDANCE}"
    );
    ensure!(
        std::fs::symlink_metadata(definition).is_ok_and(|meta| meta.is_file()),
        "No regular managed service definition was found. {GUIDANCE}"
    );
    Ok(())
}

fn confirm_install(
    input: &mut impl BufRead,
    output: &mut impl Write,
    terminal: bool,
) -> Result<bool> {
    ensure!(
        terminal,
        "Installation requires an interactive terminal for confirmation. Run agent-bridge update in a terminal, or use agent-bridge update --check to check without installing."
    );
    write!(output, "Install this update? [y/N] ")?;
    output.flush()?;
    let mut answer = String::new();
    // Bound input even if a terminal or fixture supplies an unexpectedly long line.
    std::io::Read::take(input, 1024).read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn installer_command(state: &Path, version: &str) -> Result<Command> {
    let components: Vec<_> = version.split('.').collect();
    ensure!(
        components.len() == 3
            && components.iter().all(|part| {
                !part.is_empty()
                    && part.bytes().all(|byte| byte.is_ascii_digit())
                    && (part.len() == 1 || !part.starts_with('0'))
                    && part.parse::<u64>().is_ok()
            }),
        "Update target is not a stable semantic version"
    );
    ensure!(state.is_absolute(), "Service state path must be absolute");
    let mut command = Command::new("sh");
    command
        .args([
            "-s",
            "--",
            "--version",
            &format!("v{version}"),
            "--state-dir",
        ])
        .arg(state)
        .stdin(Stdio::piped());
    Ok(command)
}

fn run_installer(mut command: Command) -> Result<()> {
    let mut child = command
        .spawn()
        .context("Could not start the bundled installer")?;
    let write_result = child
        .stdin
        .take()
        .context("Installer stdin is unavailable")
        .and_then(|mut input| input.write_all(INSTALLER.as_bytes()).map_err(Into::into));
    // Always reap the child, including when it exits before consuming its input.
    let status = child
        .wait()
        .context("Could not wait for the bundled installer")?;
    ensure!(
        status.success(),
        "Agent Bridge update failed. Inspect the installer output above for backup/rollback details."
    );
    write_result.context("Could not supply the bundled installer")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        io::Cursor,
        os::unix::fs::{PermissionsExt, symlink},
    };

    #[test]
    fn confirmation_defaults_to_no_and_never_reads_noninteractive_input() {
        for response in ["\n", "no\n", "yesterday\n", ""] {
            assert!(!confirm_install(&mut Cursor::new(response), &mut Vec::new(), true).unwrap());
        }
        for response in ["y\n", "YES\n"] {
            assert!(confirm_install(&mut Cursor::new(response), &mut Vec::new(), true).unwrap());
        }
        let mut input = Cursor::new("yes\n");
        let mut output = Vec::new();
        assert!(confirm_install(&mut input, &mut output, false).is_err());
        assert_eq!(input.position(), 0);
        assert!(output.is_empty());
    }

    #[test]
    fn unmanaged_and_symlink_installations_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let installed = root.path().join("installed");
        let source = root.path().join("source-build");
        let definition = root.path().join("service");
        fs::write(&installed, "old executable").unwrap();
        fs::write(&source, "other executable").unwrap();
        assert!(validate_managed_installation(&installed, &installed, &definition).is_err());
        fs::write(&definition, "service definition").unwrap();
        assert!(validate_managed_installation(&installed, &installed, &definition).is_ok());
        assert!(validate_managed_installation(&source, &installed, &definition).is_err());
        fs::remove_file(&installed).unwrap();
        symlink(&source, &installed).unwrap();
        assert!(validate_managed_installation(&installed, &installed, &definition).is_err());
    }

    #[test]
    fn installer_pins_target_and_preserves_literal_custom_state_path() {
        let state = Path::new("/state with spaces/$(literal)`text`");
        let command = installer_command(state, "0.3.12").unwrap();
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(
            args,
            [
                "-s",
                "--",
                "--version",
                "v0.3.12",
                "--state-dir",
                state.to_str().unwrap()
            ]
        );
        for invalid in ["0.3.1-beta", "v0.3.1", "00.3.1", "0.3", "$(touch /bad)"] {
            assert!(installer_command(state, invalid).is_err());
        }
        assert!(installer_command(Path::new("relative"), "0.3.12").is_err());
    }

    #[test]
    fn installer_receives_bundled_script_and_failure_is_reported() {
        let root = tempfile::tempdir().unwrap();
        let shell = root.path().join("fixture-shell");
        let script_copy = root.path().join("received-script");
        fs::write(&shell, "#!/bin/sh\ncat > \"$1\"\nexit \"$2\"\n").unwrap();
        fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
        for (exit, success) in [("0", true), ("1", false)] {
            let mut command = Command::new(&shell);
            command.arg(&script_copy).arg(exit).stdin(Stdio::piped());
            assert_eq!(run_installer(command).is_ok(), success);
            assert_eq!(fs::read_to_string(&script_copy).unwrap(), INSTALLER);
        }
    }
}
