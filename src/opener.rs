//! Open the bar's popup through the Herdr CLI: what `open-windows` runs.
//!
//! Herdr starts manifest commands without a shell, and Windows has no `sh`
//! for the one-liner behind `open`, so Windows has its own action and the CLI
//! call lives here.

pub const POPUP_ARGS: &[&str] =
    &["plugin", "pane", "open", "--plugin", "herdr-bar", "--entrypoint", "bar", "--placement", "popup"];

/// The Herdr binary to shell out to.
///
/// HERDR_BIN_PATH can outlive the binary it names -- upgrading Herdr under a
/// running server leaves the old path dangling -- so an unusable value falls
/// back to whatever `herdr` resolves to on PATH.
pub fn herdr_bin() -> String {
    if let Some(candidate) = std::env::var_os("HERDR_BIN_PATH") {
        if !candidate.is_empty() && is_executable_file(std::path::Path::new(&candidate)) {
            return candidate.to_string_lossy().into_owned();
        }
    }
    "herdr".into()
}

#[cfg(unix)]
pub fn is_executable_file(path: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(meta) = std::fs::metadata(path) else { return false };
    if !meta.is_file() {
        return false;
    }
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return false };
    unsafe { libc::access(c_path.as_ptr(), libc::X_OK) == 0 }
}

#[cfg(windows)]
pub fn is_executable_file(path: &std::path::Path) -> bool {
    // os.access(X_OK) on Windows only checks that the file exists.
    std::fs::metadata(path).map(|m| m.is_file()).unwrap_or(false)
}

pub fn popup_command() -> Vec<String> {
    let mut command = vec![herdr_bin()];
    command.extend(POPUP_ARGS.iter().map(|s| s.to_string()));
    command
}

pub fn open_bar() -> i32 {
    let command = popup_command();
    let mut process = std::process::Command::new(&command[0]);
    process.args(&command[1..]);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Replace this process, so nothing lingers behind the popup.
        let error = process.exec();
        eprint!("herdr-bar: could not run herdr: {}\n", crate::host::os_error_str(&error));
        2
    }
    #[cfg(not(unix))]
    {
        // No exec on Windows: wait, so the CLI's exit code reaches Herdr.
        match process.status() {
            Ok(status) => status.code().unwrap_or(1),
            Err(error) => {
                eprint!("herdr-bar: could not run herdr: {}\n", crate::host::os_error_str(&error));
                2
            }
        }
    }
}
