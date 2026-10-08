#![cfg(target_os = "macos")]

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

// Apple XNU bsd/sys/fcntl.h's FCNTLFLAGS: only these bits can be changed by
// F_SETFL. FWASWRITTEN (0x10000) is kernel bookkeeping set by any terminal write.
const SETTABLE_FLAGS: i32 =
    libc::O_APPEND | libc::O_ASYNC | libc::O_SYNC | libc::O_DSYNC | libc::O_NONBLOCK;

/// Every signal and teardown in this fixture targets only its own spawned Bree child.
struct PtyChild {
    master: Option<File>,
    slave: Option<File>,
    child: Child,
    initial_flags: i32,
    initial_modes: libc::termios,
}

impl PtyChild {
    fn new(rows: u16, columns: u16) -> Self {
        let (mut master_fd, mut slave_fd) = (-1, -1);
        let mut size = libc::winsize {
            ws_row: rows,
            ws_col: columns,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: openpty initializes two owned FDs; size points to a valid structure.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master_fd,
                    &mut slave_fd,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut size,
                )
            },
            0
        );
        let master = unsafe { File::from_raw_fd(master_fd) };
        let slave = unsafe { File::from_raw_fd(slave_fd) };
        // Keep the master out of the child so dropping it really produces terminal EOF.
        for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
            assert_eq!(
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
                0
            );
        }
        let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(
            unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
        let initial_flags = unsafe { libc::fcntl(slave.as_raw_fd(), libc::F_GETFL) };
        assert!(initial_flags >= 0);
        let mut initial_modes = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut initial_modes) },
            0
        );
        let mut command = Command::new(env!("CARGO_BIN_EXE_bree"));
        command
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env_remove("NO_COLOR")
            .env_remove("COLORFGBG")
            // Viewing and signal fixtures must not even read the user's rules.
            // Store observations do not create this absent private directory.
            .env(
                "BREE_DATA_DIR",
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join(".artifacts/stability")
                    .join(format!("signal-test-data-{}", std::process::id())),
            )
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()));
        let child = command.spawn().unwrap();
        Self {
            master: Some(master),
            slave: Some(slave),
            child,
            initial_flags,
            initial_modes,
        }
    }

    fn drain_until_started(&mut self) {
        // The desktop wordmark is drawn with block cells rather than literal text.
        // Wait for a stable home entry, which is also present before sampling finishes.
        // Ratatui can encode intervening blank cells as cursor movements.
        // run_internal installs ctrlc before entering raw mode and drawing this
        // frame. Readiness is therefore an upper bound for handler installation;
        // a fixed delay after spawn does not establish that the handler exists.
        self.drain_until(b"Clean");
    }

    fn drain_until(&mut self, marker: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut output = Vec::new();
        let mut buffer = [0_u8; 2048];
        while Instant::now() < deadline {
            match self.master.as_mut().unwrap().read(&mut buffer) {
                Ok(count) => {
                    output.extend_from_slice(&buffer[..count]);
                    if output.windows(marker.len()).any(|value| value == marker) {
                        return;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("private PTY read failed: {error}"),
            }
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "Bree exited before drawing"
            );
            thread::sleep(Duration::from_millis(5));
        }
        panic!("Bree did not draw the expected screen in the private PTY");
    }

    fn drain_for(&mut self, duration: Duration) {
        let deadline = Instant::now() + duration;
        let mut buffer = [0_u8; 8192];
        while Instant::now() < deadline {
            match self.master.as_mut().unwrap().read(&mut buffer) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("private PTY drain failed: {error}"),
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn exit_within(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("owned Bree child did not exit after a terminal signal");
    }

    fn assert_modes_and_flags_restored(&self) {
        let parent_fd = self.slave.as_ref().unwrap().as_raw_fd();
        let flags = unsafe { libc::fcntl(parent_fd, libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(
            flags & SETTABLE_FLAGS,
            self.initial_flags & SETTABLE_FLAGS,
            "all user-settable shared file-description flags must match the pre-spawn state"
        );
        let mut modes: libc::termios = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::tcgetattr(parent_fd, &mut modes) }, 0);
        assert_eq!(
            modes.c_iflag, self.initial_modes.c_iflag,
            "terminal input flags"
        );
        assert_eq!(
            modes.c_oflag, self.initial_modes.c_oflag,
            "terminal output flags"
        );
        assert_eq!(
            modes.c_cflag, self.initial_modes.c_cflag,
            "terminal control flags"
        );
        // Darwin's PENDIN is transient line-discipline state (SDK sys/termios.h),
        // not a raw/canonical/echo setting. Restoring canonical mode can set it.
        assert_eq!(
            modes.c_lflag & !libc::PENDIN,
            self.initial_modes.c_lflag & !libc::PENDIN,
            "terminal local flags"
        );
        assert_eq!(
            modes.c_cc, self.initial_modes.c_cc,
            "terminal control characters"
        );
        assert_eq!(
            modes.c_ispeed, self.initial_modes.c_ispeed,
            "terminal input speed"
        );
        assert_eq!(
            modes.c_ospeed, self.initial_modes.c_ospeed,
            "terminal output speed"
        );
    }

    fn exit_while_draining(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        let mut buffer = [0_u8; 8192];
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            match self.master.as_mut().unwrap().read(&mut buffer) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("private PTY read failed while exiting: {error}"),
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("owned Bree child did not exit after a keyboard cancellation");
    }
}

impl Drop for PtyChild {
    fn drop(&mut self) {
        // Release PTY endpoints before waiting, including on a test panic. Holding
        // an unread master while its child exits can block macOS terminal teardown.
        drop(self.master.take());
        drop(self.slave.take());
        if self.child.try_wait().ok().flatten().is_none() {
            // Failure cleanup only; this PID was created above and is never a user target.
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[test]
fn external_sigint_after_home_frame_restores_the_terminal() {
    // Exercise SIGINT itself, distinct from a raw Ctrl+C key event. Bound repeated
    // startup/teardown checks, and send only after the handler ordering is proven
    // by an observed first frame rather than relying on a scheduler-dependent sleep.
    for _ in 0..6 {
        let mut session = PtyChild::new(28, 100);
        session.drain_until_started();
        let mut modes: libc::termios = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::tcgetattr(session.slave.as_ref().unwrap().as_raw_fd(), &mut modes) },
            0
        );
        assert_eq!(modes.c_lflag & (libc::ICANON | libc::ECHO | libc::ISIG), 0);
        assert_eq!(
            unsafe { libc::kill(session.child.id() as i32, libc::SIGINT) },
            0
        );
        assert_eq!(
            session.exit_while_draining(Duration::from_secs(2)).code(),
            Some(130)
        );
        session.assert_modes_and_flags_restored();
    }
}

#[test]
fn terminal_hangup_during_initialization_is_an_error_or_signal_exit_not_a_panic() {
    for _ in 0..6 {
        let mut session = PtyChild::new(28, 100);
        session.drain_until_started();
        // The first skeleton can still be flushing or followed by another draw.
        // Closing the output here may therefore return a runtime I/O error before
        // a HUP can be delivered. A disconnected stderr must not turn it into 101.
        drop(session.master.take());
        let deadline = Instant::now() + Duration::from_millis(150);
        let status = loop {
            if let Some(status) = session.child.try_wait().unwrap() {
                break Some(status);
            }
            if Instant::now() >= deadline {
                break None;
            }
            thread::sleep(Duration::from_millis(5));
        };
        if let Some(status) = status {
            assert_eq!(
                status.code(),
                Some(1),
                "terminal loss before HUP is an I/O error"
            );
        } else {
            assert_eq!(
                unsafe { libc::kill(session.child.id() as i32, libc::SIGHUP) },
                0
            );
            let code = session.exit_within(Duration::from_secs(2)).code();
            assert!(
                matches!(code, Some(1 | 130)),
                "terminal I/O may finish between the poll and HUP; actual code: {code:?}"
            );
        }
    }
}

#[test]
fn terminal_hangup_exits_even_when_crossterm_receives_eof() {
    let mut session = PtyChild::new(28, 100);
    session.drain_until(b"GiB");
    // Allow initialization to finish, then remove the last master endpoint. This
    // private PTY is not a controlling terminal; send HUP explicitly after EOF so
    // the test proves the signal works even if the event reader never returns.
    session.drain_for(Duration::from_millis(100));
    drop(session.master.take());
    thread::sleep(Duration::from_millis(50));
    assert_eq!(
        unsafe { libc::kill(session.child.id() as i32, libc::SIGHUP) },
        0
    );
    assert_eq!(
        session.exit_within(Duration::from_secs(2)).code(),
        Some(130)
    );
}

#[test]
fn external_sigterm_exits_without_relying_on_a_reader_or_stdout_lock() {
    // The large first frame exceeds a PTY buffer if its reader stops draining.
    let mut session = PtyChild::new(120, 240);
    session.drain_until_started();
    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        unsafe { libc::kill(session.child.id() as i32, libc::SIGTERM) },
        0
    );
    assert_eq!(
        session.exit_within(Duration::from_secs(2)).code(),
        Some(130)
    );
    session.assert_modes_and_flags_restored();
}

#[test]
fn q_and_raw_control_c_restore_pre_spawn_modes_and_settable_flags() {
    for (keys, expected_exit) in [(b"q".as_slice(), 0), (b"\x03".as_slice(), 130)] {
        let mut session = PtyChild::new(28, 100);
        session.drain_until_started();
        session.master.as_mut().unwrap().write_all(keys).unwrap();
        assert_eq!(
            session.exit_while_draining(Duration::from_secs(2)).code(),
            Some(expected_exit)
        );
        session.assert_modes_and_flags_restored();
    }
}
