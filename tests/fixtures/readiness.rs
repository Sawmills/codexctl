//! JSON readiness and owned cleanup for synthetic test processes only.
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    os::fd::AsRawFd,
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

use serde_json::Value;

const MAX_DIAGNOSTIC_BYTES: u64 = 4096;

#[derive(Debug)]
pub struct StartupFailure {
    pub reason: &'static str,
    pub pid: u32,
    pub status: Option<ExitStatus>,
    pub stderr: String,
    pub io_error: Option<std::io::Error>,
}

impl StartupFailure {
    pub fn bind_conflict(&self) -> bool {
        self.reason == "eof"
            && self.status.is_some_and(|status| !status.success())
            && self.stderr.contains("Address already in use")
    }
}

impl std::fmt::Display for StartupFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "startup {} (pid {}, status {:?}): {}",
            self.reason, self.pid, self.status, self.stderr
        )?;
        if let Some(error) = &self.io_error {
            write!(f, " ({error})")?;
        }
        Ok(())
    }
}

pub struct ReadyChild {
    child: Option<Child>,
    pub ready: Value,
}

impl ReadyChild {
    pub fn start(
        command: &mut Command,
        log: &Path,
        budget: Duration,
    ) -> Result<Self, StartupFailure> {
        let stderr = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .map_err(|error| StartupFailure {
                reason: "stderr_open",
                pid: 0,
                status: None,
                stderr: String::new(),
                io_error: Some(error),
            })?;
        let offset = stderr
            .metadata()
            .map_err(|error| StartupFailure {
                reason: "stderr_metadata",
                pid: 0,
                status: None,
                stderr: String::new(),
                io_error: Some(error),
            })?
            .len();
        let child = command
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .map_err(|error| StartupFailure {
                reason: "spawn",
                pid: 0,
                status: None,
                stderr: String::new(),
                io_error: Some(error),
            })?;
        let mut running = Self {
            child: Some(child),
            ready: Value::Null,
        };
        let mut output = running.child.as_mut().unwrap().stdout.take().unwrap();
        let flags = unsafe { libc::fcntl(output.as_raw_fd(), libc::F_GETFL) };
        if flags == -1
            || unsafe { libc::fcntl(output.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                == -1
        {
            let error = std::io::Error::last_os_error();
            let mut failure = running.failure("stdout_setup", log, offset);
            failure.io_error = Some(error);
            return Err(failure);
        }
        let deadline = Instant::now() + budget;
        let mut line = Vec::new();
        loop {
            if Instant::now() >= deadline {
                return Err(running.failure("deadline", log, offset));
            }
            let mut buffer = [0_u8; 1024];
            match output.read(&mut buffer) {
                Ok(0) => return Err(running.failure("eof", log, offset)),
                Ok(count) => {
                    line.extend_from_slice(&buffer[..count]);
                    if line.len() > MAX_DIAGNOSTIC_BYTES as usize {
                        return Err(running.failure("readiness_too_large", log, offset));
                    }
                    if let Some(end) = line.iter().position(|byte| *byte == b'\n') {
                        match serde_json::from_slice(&line[..end]) {
                            Ok(ready) => {
                                running.ready = ready;
                                return Ok(running);
                            }
                            Err(_) => return Err(running.failure("invalid_json", log, offset)),
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if running
                        .child
                        .as_mut()
                        .unwrap()
                        .try_wait()
                        .ok()
                        .flatten()
                        .is_some()
                    {
                        return Err(running.failure("eof", log, offset));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => {
                    let mut failure = running.failure("stdout_read", log, offset);
                    failure.io_error = Some(error);
                    return Err(failure);
                }
            }
        }
    }

    pub fn into_child(mut self) -> Child {
        self.child.take().unwrap()
    }

    pub fn pid(&self) -> u32 {
        self.child.as_ref().unwrap().id()
    }

    fn failure(&mut self, reason: &'static str, log: &Path, offset: u64) -> StartupFailure {
        let child = self.child.as_mut().unwrap();
        let pid = child.id();
        let mut io_error = None;
        let status = match child.try_wait() {
            Ok(Some(status)) => Some(status),
            observed => {
                if let Err(error) = observed {
                    io_error = Some(error);
                }
                let _ = child.kill();
                match child.wait() {
                    Ok(status) => Some(status),
                    Err(error) => {
                        io_error = Some(error);
                        None
                    }
                }
            }
        };
        self.child.take();
        let stderr = (|| -> std::io::Result<Vec<u8>> {
            let mut file = File::open(log)?;
            let start = file
                .metadata()?
                .len()
                .saturating_sub(MAX_DIAGNOSTIC_BYTES)
                .max(offset);
            file.seek(SeekFrom::Start(start))?;
            let mut bytes = Vec::new();
            file.take(MAX_DIAGNOSTIC_BYTES).read_to_end(&mut bytes)?;
            Ok(bytes)
        })();
        let bytes = match stderr {
            Ok(bytes) => bytes,
            Err(error) => {
                io_error = Some(error);
                Vec::new()
            }
        };
        StartupFailure {
            reason,
            pid,
            status,
            stderr: String::from_utf8_lossy(&bytes).into_owned(),
            io_error,
        }
    }
}

impl Drop for ReadyChild {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
