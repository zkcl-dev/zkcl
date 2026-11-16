use std::io::{self, BufRead, Cursor, Read};
use std::sync::atomic::{AtomicBool, Ordering};

use zkcl::CancellationHandle;

static READING_LINE: AtomicBool = AtomicBool::new(false);

pub(super) fn waiting_for_line() -> bool {
    READING_LINE.load(Ordering::Acquire)
}

struct ReadingLine;

impl Drop for ReadingLine {
    fn drop(&mut self) {
        READING_LINE.store(false, Ordering::Release);
    }
}

pub(super) struct TerminalInput {
    cancellation: CancellationHandle,
    pending: Cursor<Vec<u8>>,
}

impl TerminalInput {
    pub(super) fn new(cancellation: CancellationHandle) -> Self {
        Self {
            cancellation,
            pending: Cursor::default(),
        }
    }
}

impl Read for TerminalInput {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let count = self.fill_buf()?.read(output)?;
        self.consume(count);
        Ok(count)
    }
}

impl BufRead for TerminalInput {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.pending.position() as usize == self.pending.get_ref().len() {
            self.pending = Cursor::new(
                read_line(&self.cancellation)?
                    .unwrap_or_default()
                    .into_bytes(),
            );
        }
        self.pending.fill_buf()
    }

    fn consume(&mut self, count: usize) {
        self.pending.consume(count);
    }
}

pub(super) fn read_line(cancellation: &CancellationHandle) -> io::Result<Option<String>> {
    READING_LINE.store(true, Ordering::Release);
    let _reading = ReadingLine;
    let input = io::stdin();
    #[cfg(unix)]
    {
        use rustix::event::{PollFd, PollFlags, Timespec, poll};
        let mut fds = [PollFd::new(&input, PollFlags::IN)];
        let timeout = Timespec {
            tv_sec: 0,
            tv_nsec: 50_000_000,
        };
        loop {
            if cancellation.is_cancelled() {
                return Err(io::Error::other("terminal input interrupted"));
            }
            match poll(&mut fds, Some(&timeout)) {
                Ok(0) | Err(rustix::io::Errno::INTR) => {}
                Ok(_) => break,
                Err(error) => return Err(error.into()),
            }
        }
    }
    if cancellation.is_cancelled() {
        return Err(io::Error::other("terminal input interrupted"));
    }
    let mut line = String::new();
    Ok((input.read_line(&mut line)? != 0).then_some(line))
}
