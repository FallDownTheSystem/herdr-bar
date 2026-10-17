//! One reconnecting background watcher per Herdr server, for tab titles and status.

use crate::auto_title::{title_state_dir, AutoTitles};
use crate::client::{EventStream, HerdrClient, HerdrError, Kind, Result};
use crate::config::Config;
use crate::host;
use crate::pyjson::Value;
use crate::renaming_guard::{check_plugins, installed_plugins, AutomaticTabClient};
use crate::tab_status::{Shared, TabStatus};
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const INTERVAL: f64 = 2.0;
const REGISTRATION_INTERVAL: f64 = 10.0;
const MAX_BACKOFF: f64 = 30.0;
/// In place of an inherited descriptor: the worker claims the lock file by name.
pub const LOCK_BY_NAME: &str = "-";

pub const BINARY: &str = if cfg!(windows) { "herdr-bar.exe" } else { "herdr-bar" };

fn open_append(path: &Path) -> std::result::Result<File, String> {
    OpenOptions::new().append(true).create(true).open(path).map_err(|e| host::os_error_path(&e, path))
}

fn mkdir(path: &Path) -> std::result::Result<(), String> {
    std::fs::create_dir_all(path).map_err(|e| host::os_error_path(&e, path))
}

/// The plugin directory this binary belongs to: the one Herdr names in
/// `HERDR_PLUGIN_ROOT`, else the nearest ancestor that holds the manifest.
///
/// The variable comes first so that a binary symlinked from, or copied out
/// of, another directory still belongs to the plugin that runs it.
pub fn plugin_root() -> PathBuf {
    if let Some(root) = std::env::var_os("HERDR_PLUGIN_ROOT").filter(|v| !v.is_empty()) {
        let root = PathBuf::from(root);
        if root.join("herdr-plugin.toml").is_file() {
            return std::fs::canonicalize(&root).unwrap_or(root);
        }
    }
    let exe = std::env::current_exe().ok().and_then(|p| std::fs::canonicalize(&p).ok().or(Some(p)));
    if let Some(exe) = exe {
        for ancestor in exe.ancestors().skip(1) {
            if ancestor.join("herdr-plugin.toml").is_file() {
                return ancestor.to_path_buf();
            }
        }
        if let Some(parent) = exe.parent() {
            return parent.to_path_buf();
        }
    }
    std::env::current_dir().unwrap_or_default()
}

/// Whether two plugin roots name the same directory.
fn same_root(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// A copy of `exe` in `directory` to run instead of it, named after its size
/// and modification time so a changed binary gets a fresh copy.
///
/// Windows cannot rename a directory while a process runs an image from it,
/// so a long-lived worker started from the plugin's own binary would make
/// every Herdr update of the plugin fail. The copy is written under a temp
/// name and renamed into place, so a half-written image is never run; older
/// copies are removed when nothing runs them any more.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn staged_binary(exe: &Path, directory: &Path) -> std::io::Result<PathBuf> {
    let meta = std::fs::metadata(exe)?;
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let suffix = std::env::consts::EXE_SUFFIX;
    let name = format!("herdr-bar-{}-{}{}", meta.len(), modified, suffix);
    let target = directory.join(&name);
    if !std::fs::metadata(&target).map(|m| m.len() == meta.len()).unwrap_or(false) {
        let temp = directory.join(format!("{}.{}.tmp", name, std::process::id()));
        let copied = std::fs::copy(exe, &temp).and_then(|_| std::fs::rename(&temp, &target));
        if let Err(error) = copied {
            let _ = std::fs::remove_file(&temp);
            // A racing launcher may have put the same copy in place.
            if !target.is_file() {
                return Err(error);
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(directory) {
        for entry in entries.flatten() {
            let file = entry.file_name();
            let file = file.to_string_lossy();
            if file != name && file.starts_with("herdr-bar-") && file.ends_with(suffix) && !file.ends_with(".tmp") {
                // Fails while an older worker still runs it; fine.
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    Ok(target)
}

/// How to start the watcher shipped in `root`: its binary, wherever the
/// build step put it, or the Python entry point of an older release.
fn entry_point(root: &Path) -> Vec<OsString> {
    for candidate in [root.join("bin").join(BINARY), root.join("target").join("release").join(BINARY)] {
        if candidate.is_file() {
            return vec![candidate.into_os_string()];
        }
    }
    vec![OsString::from("python3"), root.join("run.py").into_os_string()]
}

/// Best-effort launch; unavailable state storage must never break the popup.
pub fn start(resume: bool) {
    let titles = AutoTitles::new();
    let Some(directory) = titles.state_dir.clone() else { return };
    if std::env::var_os("HERDR_SOCKET_PATH").map(|v| v.is_empty()).unwrap_or(true) {
        return;
    }
    if !titles.enabled && !Config::load().tab_status {
        let records = TabStatus::load(Some(&directory.join("tab-status.json")));
        if !records.iter().any(|(_, record)| !record.get("yielded").truthy()) {
            return;
        }
    }
    let attempt = (|| -> std::result::Result<(), String> {
        mkdir(&directory)?;
        let gate_path = directory.join("title-launch.lock");
        let gate = open_append(&gate_path)?;
        if let Err(error) = host::lock(&gate, resume) {
            if host::is_busy(&error) {
                return Ok(());
            }
            return Err(host::os_error_str(&error));
        }
        launch(&directory, resume)
    })();
    if let Err(message) = attempt {
        eprintln!("herdr-bar: title watcher could not start: {}", message);
    }
}

fn launch(directory: &Path, resume: bool) -> std::result::Result<(), String> {
    let stopped = directory.join("title-watcher.stopped");
    if resume {
        if let Err(error) = std::fs::remove_file(&stopped) {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(host::os_error_path(&error, &stopped));
            }
        }
    } else if stopped.exists() {
        return Ok(());
    }
    let lock = open_append(&directory.join("title-watcher.lock"))?;
    if let Err(error) = host::lock(&lock, false) {
        if host::is_busy(&error) {
            return Ok(());
        }
        return Err(host::os_error_str(&error));
    }
    // Keep the singleton lock across spawn and any later exec replacement.
    let log = open_log(directory)?;
    let command = match std::env::current_exe() {
        Ok(exe) => vec![exe.into_os_string()],
        Err(_) => entry_point(&plugin_root()),
    };
    spawn(&command, &plugin_root(), directory, &log, Some(&lock))
}

fn open_log(directory: &Path) -> std::result::Result<File, String> {
    open_append(&directory.join("title-watcher.log"))
}

/// Start a detached worker that holds, or is about to claim, the lock.
///
/// The worker runs in its state directory, not in the plugin's: Windows does
/// not let Herdr replace a directory that a running process works in, so a
/// long-lived worker there would make every plugin update fail.
fn spawn(
    command: &[OsString],
    root: &Path,
    directory: &Path,
    log: &File,
    lock: Option<&File>,
) -> std::result::Result<(), String> {
    #[cfg(windows)]
    let staged: Vec<OsString> = match command {
        [exe] => match staged_binary(Path::new(exe), directory) {
            Ok(copy) => vec![copy.into_os_string()],
            Err(_) => command.to_vec(),
        },
        _ => command.to_vec(),
    };
    #[cfg(windows)]
    let command = &staged[..];
    let token = if host::WINDOWS {
        // CreateProcess cannot hand over a locked handle. Let go so the worker
        // can claim the file by name; a launcher racing into that gap starts a
        // second worker, which loses the claim and exits.
        if let Some(lock) = lock {
            host::unlock(lock);
        }
        LOCK_BY_NAME.to_string()
    } else {
        lock.map(|l| host::handle_number(l).to_string()).unwrap_or_else(|| LOCK_BY_NAME.to_string())
    };
    let mut process = Command::new(&command[0]);
    process.args(&command[1..]).arg("--watch-titles").arg(&token);
    process.current_dir(directory).stdin(Stdio::null()).env("HERDR_PLUGIN_ROOT", root);
    let out = log.try_clone().map_err(|e| host::os_error_str(&e))?;
    let err = log.try_clone().map_err(|e| host::os_error_str(&e))?;
    process.stdout(out).stderr(err);
    if let Some(lock) = lock {
        host::detach(&mut process, lock);
    } else {
        #[cfg(windows)]
        host::detach(&mut process, log);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let limit = host::fd_limit();
            unsafe {
                process.pre_exec(move || {
                    libc::setsid();
                    host::cloexec_all_except(-1, limit);
                    Ok(())
                });
            }
        }
    }
    process.spawn().map(|_| ()).map_err(|e| {
        let program = PathBuf::from(&command[0]);
        host::os_error_path(&e, &program)
    })
}

pub fn stop() -> std::result::Result<(), String> {
    if let Some(directory) = title_state_dir() {
        mkdir(&directory)?;
        let stopped = directory.join("title-watcher.stopped");
        open_append(&stopped)?;
    }
    Ok(())
}

/// Close the worker lock under the launch gate to make stop/resume atomic.
fn finish_if_stopped(lock: &mut Option<File>, directory: &Path, tabs: Option<&mut TabStatus>) -> bool {
    let stopped = directory.join("title-watcher.stopped");
    if !stopped.exists() {
        return false;
    }
    let Ok(gate) = OpenOptions::new().append(true).create(true).open(directory.join("title-launch.lock")) else {
        return false;
    };
    if host::lock(&gate, true).is_err() {
        return false;
    }
    if stopped.exists() {
        // Still holding the lock, so no successor can decorate meanwhile.
        if let Some(tabs) = tabs {
            tabs.clear();
        }
        *lock = None;
        return true;
    }
    false
}

/// Python's Path.resolve(): follow what exists, keep the rest as written.
fn resolve(path: &str) -> PathBuf {
    let absolute = PathBuf::from(crate::pyos::abspath(path));
    if let Ok(found) = std::fs::canonicalize(&absolute) {
        return found;
    }
    let mut existing = absolute.clone();
    let mut rest: Vec<OsString> = Vec::new();
    while let Some(name) = existing.file_name().map(|n| n.to_os_string()) {
        existing.pop();
        rest.push(name);
        if let Ok(found) = std::fs::canonicalize(&existing) {
            let mut out = found;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return out;
        }
    }
    absolute
}

/// Ask the server, including in named sessions and custom socket locations.
fn registered_root(client: &mut HerdrClient) -> Result<Option<PathBuf>> {
    let plugins = installed_plugins(client)?;
    check_plugins(&plugins, &Config::load().tab_renaming_plugins)?;
    for plugin in &plugins {
        if plugin.get("plugin_id").and_then(|v| v.as_str()) == Some("herdr-bar") {
            if !plugin.get("enabled").map(|v| v.truthy()).unwrap_or(false) {
                return Ok(None);
            }
            return match plugin.get("plugin_root") {
                Some(Value::Str(root)) if !root.is_empty() => Ok(Some(resolve(root))),
                _ => Err(HerdrError::new("plugin root missing from plugin.list")),
            };
        }
    }
    Ok(None)
}

type Revision = Vec<(PathBuf, i128, u64)>;

#[cfg(unix)]
fn stamp(meta: &std::fs::Metadata) -> i128 {
    use std::os::unix::fs::MetadataExt;
    meta.mtime() as i128 * 1_000_000_000 + meta.mtime_nsec() as i128
}

#[cfg(windows)]
fn stamp(meta: &std::fs::Metadata) -> i128 {
    use std::os::windows::fs::MetadataExt;
    meta.last_write_time() as i128
}

/// Detect in-place upgrades as well as installs that move the plugin root.
fn revision(root: &Path) -> Result<Revision> {
    let entry = entry_point(root);
    let mut files: Vec<PathBuf> = Vec::new();
    if entry.len() == 1 {
        files.push(PathBuf::from(&entry[0]));
    } else {
        files.push(root.join("run.py"));
        let mut sources: Vec<PathBuf> = std::fs::read_dir(root.join("src").join("herdr_bar"))
            .map(|entries| {
                entries.flatten().map(|e| e.path()).filter(|p| p.extension().map(|x| x == "py").unwrap_or(false)).collect()
            })
            .unwrap_or_default();
        sources.sort();
        files.extend(sources);
    }
    let mut out = Vec::new();
    for path in files {
        let meta = std::fs::metadata(&path).map_err(|e| HerdrError::new(host::os_error_path(&e, &path)))?;
        out.push((path, stamp(&meta), meta.len()));
    }
    Ok(out)
}

/// Hand this server's watcher over to the plugin files now registered.
fn replace(root: &Path, lock: &mut Option<File>, directory: &Path) -> Result<()> {
    let command = entry_point(root);
    if host::WINDOWS {
        // No exec on Windows. Release the lock, then start a worker that
        // claims it; a startup hook racing into the gap loses and exits.
        *lock = None;
        let log = open_log(directory).map_err(HerdrError::new)?;
        return spawn(&command, root, directory, &log, None).map_err(HerdrError::new);
    }
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        use std::os::unix::process::CommandExt;
        // exec keeps the lock held, so no startup hook can mistake this
        // handoff for a stopped worker, and the replacement cannot race an
        // old writer.
        let fd = lock.as_ref().map(|l| l.as_raw_fd()).unwrap_or(-1);
        // Nothing else this process holds belongs to the replacement.
        host::cloexec_all_except(fd, host::fd_limit());
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 {
                libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
            }
        }
        let error = Command::new(&command[0])
            .args(&command[1..])
            .arg("--watch-titles")
            .arg(fd.to_string())
            .env("HERDR_PLUGIN_ROOT", root)
            .exec();
        Err(HerdrError::os(&error))
    }
    #[cfg(not(unix))]
    {
        Ok(())
    }
}

/// Take the worker lock by name; None when another worker holds it.
fn claim(directory: &Path) -> std::result::Result<Option<File>, String> {
    mkdir(directory)?;
    let lock = open_append(&directory.join("title-watcher.lock"))?;
    match host::lock(&lock, false) {
        Ok(()) => Ok(Some(lock)),
        Err(error) if host::is_busy(&error) => Ok(None),
        Err(error) => Err(host::os_error_str(&error)),
    }
}

#[cfg(unix)]
fn adopt(fd: i64) -> Option<File> {
    use std::os::unix::io::FromRawFd;
    let fd = i32::try_from(fd).ok()?;
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
        return None;
    }
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
    }
    Some(unsafe { File::from_raw_fd(fd) })
}

#[cfg(windows)]
fn adopt(_fd: i64) -> Option<File> {
    None
}

fn monotonic() -> Instant {
    Instant::now()
}

fn secs(seconds: f64) -> Duration {
    Duration::from_secs_f64(seconds.max(0.0))
}

pub fn watch(lock_fd: Option<i64>) -> i32 {
    let client = HerdrClient::new(None, 2.0, true);
    let mut titles = AutoTitles::new();
    let Some(directory) = titles.state_dir.clone() else {
        if let Some(fd) = lock_fd {
            drop(adopt(fd));
        }
        return 0;
    };
    let lock = match lock_fd {
        Some(fd) => match adopt(fd) {
            Some(file) => Some(file),
            None => {
                eprintln!("herdr-bar: title watcher: [Errno 9] Bad file descriptor");
                return 1;
            }
        },
        None => match claim(&directory) {
            Ok(lock) => lock,
            Err(message) => {
                eprintln!("herdr-bar: title watcher: {}", message);
                return 1;
            }
        },
    };
    let mut lock = lock;
    if lock.is_none() {
        return 0;
    }
    #[cfg(windows)]
    {
        // A worker running the plugin's own binary would block Herdr's next
        // update of the plugin. An 0.8 watcher hands over that way, through
        // run.py, so move to a copy in the state directory first.
        let exe = std::env::current_exe().ok();
        let staged = exe.as_deref().is_some_and(|e| e.parent().is_some_and(|p| same_root(p, &directory)));
        if let (Some(exe), false) = (exe.as_deref(), staged) {
            if let Ok(copy) = staged_binary(exe, &directory) {
                drop(lock.take()); // let the copy claim the lock by name
                let started = open_log(&directory)
                    .and_then(|log| spawn(&[copy.into_os_string()], &plugin_root(), &directory, &log, None));
                if started.is_ok() {
                    return 0;
                }
                // Keep watching from here if the copy cannot start.
                lock = claim(&directory).ok().flatten();
                if lock.is_none() {
                    return 0;
                }
            }
        }
    }
    let socket_path = client.socket_path.clone();
    let mut tabs = TabStatus::new(AutomaticTabClient::new(client), Some(directory.clone()));
    // Status changes arrive as events, so idle tabs cost no polling beyond titles.
    let mut events = EventStream::new(socket_path, 2.0);
    let current_root = plugin_root();
    let current_revision = match revision(&current_root) {
        Ok(found) => found,
        Err(error) => {
            eprintln!("herdr-bar: title watcher: {}", error);
            return 1;
        }
    };
    let started = monotonic();
    let at = |offset: f64| started + secs(offset);
    let elapsed = || started.elapsed().as_secs_f64();
    let mut next_registration = 0.0f64;
    let mut next_poll = 0.0f64;
    let mut delay = INTERVAL;
    let mut last_error = String::new();
    loop {
        if finish_if_stopped(&mut lock, &directory, Some(&mut tabs)) {
            return 0;
        }
        let now = elapsed();
        let mut deadline = now + delay;
        let tick = (|| -> Result<Option<i32>> {
            if now >= next_registration {
                let root = registered_root(&mut tabs.client.client)?;
                let Some(root) = root else {
                    tabs.clear();
                    return Ok(Some(0));
                };
                // Same directory and same files: nothing to hand over to.
                if !same_root(&root, &current_root) || revision(&root)? != current_revision {
                    replace(&root, &mut lock, &directory)?;
                    return Ok(Some(0));
                }
                next_registration = now + REGISTRATION_INTERVAL;
            }
            let snapshot: Shared;
            if now >= next_poll {
                let config = Config::load();
                titles.configure(&config);
                tabs.configure(&config);
                if !(titles.enabled || tabs.enabled) {
                    tabs.clear();
                    events.close();
                    return Ok(Some(0));
                }
                let first = tabs.snapshot()?;
                snapshot = titles.apply(&mut tabs, first)?;
                next_poll = now + INTERVAL;
            } else {
                snapshot = tabs.snapshot()?;
            }
            deadline = next_poll;
            tabs.apply(&snapshot);
            if !tabs.client.conflict.is_empty() {
                return Err(HerdrError::renaming(tabs.client.conflict.clone()));
            }
            // e.g. a pane closed since the snapshot; polling covers it
            let _ = events.follow(&tabs.subscriptions());
            delay = INTERVAL;
            last_error.clear();
            Ok(None)
        })();
        match tick {
            Ok(Some(code)) => return code,
            Ok(None) => {}
            Err(error) if error.kind == Kind::Renaming => {
                // Do not even restore our prefixes: another writer owns names now.
                eprintln!("herdr-bar: {}", error);
                events.close();
                return 0;
            }
            Err(error) => {
                // Stay available across server shutdown/restart. Never fall back
                // to a CLI that might address a different server or spawn one.
                let message = error.message.clone();
                if message != last_error {
                    eprintln!("herdr-bar: title watcher retrying: {}", message);
                    last_error = message;
                }
                events.close();
                next_registration = 0.0;
                next_poll = 0.0;
                delay = MAX_BACKOFF.min(delay * 2.0);
                deadline = elapsed() + delay;
            }
        }
        // An explicit stop is noticed even while reconnecting with backoff.
        while monotonic() < at(deadline) {
            if finish_if_stopped(&mut lock, &directory, Some(&mut tabs)) {
                return 0;
            }
            let left = deadline - elapsed();
            if events.wait(left.clamp(0.0, 1.0)) {
                next_poll = 0.0;
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_binary_is_reused_and_replaced() {
        let base = std::env::temp_dir().join(format!("herdr-bar-stage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let state = base.join("state");
        std::fs::create_dir_all(&state).unwrap();
        let exe = base.join("source");
        std::fs::write(&exe, b"one").unwrap();
        let first = staged_binary(&exe, &state).unwrap();
        assert_eq!(std::fs::read(&first).unwrap(), b"one");
        assert_eq!(staged_binary(&exe, &state).unwrap(), first);
        std::fs::write(&exe, b"two, longer").unwrap();
        let second = staged_binary(&exe, &state).unwrap();
        assert_ne!(second, first);
        assert_eq!(std::fs::read(&second).unwrap(), b"two, longer");
        // The stale copy is gone and no temp file is left behind.
        let names: Vec<_> = std::fs::read_dir(&state).unwrap().flatten().map(|e| e.path()).collect();
        assert_eq!(names, vec![second]);
        assert!(staged_binary(&base.join("missing"), &state).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn roots_compare_through_links() {
        let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        assert!(same_root(&base, &std::env::temp_dir()));
        assert!(same_root(&base, &base.join(".")));
        assert!(!same_root(&base, &base.join("herdr-bar-no-such-dir")));
    }
}
