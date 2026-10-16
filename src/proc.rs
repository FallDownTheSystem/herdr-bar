//! Running a helper program with a deadline, as `subprocess.run(timeout=...)`.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub struct Output {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub success: bool,
}

pub enum RunError {
    /// The program could not be started; `NotFound` when it does not exist.
    Spawn(std::io::Error),
    Timeout,
}

/// Run to completion or until `timeout` seconds pass, collecting both pipes.
pub fn run(argv: &[String], timeout: f64) -> Result<Output, RunError> {
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(RunError::Spawn)?;
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    // Drain stderr on a thread so a chatty program cannot fill the pipe and
    // stall while we read stdout.
    let err_thread = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(pipe) = stderr.as_mut() {
            let _ = pipe.read_to_end(&mut buffer);
        }
        buffer
    });
    let deadline = Instant::now() + Duration::from_secs_f64(timeout.max(0.0));
    let out_thread = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(pipe) = stdout.as_mut() {
            let _ = pipe.read_to_end(&mut buffer);
        }
        buffer
    });
    let exit = ExitWatch::new(&child);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => break None,
        }
        let now = Instant::now();
        if now >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(RunError::Timeout);
        }
        exit.wait(deadline - now);
    };
    let stdout = out_thread.join().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default();
    Ok(Output { stdout, stderr, success: status.map(|s| s.success()).unwrap_or(false) })
}

/// Sleeps until a child has probably exited, or `limit` passes.
///
/// On Linux a pidfd becomes readable the moment the child exits, so the
/// caller's `try_wait` sees it without delay. Elsewhere, or when pidfds are
/// unavailable, it naps for a millisecond and lets the caller look again.
/// Either way the caller's loop decides; this only saves it spinning.
struct ExitWatch {
    #[cfg(target_os = "linux")]
    fd: Option<std::os::fd::OwnedFd>,
}

impl ExitWatch {
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
    fn new(child: &std::process::Child) -> ExitWatch {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::FromRawFd;
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id() as libc::pid_t, 0) };
            let fd = if fd >= 0 { Some(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) }) } else { None };
            ExitWatch { fd }
        }
        #[cfg(not(target_os = "linux"))]
        {
            ExitWatch {}
        }
    }

    fn wait(&self, limit: Duration) {
        #[cfg(target_os = "linux")]
        if let Some(fd) = &self.fd {
            use std::os::fd::AsRawFd;
            let mut poll = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            let ms = limit.as_millis().clamp(1, i32::MAX as u128) as i32;
            let ready = unsafe { libc::poll(&mut poll, 1, ms) };
            if ready >= 0 {
                return;
            }
        }
        std::thread::sleep(limit.min(Duration::from_millis(1)));
    }
}

/// `run`, folding every failure into None.
pub fn run_capture(argv: &[String], timeout: f64) -> Option<Output> {
    run(argv, timeout).ok()
}
