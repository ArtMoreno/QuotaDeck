//! The typed `quotadeck` command runs in the current terminal, never opens a
//! split. With no arguments it is the dashboard; any arguments go to the
//! plugin binary as they are (`quotadeck settings`, `quotadeck dashboard
//! --json`, `quotadeck refresh --provider all --force`), with the plugin's
//! state and config directories already in the environment.
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

#[cfg(windows)]
const MARKER: &str = "rem QuotaDeck managed in-pane launcher";
#[cfg(not(windows))]
const MARKER: &str = "# QuotaDeck managed in-pane launcher";

fn launcher_path(home: &Path) -> PathBuf {
    if cfg!(windows) {
        home.join(".local/bin/quotadeck.cmd")
    } else {
        home.join(".local/bin/quotadeck")
    }
}

#[cfg(windows)]
fn quote(path: &Path) -> String {
    // cmd.exe cannot launch Windows' verbatim paths returned by current_exe.
    let path = path.to_string_lossy();
    let path = if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        path.strip_prefix(r"\\?\").unwrap_or(&path).to_string()
    };
    path.replace('%', "%%")
}

/// A POSIX single-quoted word: the only character that needs care is the
/// quote itself.
#[cfg(not(windows))]
fn quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

pub fn apply() -> Result<()> {
    let Some(config) = std::env::var_os("HERDR_PLUGIN_CONFIG_DIR") else {
        return Ok(());
    };
    let home = crate::platform::home_dir()?;
    let cache = crate::cache::CacheStore::from_env()?;
    let executable = std::env::current_exe()?;
    #[cfg(windows)]
    let executable = executable.with_extension("exe");
    install(
        &launcher_path(&home),
        &executable,
        cache.root(),
        Path::new(&config),
    )
}

fn install(path: &Path, executable: &Path, state: &Path, config: &Path) -> Result<()> {
    let previous = match fs::read_to_string(path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).context("read existing QuotaDeck command"),
    };
    if !(previous.is_empty()
        || previous.contains(MARKER)
        || previous.contains("herdr-agent-quota-win") && previous.contains("open-dashboard-split"))
    {
        anyhow::bail!("{} is a custom command; preserve it and rename it before installing QuotaDeck's command", path.display());
    }
    fs::create_dir_all(path.parent().context("launcher directory")?)?;
    fs::write(path, body(executable, state, config)).context("write QuotaDeck command")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .context("mark QuotaDeck command executable")?;
    }
    println!(
        "QuotaDeck command: {} (runs in the current pane; q returns to the shell).",
        path.display()
    );
    if let Some(directory) = path.parent() {
        if !directory_on_path(directory) {
            println!(
                "{} is not on PATH here; run the command by that path, or add the directory to your shell's PATH.",
                directory.display()
            );
        }
    }
    Ok(())
}

/// Whether `directory` is in this process's PATH. The configure action runs
/// in Herdr's environment, which is usually the user's login environment,
/// so a miss here is a fair warning even if a particular shell differs.
fn directory_on_path(directory: &Path) -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|entry| entry == directory))
        .unwrap_or(false)
}

#[cfg(windows)]
fn body(executable: &Path, state: &Path, config: &Path) -> String {
    format!(
        "@echo off\r\n{MARKER}\r\nsetlocal DisableDelayedExpansion\r\nset \"HERDR_PLUGIN_STATE_DIR={}\"\r\nset \"HERDR_PLUGIN_CONFIG_DIR={}\"\r\nif \"%~1\"==\"\" (\r\n\"{exe}\" dashboard\r\n) else (\r\n\"{exe}\" %*\r\n)\r\nexit /b %errorlevel%\r\n",
        quote(state),
        quote(config),
        exe = quote(executable),
    )
}

#[cfg(not(windows))]
fn body(executable: &Path, state: &Path, config: &Path) -> String {
    format!(
        "#!/bin/sh\n{MARKER}\nHERDR_PLUGIN_STATE_DIR={}\nHERDR_PLUGIN_CONFIG_DIR={}\nexport HERDR_PLUGIN_STATE_DIR HERDR_PLUGIN_CONFIG_DIR\n[ \"$#\" -eq 0 ] && set -- dashboard\nexec {} \"$@\"\n",
        quote(state),
        quote(config),
        quote(executable),
    )
}

pub fn uninstall() -> Result<()> {
    if std::env::var_os("HERDR_PLUGIN_CONFIG_DIR").is_none() {
        return Ok(());
    }
    let path = launcher_path(&crate::platform::home_dir()?);
    if fs::read_to_string(&path).is_ok_and(|body| body.contains(MARKER)) {
        fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn typed_command_runs_dashboard_directly_preserves_errors_and_custom_commands() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("space & 100% ! directory");
        fs::create_dir_all(&bin).unwrap();
        let exe = bin.join("collector.cmd");
        fs::write(&exe, "@echo off\r\nif not \"%1\"==\"dashboard\" exit /b 9\r\nif not defined HERDR_PLUGIN_STATE_DIR exit /b 8\r\nif not defined HERDR_PLUGIN_CONFIG_DIR exit /b 7\r\nexit /b 23\r\n").unwrap();
        let launcher = bin.join("quotadeck.cmd");
        fs::write(
            &launcher,
            "herdr plugin action invoke open-dashboard-split --plugin herdr-agent-quota-win",
        )
        .unwrap();
        install(&launcher, &fs::canonicalize(&exe).unwrap(), &bin, &bin).unwrap();
        let output = std::process::Command::new(&launcher).output().unwrap();
        assert_eq!(output.status.code(), Some(23), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        let output = std::process::Command::new(&launcher)
            .arg("settings")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(9), "{output:?}");
        fs::write(&launcher, "user custom command").unwrap();
        assert!(install(&launcher, &exe, &bin, &bin).is_err());
        assert_eq!(fs::read_to_string(launcher).unwrap(), "user custom command");
    }

    #[cfg(unix)]
    #[test]
    fn typed_command_runs_dashboard_forwards_arguments_and_keeps_custom_commands() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("space & it's 100% ! directory");
        fs::create_dir_all(&bin).unwrap();
        let exe = bin.join("collector");
        fs::write(
            &exe,
            "#!/bin/sh\n[ -n \"$HERDR_PLUGIN_STATE_DIR\" ] || exit 8\n[ -n \"$HERDR_PLUGIN_CONFIG_DIR\" ] || exit 7\n[ \"$1\" = dashboard ] && [ \"$2\" = --json ] && exit 25\n[ \"$1\" = dashboard ] && exit 23\n[ \"$1\" = settings ] && [ \"$#\" -eq 1 ] && exit 24\nexit 9\n",
        )
        .unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        let launcher = bin.join("quotadeck");
        fs::write(
            &launcher,
            "herdr plugin action invoke open-dashboard-split --plugin herdr-agent-quota-win",
        )
        .unwrap();
        install(&launcher, &exe, &bin, &bin).unwrap();
        let run = |arguments: &[&str]| {
            std::process::Command::new(&launcher)
                .args(arguments)
                .output()
                .unwrap()
        };
        let output = run(&[]);
        assert_eq!(output.status.code(), Some(23), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        assert_eq!(run(&["settings"]).status.code(), Some(24));
        assert_eq!(run(&["dashboard", "--json"]).status.code(), Some(25));
        let body = fs::read_to_string(&launcher).unwrap();
        assert!(body.starts_with("#!/bin/sh\n"), "{body}");
        assert!(body.contains(MARKER), "{body}");
        assert!(body.contains("it'\\''s"), "{body}");
        fs::write(&launcher, "user custom command").unwrap();
        assert!(install(&launcher, &exe, &bin, &bin).is_err());
        assert_eq!(fs::read_to_string(launcher).unwrap(), "user custom command");
    }
}
