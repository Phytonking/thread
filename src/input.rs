//! REPL input with Ctrl-C. SIGINT only sets a flag; lines are read from fd 0 through
//! `poll`, so a Ctrl-C at the prompt is seen within 100ms instead of after Enter.
//! Every REPL read goes through here (it keeps its own line buffer).

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

static HIT: AtomicBool = AtomicBool::new(false);
/// Set while quitting: another Ctrl-C exits at once.
static FORCE: AtomicBool = AtomicBool::new(false);
static BUF: Mutex<Vec<u8>> = Mutex::new(Vec::new());

pub enum Input {
    Line(String),
    Interrupt,
    Eof,
}

extern "C" fn on_sigint(_: libc::c_int) {
    if FORCE.load(Ordering::Relaxed) {
        unsafe { libc::_exit(130) }
    }
    HIT.store(true, Ordering::Relaxed);
}

pub fn install() {
    unsafe {
        libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t);
    }
}

/// Consume a pending Ctrl-C.
pub fn take() -> bool {
    HIT.swap(false, Ordering::Relaxed)
}

/// From now on Ctrl-C quits immediately (used once a hand-off has started).
pub fn force_next() {
    FORCE.store(true, Ordering::Relaxed);
}

/// The next line (without its newline), a Ctrl-C, or end of input.
pub fn read_line() -> std::io::Result<Input> {
    loop {
        {
            let mut buf = BUF.lock().unwrap();
            if let Some(i) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=i).collect();
                return Ok(Input::Line(
                    String::from_utf8_lossy(&line[..i]).into_owned(),
                ));
            }
        }
        if take() {
            return Ok(Input::Interrupt);
        }
        let mut p = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut p, 1, 100) } <= 0 {
            continue;
        }
        let mut chunk = [0u8; 4096];
        let n = unsafe { libc::read(0, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        let mut buf = BUF.lock().unwrap();
        if n == 0 {
            if buf.is_empty() {
                return Ok(Input::Eof);
            }
            let line = String::from_utf8_lossy(&buf).into_owned();
            buf.clear();
            return Ok(Input::Line(line));
        }
        buf.extend_from_slice(&chunk[..n as usize]);
    }
}
