use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const IO_CHUNK_BYTES: usize = 8 * 1024;
const IO_OPERATIONS_PER_TURN: usize = 8;

pub struct ProcessOutput {
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

pub struct ProcessOptions<'a> {
    pub cwd: Option<&'a std::path::Path>,
    pub timeout: Duration,
    pub input: Option<&'a [u8]>,
    pub env: Option<&'a HashMap<String, String>>,
    pub max_output: usize,
    pub inherit_output: bool,
}

/// Run a subprocess with one deadline covering spawn, input, execution, and pipe draining.
///
/// The child is placed in a fresh process group. On timeout we kill that group and close
/// our pipe ends. A descendant that deliberately creates another process group cannot keep
/// this function blocked by retaining an inherited pipe.
pub fn run(program: &str, args: &[String], options: ProcessOptions<'_>) -> ProcessOutput {
    let deadline = Instant::now() + options.timeout;
    let mut command = Command::new(program);
    command.args(args).stdin(if options.input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    if options.inherit_output {
        command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    } else {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
    }
    if let Some(cwd) = options.cwd {
        command.current_dir(cwd);
    }
    if let Some(env) = options.env {
        command.envs(env);
    }
    command.process_group(0);

    let Ok(mut child) = command.spawn() else {
        return ProcessOutput {
            status: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            timed_out: false,
            stdout_truncated: false,
            stderr_truncated: false,
        };
    };

    if options.inherit_output {
        return wait_without_pipes(&mut child, deadline);
    }

    let mut stdin = child.stdin.take();
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    for fd in stdin
        .as_ref()
        .map(AsRawFd::as_raw_fd)
        .into_iter()
        .chain(stdout.as_ref().map(AsRawFd::as_raw_fd))
        .chain(stderr.as_ref().map(AsRawFd::as_raw_fd))
    {
        if set_nonblocking(fd).is_err() {
            terminate(&mut child);
            return ProcessOutput {
                status: None,
                stdout: Vec::new(),
                stderr: Vec::new(),
                timed_out: false,
                stdout_truncated: false,
                stderr_truncated: false,
            };
        }
    }

    let input = options.input.unwrap_or_default();
    let mut input_offset = 0;
    let mut stdout_bytes = Vec::with_capacity(options.max_output.min(8 * 1024));
    let mut stderr_bytes = Vec::with_capacity(options.max_output.min(8 * 1024));
    let mut stdout_truncated = false;
    let mut stderr_truncated = false;
    let mut status = None;
    let mut process_reaped = false;

    loop {
        if let Some(stream) = stdout.as_mut()
            && drain(
                stream,
                &mut stdout_bytes,
                options.max_output,
                &mut stdout_truncated,
                deadline,
            )
        {
            stdout = None;
        }
        if let Some(stream) = stderr.as_mut()
            && drain(
                stream,
                &mut stderr_bytes,
                options.max_output,
                &mut stderr_truncated,
                deadline,
            )
        {
            stderr = None;
        }
        if let Some(stream) = stdin.as_mut()
            && (input_offset == input.len()
                || write_input(stream, input, &mut input_offset, deadline))
        {
            stdin = None;
        }

        if !process_reaped {
            match child.try_wait() {
                Ok(Some(exit)) => {
                    status = exit.code();
                    process_reaped = true;
                    // Retire ordinary descendants still holding our output pipes.
                    kill_group(child.id());
                }
                Ok(None) => {}
                Err(_) => {
                    terminate(&mut child);
                    return ProcessOutput {
                        status: None,
                        stdout: stdout_bytes,
                        stderr: stderr_bytes,
                        timed_out: false,
                        stdout_truncated,
                        stderr_truncated,
                    };
                }
            }
        }

        let now = Instant::now();
        if now >= deadline {
            if !process_reaped {
                terminate(&mut child);
                status = child.try_wait().ok().flatten().and_then(|exit| exit.code());
            }
            // Closing local pipe ends is what bounds escaped descendants that inherited them.
            drop(stdin.take());
            drop(stdout.take());
            drop(stderr.take());
            return ProcessOutput {
                status,
                stdout: stdout_bytes,
                stderr: stderr_bytes,
                timed_out: true,
                stdout_truncated,
                stderr_truncated,
            };
        }

        if process_reaped && stdout.is_none() && stderr.is_none() {
            return ProcessOutput {
                status,
                stdout: stdout_bytes,
                stderr: stderr_bytes,
                timed_out: false,
                stdout_truncated,
                stderr_truncated,
            };
        }

        poll_pipes(
            stdin.as_ref(),
            stdout.as_ref(),
            stderr.as_ref(),
            deadline
                .saturating_duration_since(now)
                .min(Duration::from_millis(25)),
        );
    }
}

fn wait_without_pipes(child: &mut Child, deadline: Instant) -> ProcessOutput {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                kill_group(child.id());
                return ProcessOutput {
                    status: status.code(),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                    timed_out: Instant::now() >= deadline,
                    stdout_truncated: false,
                    stderr_truncated: false,
                };
            }
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(10)),
                );
            }
            Ok(None) => {
                terminate(child);
                return ProcessOutput {
                    status: child
                        .try_wait()
                        .ok()
                        .flatten()
                        .and_then(|status| status.code()),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                    timed_out: true,
                    stdout_truncated: false,
                    stderr_truncated: false,
                };
            }
            Err(_) => {
                terminate(child);
                return ProcessOutput {
                    status: None,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                    timed_out: false,
                    stdout_truncated: false,
                    stderr_truncated: false,
                };
            }
        }
    }
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: `fd` comes from a live ChildStdin/Stdout/Stderr owned by this function.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: F_SETFL updates flags on the same live descriptor.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn drain<R: Read>(
    stream: &mut R,
    retained: &mut Vec<u8>,
    limit: usize,
    truncated: &mut bool,
    deadline: Instant,
) -> bool {
    let mut buffer = [0_u8; IO_CHUNK_BYTES];
    for _ in 0..IO_OPERATIONS_PER_TURN {
        if Instant::now() >= deadline {
            return false;
        }
        match stream.read(&mut buffer) {
            Ok(0) => return true,
            Ok(read) => {
                let keep = read.min(limit.saturating_sub(retained.len()));
                if retained.try_reserve_exact(keep).is_ok() {
                    retained.extend_from_slice(&buffer[..keep]);
                } else {
                    *truncated = true;
                }
                *truncated |= keep < read;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return false,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return true,
        }
    }
    false
}

fn write_input(
    stream: &mut ChildStdin,
    input: &[u8],
    offset: &mut usize,
    deadline: Instant,
) -> bool {
    for _ in 0..IO_OPERATIONS_PER_TURN {
        if Instant::now() >= deadline {
            return false;
        }
        let end = input.len().min(offset.saturating_add(IO_CHUNK_BYTES));
        match stream.write(&input[*offset..end]) {
            Ok(0) => return true,
            Ok(written) => {
                *offset += written;
                if *offset == input.len() {
                    return true;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return false,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return true,
        }
    }
    false
}

fn poll_pipes(
    stdin: Option<&ChildStdin>,
    stdout: Option<&ChildStdout>,
    stderr: Option<&ChildStderr>,
    duration: Duration,
) {
    let mut descriptors = Vec::with_capacity(3);
    if let Some(stream) = stdin {
        descriptors.push(libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        });
    }
    for fd in [
        stdout.map(AsRawFd::as_raw_fd),
        stderr.map(AsRawFd::as_raw_fd),
    ]
    .into_iter()
    .flatten()
    {
        descriptors.push(libc::pollfd {
            fd,
            events: libc::POLLIN | libc::POLLHUP,
            revents: 0,
        });
    }
    let milliseconds = duration.as_millis().min(i32::MAX as u128) as i32;
    if descriptors.is_empty() {
        thread::sleep(duration);
    } else {
        // SAFETY: the vector owns a valid contiguous array for this call.
        unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as libc::nfds_t,
                milliseconds,
            );
        }
    }
}

fn kill_group(pid: u32) {
    // SAFETY: a negative pid addresses the process group created for this child.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

fn terminate(child: &mut Child) {
    kill_group(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_output_yields_after_bounded_progress_and_respects_deadline() {
        let mut retained = Vec::new();
        let mut truncated = false;
        let mut source = io::repeat(1).take((IO_CHUNK_BYTES * IO_OPERATIONS_PER_TURN * 2) as u64);
        assert!(!drain(
            &mut source,
            &mut retained,
            1024,
            &mut truncated,
            Instant::now() + Duration::from_secs(1),
        ));
        assert_eq!(retained.len(), 1024);
        assert!(truncated);
        assert!(source.limit() > 0, "busy output must yield to other I/O");

        let remaining = source.limit();
        assert!(!drain(
            &mut source,
            &mut retained,
            1024,
            &mut truncated,
            Instant::now(),
        ));
        assert_eq!(source.limit(), remaining, "expired calls must not read");
    }

    #[test]
    fn timeout_includes_pipe_drain_from_escaped_descendant() {
        let started = Instant::now();
        let output = run(
            "sh",
            &["-c".into(), "setsid sleep 2 & sleep 5".into()],
            ProcessOptions {
                cwd: None,
                timeout: Duration::from_millis(100),
                input: None,
                env: None,
                max_output: 1024,
                inherit_output: false,
            },
        );
        assert!(output.timed_out);
        assert!(started.elapsed() < Duration::from_millis(600));
    }
}
