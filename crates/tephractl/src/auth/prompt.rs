//! Reading a token from the terminal with echo off, or from a pipe.
//!
//! The hidden read puts the terminal in a no-echo, non-canonical mode
//! with signal keys off, so Ctrl-C arrives as the byte 0x03 and is
//! handled here instead of killing the process with echo still off. An
//! [`EchoGuard`] restores the saved terminal state on every way out of
//! the read: a normal return, Ctrl-C, end of input, a read or write
//! error, a panic.
//!
//! What it does not cover: a signal sent from another process
//! (`kill -TERM`) ends the process with the terminal as the read left
//! it; `reset` or `stty sane` fixes that.

use std::fmt;
use std::io::{self, BufRead, Read, Write};
use std::os::fd::RawFd;

/// The longest token (or pasted text) read: far above any real token.
pub const MAX_TOKEN_INPUT: usize = 64 * 1024;

/// Why a read of a token failed. Carries no part of the input.
#[derive(Debug)]
pub enum InputError {
    /// The descriptor is not a terminal.
    NotATerminal,
    /// Ctrl-C at the prompt.
    Interrupted,
    /// End of input before anything was typed.
    Eof,
    /// More than [`MAX_TOKEN_INPUT`] bytes.
    TooLong,
    /// The input was not UTF-8 text.
    NotText,
    Io(io::Error),
}

impl fmt::Display for InputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InputError::NotATerminal => f.write_str("stdin is not a terminal"),
            InputError::Interrupted => f.write_str("interrupted"),
            InputError::Eof => f.write_str("no token was entered"),
            InputError::TooLong => f.write_str("the input is too long to be a token"),
            InputError::NotText => f.write_str("the input is not text"),
            InputError::Io(e) => write!(f, "cannot read the token: {}", e.kind()),
        }
    }
}

impl std::error::Error for InputError {}

/// Where `auth github` gets its token. Real: [`StdinInput`]; tests give
/// their own.
pub trait TokenInput {
    /// Whether stdin is a terminal (so the hidden prompt is possible).
    fn is_terminal(&self) -> bool;
    /// Prompts and reads one line with echo off.
    fn read_hidden(&mut self, prompt: &str) -> Result<String, InputError>;
    /// Reads one line from a pipe, without its line ending.
    fn read_line(&mut self) -> Result<String, InputError>;
}

/// Process stdin.
pub struct StdinInput;

impl TokenInput for StdinInput {
    fn is_terminal(&self) -> bool {
        // SAFETY: `isatty` only inspects the descriptor.
        unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
    }

    fn read_hidden(&mut self, prompt: &str) -> Result<String, InputError> {
        read_hidden_fd(libc::STDIN_FILENO, prompt, &mut io::stderr())
    }

    fn read_line(&mut self) -> Result<String, InputError> {
        read_line_from(&mut io::stdin().lock())
    }
}

/// Reads one line, at most [`MAX_TOKEN_INPUT`] bytes, and strips its
/// `\n` or `\r\n` (and nothing else: a space stays, so the caller can
/// refuse it). End of input with no bytes is [`InputError::Eof`].
pub fn read_line_from(reader: &mut dyn BufRead) -> Result<String, InputError> {
    let mut buf = Vec::new();
    let read = reader
        .take(MAX_TOKEN_INPUT as u64 + 1)
        .read_until(b'\n', &mut buf)
        .map_err(InputError::Io)?;
    if read == 0 {
        return Err(InputError::Eof);
    }
    if buf.len() > MAX_TOKEN_INPUT {
        zero(&mut buf);
        return Err(InputError::TooLong);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
    }
    into_text(buf)
}

fn zero(buf: &mut [u8]) {
    buf.iter_mut().for_each(|b| *b = 0);
    std::hint::black_box(&*buf);
}

fn into_text(mut buf: Vec<u8>) -> Result<String, InputError> {
    match String::from_utf8(buf) {
        Ok(text) => Ok(text),
        Err(e) => {
            buf = e.into_bytes();
            zero(&mut buf);
            Err(InputError::NotText)
        }
    }
}

/// Restores the terminal state it saved, when dropped. It restores at
/// once (`TCSANOW`) rather than after pending output drains: a terminal
/// that has stopped reading (Ctrl-S, a hung emulator) must not be able to
/// keep echo off by blocking the restore.
pub struct EchoGuard {
    fd: RawFd,
    saved: libc::termios,
}

impl Drop for EchoGuard {
    fn drop(&mut self) {
        // SAFETY: `saved` came from `tcgetattr` on this descriptor.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved);
        }
    }
}

impl EchoGuard {
    /// Switches `fd` to no echo, no line buffering, no signal keys, and
    /// returns the guard that puts it back.
    pub fn new(fd: RawFd) -> Result<EchoGuard, InputError> {
        // SAFETY: `isatty` only inspects the descriptor.
        if unsafe { libc::isatty(fd) } != 1 {
            return Err(InputError::NotATerminal);
        }
        // SAFETY: a zeroed termios is a valid out-parameter for tcgetattr.
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: `saved` is a valid pointer for the call.
        if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
            return Err(InputError::Io(io::Error::last_os_error()));
        }
        let mut raw = saved;
        raw.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG | libc::IEXTEN);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        // SAFETY: `raw` is a valid termios.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(InputError::Io(io::Error::last_os_error()));
        }
        Ok(EchoGuard { fd, saved })
    }
}

fn read_byte(fd: RawFd) -> io::Result<Option<u8>> {
    let mut byte = 0u8;
    loop {
        // SAFETY: reads at most one byte into `byte`.
        let n = unsafe { libc::read(fd, (&mut byte as *mut u8).cast(), 1) };
        match n {
            1 => return Ok(Some(byte)),
            0 => return Ok(None),
            _ => {
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::Interrupted {
                    return Err(e);
                }
            }
        }
    }
}

/// Writes `prompt` to `prompt_out`, then reads one line from the
/// terminal `fd` with echo off. Enter ends it; Backspace and Ctrl-U edit
/// it; Ctrl-C is [`InputError::Interrupted`]; Ctrl-D on an empty line is
/// [`InputError::Eof`]. The terminal is restored on every return.
pub fn read_hidden_fd(
    fd: RawFd,
    prompt: &str,
    prompt_out: &mut dyn Write,
) -> Result<String, InputError> {
    let _guard = EchoGuard::new(fd)?;
    prompt_out
        .write_all(prompt.as_bytes())
        .and_then(|_| prompt_out.flush())
        .map_err(InputError::Io)?;
    let mut buf: Vec<u8> = Vec::new();
    let outcome = loop {
        match read_byte(fd) {
            Err(e) => break Err(InputError::Io(e)),
            Ok(None) => {
                if buf.is_empty() {
                    break Err(InputError::Eof);
                }
                break Ok(());
            }
            Ok(Some(b'\n')) | Ok(Some(b'\r')) => break Ok(()),
            Ok(Some(0x03)) => break Err(InputError::Interrupted),
            Ok(Some(0x04)) => {
                if buf.is_empty() {
                    break Err(InputError::Eof);
                }
            }
            Ok(Some(0x15)) => zero_clear(&mut buf),
            Ok(Some(0x7f)) | Ok(Some(0x08)) => {
                while buf.last().is_some_and(|b| (0x80..0xC0).contains(b)) {
                    buf.pop();
                }
                buf.pop();
            }
            Ok(Some(b)) => {
                if buf.len() >= MAX_TOKEN_INPUT {
                    break Err(InputError::TooLong);
                }
                buf.push(b);
            }
        }
    };
    // The newline the terminal would have echoed. A failure here is
    // reported, and the guard still restores the terminal.
    let newline = prompt_out
        .write_all(b"\n")
        .and_then(|_| prompt_out.flush())
        .map_err(InputError::Io);
    match outcome.and(newline) {
        Ok(()) => into_text(buf),
        Err(e) => {
            zero(&mut buf);
            Err(e)
        }
    }
}

fn zero_clear(buf: &mut Vec<u8>) {
    zero(buf);
    buf.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::os::fd::AsRawFd;

    #[test]
    fn read_line_strips_lf_and_crlf_only() {
        assert_eq!(read_line_from(&mut Cursor::new("abc\n")).unwrap(), "abc");
        assert_eq!(read_line_from(&mut Cursor::new("abc\r\n")).unwrap(), "abc");
        assert_eq!(read_line_from(&mut Cursor::new("abc")).unwrap(), "abc");
        assert_eq!(read_line_from(&mut Cursor::new("a b \n")).unwrap(), "a b ");
        assert_eq!(read_line_from(&mut Cursor::new("a\nb\n")).unwrap(), "a");
        assert_eq!(read_line_from(&mut Cursor::new("\n")).unwrap(), "");
    }

    #[test]
    fn read_line_on_empty_input_is_eof_and_never_hangs() {
        assert!(matches!(
            read_line_from(&mut Cursor::new("")),
            Err(InputError::Eof)
        ));
    }

    #[test]
    fn read_line_refuses_an_oversized_line_and_non_text() {
        let big = vec![b'a'; MAX_TOKEN_INPUT + 10];
        assert!(matches!(
            read_line_from(&mut Cursor::new(big)),
            Err(InputError::TooLong)
        ));
        assert!(matches!(
            read_line_from(&mut Cursor::new(vec![0xff, 0xfe, b'\n'])),
            Err(InputError::NotText)
        ));
    }

    /// A pty pair: `(master, slave)`, both closed on drop.
    struct Pty {
        master: std::fs::File,
        slave: std::fs::File,
    }

    fn pty() -> Pty {
        use std::os::fd::FromRawFd;
        let (mut m, mut s) = (0, 0);
        // SAFETY: valid out-pointers; null name/termios/winsize are allowed.
        let rc = unsafe {
            libc::openpty(
                &mut m,
                &mut s,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, 0, "openpty");
        // SAFETY: openpty returned two fresh descriptors we now own.
        unsafe {
            Pty {
                master: std::fs::File::from_raw_fd(m),
                slave: std::fs::File::from_raw_fd(s),
            }
        }
    }

    /// The local-mode bits this module changes. (The kernel sets other
    /// bits, such as `PENDIN`, by itself when the mode changes.)
    fn lflag(fd: RawFd) -> libc::tcflag_t {
        // SAFETY: zeroed termios as an out-parameter.
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::tcgetattr(fd, &mut t) }, 0);
        t.c_lflag & (libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG | libc::IEXTEN)
    }

    fn type_into(master: &std::fs::File, bytes: &[u8]) {
        // Writes to the master reach the slave as typed input. The
        // slave's mode is set before the prompt is written, so wait for
        // the prompt to be sure the read is in its no-echo mode.
        let mut m = master;
        m.write_all(bytes).unwrap();
    }

    /// Reads from the master until the pty has been quiet for 200 ms.
    fn drain_master(master: &std::fs::File) -> Vec<u8> {
        let mut out = Vec::new();
        let fd = master.as_raw_fd();
        loop {
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd.
            let ready = unsafe { libc::poll(&mut pfd, 1, 200) };
            if ready <= 0 {
                return out;
            }
            let mut chunk = [0u8; 256];
            // SAFETY: reads into a local buffer.
            let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
            if n <= 0 {
                return out;
            }
            out.extend_from_slice(&chunk[..n as usize]);
        }
    }

    fn wait_for_prompt(master: &std::fs::File, prompt: &str) {
        let mut seen = Vec::new();
        for _ in 0..50 {
            seen.extend(drain_master(master));
            if String::from_utf8_lossy(&seen).contains(prompt) {
                return;
            }
        }
        panic!("prompt never appeared");
    }

    #[test]
    fn hidden_read_returns_the_typed_line_without_echoing_it() {
        let p = pty();
        let before = lflag(p.slave.as_raw_fd());
        assert_ne!(before & libc::ECHO, 0, "a fresh pty echoes");
        let slave_fd = p.slave.as_raw_fd();
        let mut slave_out = p.slave.try_clone().unwrap();
        let reader = std::thread::spawn(move || {
            read_hidden_fd(slave_fd, "token: ", &mut slave_out).map(|t| (t, slave_out))
        });
        wait_for_prompt(&p.master, "token: ");
        assert_eq!(lflag(p.slave.as_raw_fd()) & libc::ECHO, 0, "echo is off");
        type_into(&p.master, b"ghp_SENT\x7fINEL\n");
        let (token, _keep) = reader.join().unwrap().unwrap();
        assert_eq!(token, "ghp_SENINEL");
        let shown = String::from_utf8_lossy(&drain_master(&p.master)).into_owned();
        assert!(!shown.contains("ghp_"), "typed text was echoed: {shown:?}");
        assert!(!shown.contains("SEN"), "typed text was echoed: {shown:?}");
        assert_eq!(lflag(p.slave.as_raw_fd()), before, "terminal restored");
    }

    #[test]
    fn ctrl_c_ends_the_read_and_restores_the_terminal() {
        let p = pty();
        let before = lflag(p.slave.as_raw_fd());
        let slave_fd = p.slave.as_raw_fd();
        let mut slave_out = p.slave.try_clone().unwrap();
        let reader = std::thread::spawn(move || {
            read_hidden_fd(slave_fd, "token: ", &mut slave_out).map(|_| ())
        });
        wait_for_prompt(&p.master, "token: ");
        type_into(&p.master, b"abc\x03");
        let result = reader.join().unwrap();
        assert!(matches!(result, Err(InputError::Interrupted)), "{result:?}");
        assert_eq!(lflag(p.slave.as_raw_fd()), before, "echo restored");
    }

    #[test]
    fn ctrl_d_on_an_empty_line_is_eof_and_restores_the_terminal() {
        let p = pty();
        let before = lflag(p.slave.as_raw_fd());
        let slave_fd = p.slave.as_raw_fd();
        let mut slave_out = p.slave.try_clone().unwrap();
        let reader = std::thread::spawn(move || {
            read_hidden_fd(slave_fd, "token: ", &mut slave_out).map(|_| ())
        });
        wait_for_prompt(&p.master, "token: ");
        type_into(&p.master, b"\x04");
        assert!(matches!(reader.join().unwrap(), Err(InputError::Eof)));
        assert_eq!(lflag(p.slave.as_raw_fd()), before);
    }

    struct FailingWriter;
    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_write_error_during_the_prompt_restores_the_terminal() {
        let p = pty();
        let before = lflag(p.slave.as_raw_fd());
        let result = read_hidden_fd(p.slave.as_raw_fd(), "token: ", &mut FailingWriter);
        assert!(matches!(result, Err(InputError::Io(_))), "{result:?}");
        assert_eq!(lflag(p.slave.as_raw_fd()), before, "echo restored");
    }

    #[test]
    fn a_panic_while_the_guard_is_held_restores_the_terminal() {
        let p = pty();
        let before = lflag(p.slave.as_raw_fd());
        let fd = p.slave.as_raw_fd();
        let outcome = std::panic::catch_unwind(|| {
            let _guard = EchoGuard::new(fd).unwrap();
            assert_eq!(lflag(fd) & libc::ECHO, 0);
            panic!("boom");
        });
        assert!(outcome.is_err());
        assert_eq!(lflag(fd), before);
    }

    #[test]
    fn a_descriptor_that_is_not_a_terminal_is_refused_without_reading() {
        let file = std::fs::File::open("/dev/null").unwrap();
        let result = read_hidden_fd(file.as_raw_fd(), "x", &mut io::sink());
        assert!(
            matches!(result, Err(InputError::NotATerminal)),
            "{result:?}"
        );
    }
}
