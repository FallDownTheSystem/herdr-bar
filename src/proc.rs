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
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => break None,
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(RunError::Timeout);
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    let stdout = out_thread.join().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default();
    Ok(Output { stdout, stderr, success: status.map(|s| s.success()).unwrap_or(false) })
}

/// `run`, folding every failure into None.
pub fn run_capture(argv: &[String], timeout: f64) -> Option<Output> {
    run(argv, timeout).ok()
}
