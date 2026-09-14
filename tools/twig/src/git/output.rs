//! Deadline-bounded Git capture. Drain both pipes before reaping the child.
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn capture(mut command: Command, timeout: Duration) -> io::Result<Output> {
    let (wake_read, wake_write) = UnixStream::pair()?;
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    if let Err(error) =
        nonblocking(stdout.as_raw_fd()).and_then(|_| nonblocking(stderr.as_raw_fd()))
    {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let pid = child.id();
    let (sender, receiver) = mpsc::channel();
    let waiter = match std::thread::Builder::new()
        .name("twig-git-wait".into())
        .spawn(move || {
            let result = loop {
                let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
                // WNOWAIT keeps the PID reserved until the owner reaps it. A
                // timeout can therefore never signal a subsequently reused PID.
                let status = unsafe {
                    libc::waitid(
                        libc::P_PID,
                        pid as libc::id_t,
                        info.as_mut_ptr(),
                        libc::WEXITED | libc::WNOWAIT,
                    )
                };
                if status == 0 {
                    break Ok(());
                }
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    break Err(error);
                }
            };
            let _ = sender.send(result);
            drop(wake_write); // POLLHUP wakes the owner without a polling interval.
        }) {
        Ok(waiter) => waiter,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    let result = drain(
        [
            stdout.as_raw_fd(),
            stderr.as_raw_fd(),
            wake_read.as_raw_fd(),
        ],
        receiver,
        timeout,
    );
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait();
    let _ = waiter.join();
    let [stdout, stderr] = result?;
    Ok(Output {
        status: status?,
        stdout,
        stderr,
    })
}

fn drain(
    fds: [RawFd; 3],
    receiver: mpsc::Receiver<io::Result<()>>,
    timeout: Duration,
) -> io::Result<[Vec<u8>; 2]> {
    let started = Instant::now();
    let mut fds = fds.map(|fd| libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    });
    let mut output = [Vec::new(), Vec::new()];
    let mut buffer = [0u8; 8192];
    while fds.iter().any(|fd| fd.fd >= 0) {
        let remaining = timeout
            .checked_sub(started.elapsed())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "git command timed out"))?;
        let millis = (remaining.as_millis() + 1).min(i32::MAX as u128) as i32;
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, millis) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if fds[2].revents != 0 {
            receiver
                .recv()
                .map_err(|_| io::Error::other("git waiter stopped"))??;
            fds[2].fd = -1;
        }
        for index in 0..2 {
            if fds[index].revents == 0 || fds[index].fd < 0 {
                continue;
            }
            // Bound each drain so a continuously writing child cannot prevent
            // servicing stderr or checking the deadline.
            for _ in 0..8 {
                let count =
                    unsafe { libc::read(fds[index].fd, buffer.as_mut_ptr().cast(), buffer.len()) };
                if count == 0 {
                    fds[index].fd = -1;
                    break;
                }
                if count > 0 {
                    output[index].extend_from_slice(&buffer[..count as usize]);
                    continue;
                }
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    break;
                }
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn large_stdout_and_stderr_do_not_fill_pipes_and_timeout() {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "head -c 262144 /dev/zero; head -c 262144 /dev/zero >&2",
        ]);
        let output = capture(command, Duration::from_secs(2)).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 262144);
        assert_eq!(output.stderr.len(), 262144);
    }
    #[test]
    fn deadline_applies_even_after_both_output_pipes_close() {
        let mut command = Command::new("sh");
        command.args(["-c", "exec 1>&- 2>&-; exec sleep 5"]);
        let start = Instant::now();
        let error = capture(command, Duration::from_millis(50)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(2));
    }
    #[test]
    fn exit_status_and_empty_output_are_preserved() {
        let mut command = Command::new("sh");
        command.args(["-c", "exit 7"]);
        let output = capture(command, Duration::from_secs(2)).unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert!(output.stdout.is_empty() && output.stderr.is_empty());
    }
}
