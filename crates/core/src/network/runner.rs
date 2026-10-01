//! Console programs run to completion or to a deadline, with their output captured and
//! decoded from the OEM code page they write redirected output in.

use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::win::console_text::{decode_code_page, oem_code_page};
use crate::win::process::{hardened_command, CREATE_NO_WINDOW};
use crate::Result;

/// Interval between checks whether the process has exited.
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// How long the output readers get to reach the end of the pipes once the process has
/// exited or was killed. A grandchild that inherited a pipe can keep it open for longer.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
/// Output kept per stream; older output is dropped first.
const MAX_CAPTURE_BYTES: usize = 1024 * 1024;

/// What a finished (or abandoned) command wrote and how it ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CommandOutput {
    /// `None` when the command did not finish before its deadline.
    pub exit_code: Option<i32>,
    /// Decoded text with `\n` line ends.
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

/// What happens to a command that is still running at its deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OnTimeout {
    /// Terminate the process and wait for it to exit.
    #[cfg_attr(not(test), allow(dead_code))]
    Kill,
    /// Return at once with the output read so far; the process finishes on its own.
    Leave,
}

/// Runs console programs. Production uses [`SystemRunner`]; tests substitute a fake.
pub(crate) trait CommandRunner {
    fn run(
        &self,
        program: &Path,
        args: &[&str],
        timeout: Duration,
        on_timeout: OnTimeout,
    ) -> Result<CommandOutput>;
}

/// Starts real processes through [`hardened_command`], without a console window.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(
        &self,
        program: &Path,
        args: &[&str],
        timeout: Duration,
        on_timeout: OnTimeout,
    ) -> Result<CommandOutput> {
        let mut command = hardened_command(program)?;
        command
            .args(args)
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        let stdout = Capture::start(child.stdout.take());
        let stderr = Capture::start(child.stderr.take());
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(CommandOutput {
                    exit_code: status.code(),
                    stdout: stdout.finish(DRAIN_TIMEOUT),
                    stderr: stderr.finish(DRAIN_TIMEOUT),
                    timed_out: false,
                });
            }
            if Instant::now() >= deadline {
                let drain = match on_timeout {
                    OnTimeout::Kill => {
                        let _ = child.kill();
                        let _ = child.wait();
                        DRAIN_TIMEOUT
                    }
                    OnTimeout::Leave => Duration::ZERO,
                };
                return Ok(CommandOutput {
                    exit_code: None,
                    stdout: stdout.finish(drain),
                    stderr: stderr.finish(drain),
                    timed_out: true,
                });
            }
            thread::sleep(POLL_INTERVAL);
        }
    }
}

/// One output pipe drained by its own thread into a shared buffer, so a full pipe never
/// blocks the child and a deadline never waits for a read.
struct Capture {
    bytes: Arc<Mutex<Vec<u8>>>,
    reader: Option<JoinHandle<()>>,
}

impl Capture {
    fn start<R: Read + Send + 'static>(source: Option<R>) -> Capture {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let reader = source.map(|mut source| {
            let sink = Arc::clone(&bytes);
            thread::spawn(move || {
                let mut chunk = [0u8; 4096];
                loop {
                    match source.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let mut buf = sink.lock();
                            buf.extend_from_slice(&chunk[..n]);
                            if buf.len() > 2 * MAX_CAPTURE_BYTES {
                                let excess = buf.len() - MAX_CAPTURE_BYTES;
                                buf.drain(..excess);
                            }
                        }
                    }
                }
            })
        });
        Capture { bytes, reader }
    }

    /// Waits up to `wait` for the reader to reach the end of the pipe, then decodes what
    /// has arrived. A reader still running afterwards keeps draining on its own.
    fn finish(self, wait: Duration) -> String {
        if let Some(reader) = self.reader {
            let deadline = Instant::now() + wait;
            while !reader.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            if reader.is_finished() {
                let _ = reader.join();
            }
        }
        let bytes = self.bytes.lock().clone();
        decode_output(&bytes)
    }
}

/// OEM code page text (or UTF-8) with CRLF line ends turned into LF.
fn decode_output(bytes: &[u8]) -> String {
    decode_code_page(bytes, oem_code_page()).replace("\r\n", "\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win::paths::system_dir;

    fn system_program(name: &str) -> std::path::PathBuf {
        system_dir().unwrap().join(name)
    }

    #[test]
    fn runner_captures_output_and_exit_code() {
        let out = SystemRunner
            .run(
                &system_program("cmd.exe"),
                &["/d", "/c", "echo probe & exit /b 3"],
                Duration::from_secs(30),
                OnTimeout::Kill,
            )
            .unwrap();
        assert!(!out.timed_out);
        assert_eq!(out.exit_code, Some(3));
        assert_eq!(out.stdout.trim(), "probe");
        assert!(!out.stdout.contains('\r'), "{:?}", out.stdout);
        assert_eq!(out.stderr, "");
    }

    #[test]
    fn runner_kills_on_timeout_when_asked() {
        let started = Instant::now();
        let out = SystemRunner
            .run(
                &system_program("ping.exe"),
                &["-n", "30", "127.0.0.1"],
                Duration::from_secs(1),
                OnTimeout::Kill,
            )
            .unwrap();
        let elapsed = started.elapsed();
        assert!(out.timed_out);
        assert_eq!(out.exit_code, None);
        assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
    }

    #[test]
    fn runner_leaves_process_on_timeout_when_asked() {
        let started = Instant::now();
        let out = SystemRunner
            .run(
                &system_program("ping.exe"),
                &["-n", "3", "127.0.0.1"],
                Duration::from_secs(1),
                OnTimeout::Leave,
            )
            .unwrap();
        let elapsed = started.elapsed();
        assert!(out.timed_out);
        assert_eq!(out.exit_code, None);
        assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    }

    #[test]
    fn relative_program_paths_are_refused() {
        let err = SystemRunner
            .run(
                Path::new("cmd.exe"),
                &["/d", "/c", "exit /b 0"],
                Duration::from_secs(5),
                OnTimeout::Kill,
            )
            .unwrap_err();
        assert!(err.to_string().contains("absolute"), "{err}");
    }

    #[test]
    fn output_is_decoded_with_lf_line_ends() {
        assert_eq!(decode_output(b"a\r\nb\r\n"), "a\nb\n");
        assert_eq!(decode_output(b""), "");
    }
}
