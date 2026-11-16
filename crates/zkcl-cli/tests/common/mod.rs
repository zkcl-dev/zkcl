//! Shared helpers for CLI tests that spawn the binary.

#![allow(dead_code)]

use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub fn zkcl() -> Command {
    Command::new(env!("CARGO_BIN_EXE_zkcl"))
}

/// Owns a REPL process so a failing test cannot leave it running. `Drop` closes
/// the session's input, gives it a bounded chance to unwind, and only then
/// reclaims it by force.
pub struct ReplSession {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
}

impl ReplSession {
    pub fn spawn(command: &mut Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        Self {
            child,
            stdin,
            stdout,
        }
    }

    /// Hand the client pipes to the test; the process still ends with the test.
    pub fn pipes(&mut self) -> (ChildStdin, ChildStdout) {
        (self.stdin.take().unwrap(), self.stdout.take().unwrap())
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn disconnect(&mut self) {
        self.stdin.take();
    }

    pub fn exited_within(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait().expect("session state is readable") {
                Some(_) => return true,
                None if Instant::now() >= deadline => return false,
                None => thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    /// Close the client's end and reap the session, reclaiming it if it hangs.
    pub fn wait(&mut self) -> ExitStatus {
        self.disconnect();
        if !self.exited_within(Duration::from_secs(10)) {
            let _ = self.child.kill();
        }
        self.child.wait().expect("session is reapable")
    }
}

impl Drop for ReplSession {
    /// A session that no longer answers is reclaimed rather than left running;
    /// the bound only decides how long it may take to unwind on its own.
    fn drop(&mut self) {
        self.disconnect();
        if !self.exited_within(Duration::from_secs(5)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
