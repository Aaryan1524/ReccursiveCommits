//! Bounded curl I/O: credentials remain on stdin, and all three pipes share one deadline.

use std::{
    io::{self, Read, Write},
    os::fd::AsFd,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

pub(crate) const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 64 * 1024;

pub(crate) enum Error {
    Io(io::Error),
    TimedOut,
    OutputTooLarge,
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub(crate) fn run_curl(
    arguments: &[String],
    config: &[u8],
    timeout: Duration,
) -> Result<Output, Error> {
    if timeout.is_zero() {
        return Err(Error::TimedOut);
    }
    let deadline = Instant::now() + timeout;
    let mut child = Command::new("curl")
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let result = (|| {
        let mut stdin = Some(child.stdin.take().expect("piped stdin"));
        set_nonblocking(stdin.as_ref().expect("stdin still open"))?;
        let mut stdout = Collector::new(
            child.stdout.take().expect("piped stdout"),
            MAX_RESPONSE_BYTES,
        )?;
        let mut stderr =
            Collector::new(child.stderr.take().expect("piped stderr"), MAX_ERROR_BYTES)?;
        let mut written = 0;
        let mut status = None;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::TimedOut);
            }
            let mut progressed = false;
            if let Some(input) = stdin.as_mut() {
                if written < config.len() {
                    let end = config.len().min(written + 8192);
                    match input.write(&config[written..end]) {
                        Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero).into()),
                        Ok(count) => {
                            written += count;
                            progressed = true;
                        }
                        Err(error)
                            if matches!(
                                error.kind(),
                                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                            ) => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                if written == config.len() {
                    // EOF tells curl its explicit configuration is complete.
                    stdin.take();
                }
            }
            progressed |= stdout.drain()? | stderr.drain()?;
            if status.is_none() {
                status = child.try_wait()?;
            }
            if let Some(status) = status
                && stdout.eof
                && stderr.eof
            {
                return Ok(Output {
                    status,
                    stdout: stdout.bytes,
                    stderr: stderr.bytes,
                });
            }
            if !progressed {
                thread::sleep(remaining.min(Duration::from_millis(10)));
            }
        }
    })();
    if result.is_err() {
        // Close all pipes and reap on timeout, output overflow, or I/O failure.
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

fn set_nonblocking(fd: &impl AsFd) -> io::Result<()> {
    let flags = rustix::fs::fcntl_getfl(fd)?;
    rustix::fs::fcntl_setfl(fd, flags | rustix::fs::OFlags::NONBLOCK)?;
    Ok(())
}

struct Collector<R> {
    reader: R,
    bytes: Vec<u8>,
    maximum: usize,
    eof: bool,
}

impl<R: Read + AsFd> Collector<R> {
    fn new(reader: R, maximum: usize) -> io::Result<Self> {
        set_nonblocking(&reader)?;
        Ok(Self {
            reader,
            bytes: Vec::new(),
            maximum,
            eof: false,
        })
    }

    fn drain(&mut self) -> Result<bool, Error> {
        let mut progressed = false;
        let mut chunk = [0; 8192];
        // Bound each pass so neither a busy pipe nor stdin can starve deadline checks.
        for _ in 0..8 {
            if self.eof {
                break;
            }
            match self.reader.read(&mut chunk) {
                Ok(0) => self.eof = true,
                Ok(count) => {
                    if count > self.maximum.saturating_sub(self.bytes.len()) {
                        return Err(Error::OutputTooLarge);
                    }
                    self.bytes.extend_from_slice(&chunk[..count]);
                    progressed = true;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Ok(progressed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn collector_accepts_the_exact_limit_but_rejects_the_next_byte() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        let mut collector = Collector::new(reader, 16).unwrap();
        writer.write_all(&[b'x'; 16]).unwrap();
        assert!(matches!(collector.drain(), Ok(true)));
        assert_eq!(collector.bytes, vec![b'x'; 16]);
        writer.write_all(b"x").unwrap();
        assert!(matches!(collector.drain(), Err(Error::OutputTooLarge)));
        assert_eq!(collector.bytes.len(), 16);
    }

    #[test]
    fn a_zero_timeout_does_not_start_curl() {
        assert!(matches!(
            run_curl(&[], b"", Duration::ZERO),
            Err(Error::TimedOut)
        ));
    }
}
