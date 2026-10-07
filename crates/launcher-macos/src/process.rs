//! Bounded subprocess execution. Arguments are passed directly, never through a shell here.
use anyhow::Result;
use launcher_core::CancellationToken;
use std::{
    process::{Command, ExitStatus},
    time::Duration,
};

#[derive(Debug)]
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
    run(command, token, timeout, max_bytes, None)
}

/// Feed a bounded candidate buffer without blocking cancellation on a full pipe.
pub fn run_bounded_with_input(
    command: &mut Command,
    token: &CancellationToken,
    timeout: Duration,
    max_bytes: usize,
    input: Vec<u8>,
) -> Result<ProcessOutput> {
    run(command, token, timeout, max_bytes, Some(input))
}

fn run(
    command: &mut Command,
    token: &CancellationToken,
    timeout: Duration,
    max_bytes: usize,
    input: Option<Vec<u8>>,
) -> Result<ProcessOutput> {
    use anyhow::{Context, bail};
    use std::{
        io::{Read, Write},
        process::Stdio,
        sync::mpsc,
        time::Instant,
    };
    if token.is_cancelled() {
        bail!("operation cancelled");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
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
    enum Stream {
        Stdout,
        Stderr,
        Input,
    }
    let (tx, rx) = mpsc::channel();
    let mut input_done = input.is_none();
    if let Some(input) = input {
        let mut stdin = child.0.stdin.take().context("opening stdin")?;
        let tx = tx.clone();
        std::thread::spawn(move || {
            let result = match stdin.write_all(&input) {
                Ok(()) => Ok(Vec::new()),
                // A filter may exit before reading everything, e.g. no matches.
                Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(Vec::new()),
                Err(error) => Err(error),
            };
            drop(stdin);
            let _ = tx.send((Stream::Input, result));
        });
    }
    for (kind, mut stream) in [
        (Stream::Stdout, Box::new(stdout) as Box<dyn Read + Send>),
        (Stream::Stderr, Box::new(stderr) as Box<dyn Read + Send>),
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
            let _ = tx.send((kind, result));
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
        while let Ok((kind, result)) = rx.try_recv() {
            let bytes = result.context("subprocess I/O")?;
            match kind {
                Stream::Stdout => stdout = Some(bytes),
                Stream::Stderr => stderr = Some(bytes),
                Stream::Input => input_done = true,
            }
        }
        if status.is_none() {
            status = child.0.try_wait().context("waiting for subprocess")?;
        }
        if let (Some(status), Some(stdout), Some(stderr), true) =
            (status, &mut stdout, &mut stderr, input_done)
        {
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
    fn pipe_input_preserves_nul_unicode_and_large_buffers_without_deadlock() {
        let bytes = "long filename 🦀\nwith newline\0"
            .repeat(12000)
            .into_bytes();
        let out = run_bounded_with_input(
            &mut Command::new("/bin/cat"),
            &CancellationToken::default(),
            Duration::from_secs(2),
            bytes.len(),
            bytes.clone(),
        )
        .unwrap();
        assert_eq!(out.stdout, bytes);
    }

    #[test]
    fn piped_input_is_bounded_when_the_child_never_reads() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 20"]);
        assert!(
            run_bounded_with_input(
                &mut command,
                &CancellationToken::default(),
                Duration::from_millis(50),
                10,
                vec![0; 1_000_000]
            )
            .unwrap_err()
            .to_string()
            .contains("timed out")
        );
    }

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
