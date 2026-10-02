//! Bounded subprocess execution. Arguments are passed directly, never through a shell here.
use anyhow::Result;
use launcher_core::CancellationToken;
use std::{
    process::{Command, ExitStatus},
    time::Duration,
};

pub struct ProcessOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub fn run_bounded(
    command: &mut Command,
    token: &CancellationToken,
    timeout: Duration,
    max_bytes: usize,
) -> Result<ProcessOutput> {
    use anyhow::{Context, bail};
    use std::{io::Read, process::Stdio, sync::mpsc, time::Instant};
    if token.is_cancelled() {
        bail!("operation cancelled");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting subprocess")?;
    struct Guard(std::process::Child, bool);
    impl Drop for Guard {
        fn drop(&mut self) {
            if self.1 {
                #[cfg(unix)]
                unsafe {
                    libc::kill(-(self.0.id() as i32), libc::SIGKILL);
                }
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }
    let mut child = Guard(child, true);
    let stdout = child.0.stdout.take().context("opening stdout")?;
    let stderr = child.0.stderr.take().context("opening stderr")?;
    let (tx, rx) = mpsc::channel();
    for (is_stderr, mut stream) in [
        (false, Box::new(stdout) as Box<dyn Read + Send>),
        (true, Box::new(stderr) as Box<dyn Read + Send>),
    ] {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let result = (|| -> std::io::Result<Vec<u8>> {
                let mut bytes = Vec::new();
                let mut buffer = [0u8; 8192];
                loop {
                    let n = stream.read(&mut buffer)?;
                    if n == 0 {
                        break;
                    }
                    let keep = n.min(max_bytes.saturating_sub(bytes.len()));
                    bytes.extend_from_slice(&buffer[..keep]);
                }
                Ok(bytes)
            })();
            let _ = tx.send((is_stderr, result));
        });
    }
    drop(tx);
    let start = Instant::now();
    let mut status = None;
    let mut stdout = None;
    let mut stderr = None;
    loop {
        if token.is_cancelled() {
            bail!("operation cancelled");
        }
        if start.elapsed() >= timeout {
            bail!("operation timed out");
        }
        while let Ok((is_stderr, result)) = rx.try_recv() {
            let bytes = result.context("reading subprocess output")?;
            if is_stderr {
                stderr = Some(bytes);
            } else {
                stdout = Some(bytes);
            }
        }
        if status.is_none() {
            status = child.0.try_wait().context("waiting for subprocess")?;
        }
        if let (Some(status), Some(stdout), Some(stderr)) = (status, &mut stdout, &mut stderr) {
            child.1 = false;
            return Ok(ProcessOutput {
                status,
                stdout: std::mem::take(stdout),
                stderr: std::mem::take(stderr),
            });
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn captures_output_and_caps_each_stream() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf abcdef; printf error >&2"]);
        let out = run_bounded(
            &mut command,
            &CancellationToken::default(),
            Duration::from_secs(1),
            3,
        )
        .unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, b"abc");
        assert_eq!(out.stderr, b"err");
    }
    #[test]
    fn timeout_reaps_process_group_including_inherited_pipes() {
        let start = std::time::Instant::now();
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 20 & wait"]);
        let error = run_bounded(
            &mut command,
            &CancellationToken::default(),
            Duration::from_millis(50),
            10,
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(2));
    }
    #[test]
    fn cancelled_before_spawn_returns_without_execution() {
        let token = CancellationToken::default();
        token.cancel();
        assert!(
            run_bounded(
                &mut Command::new("/usr/bin/true"),
                &token,
                Duration::from_secs(1),
                10
            )
            .err()
            .unwrap()
            .to_string()
            .contains("cancelled")
        );
    }
}
