//! A small choose-one list for the REPL: arrow keys (or j/k, or a number) and Enter
//! on a terminal; a numbered list and a typed answer when stdin isn't one.

use anyhow::Result;
use std::io::{IsTerminal, Read, Write};
use std::process::{Command, Stdio};

/// Pick one of `items`. `None` means the user cancelled.
pub fn pick(title: &str, items: &[String], start: usize) -> Result<Option<usize>> {
    if items.is_empty() {
        return Ok(None);
    }
    if std::io::stdin().is_terminal()
        && std::io::stderr().is_terminal()
        && let Some(raw) = Raw::enter()
    {
        return interactive(title, items, start.min(items.len() - 1), raw);
    }
    numbered(title, items)
}

fn numbered(title: &str, items: &[String]) -> Result<Option<usize>> {
    eprintln!("{title}");
    for (i, it) in items.iter().enumerate() {
        eprintln!("{:>3}. {it}", i + 1);
    }
    eprint!("\x1b[2mnumber (enter to cancel):\x1b[0m ");
    std::io::stderr().flush().ok();
    let line = match crate::input::read_line()? {
        crate::input::Input::Line(l) => l,
        _ => return Ok(None),
    };
    Ok(line
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|n| (1..=items.len()).contains(n))
        .map(|n| n - 1))
}

fn interactive(
    title: &str,
    items: &[String],
    mut sel: usize,
    mut raw: Raw,
) -> Result<Option<usize>> {
    let mut err = std::io::stderr();
    let (rows, cols) = size();
    // every row must be exactly one terminal line, or redrawing walks up too few lines
    let width = cols.saturating_sub(3).max(10);
    let shown = items.len().min(rows.saturating_sub(2).max(3));
    let mut top = 0;
    let more = if shown < items.len() {
        format!(" {}/{}", shown, items.len())
    } else {
        String::new()
    };
    write!(err, "\x1b[?7l").ok();
    let mut draw = |err: &mut std::io::Stderr, sel: usize, first: bool| {
        if sel < top {
            top = sel;
        } else if sel >= top + shown {
            top = sel + 1 - shown;
        }
        if !first {
            write!(err, "\x1b[{}A", shown + 1).ok();
        }
        write!(
            err,
            "\r\x1b[J{} \x1b[2m(↑/↓ enter · esc to cancel){more}\x1b[0m\r\n",
            clip(title, width)
        )
        .ok();
        for (i, it) in items.iter().enumerate().skip(top).take(shown) {
            let it = clip(it, width);
            if i == sel {
                write!(err, "\x1b[36m▸ {it}\x1b[0m\r\n").ok();
            } else {
                write!(err, "  {it}\r\n").ok();
            }
        }
        err.flush().ok();
    };
    draw(&mut err, sel, true);
    let picked = loop {
        match raw.key() {
            Key::Up => sel = sel.checked_sub(1).unwrap_or(items.len() - 1),
            Key::Down => sel = (sel + 1) % items.len(),
            Key::Digit(d) if (1..=items.len()).contains(&d) => {
                sel = d - 1;
                draw(&mut err, sel, false);
                break Some(sel);
            }
            Key::Enter => break Some(sel),
            Key::Cancel => break None,
            _ => continue,
        }
        draw(&mut err, sel, false);
    };
    // collapse the list to one line so scrollback stays readable
    write!(err, "\x1b[{}A\r\x1b[J\x1b[?7h", shown + 1).ok();
    err.flush().ok();
    Ok(picked)
}

/// Terminal (rows, columns).
fn size() -> (usize, usize) {
    stty(&["size"])
        .and_then(|s| {
            let mut it = s.split_whitespace().filter_map(|n| n.parse().ok());
            Some((it.next()?, it.next()?))
        })
        .filter(|&(r, c)| r > 0 && c > 0)
        .unwrap_or((24, 80))
}

/// At most `width` characters, with `…` when cut.
fn clip(s: &str, width: usize) -> String {
    let s = s.replace(['\n', '\r', '\t'], " ");
    if s.chars().count() <= width {
        return s;
    }
    let mut out: String = s.chars().take(width - 1).collect();
    out.push('…');
    out
}

enum Key {
    Up,
    Down,
    Enter,
    Cancel,
    Digit(usize),
    Other,
}

/// The terminal in no-echo, unbuffered mode; restored on drop.
struct Raw {
    saved: String,
    tty: std::fs::File,
}

impl Raw {
    fn enter() -> Option<Raw> {
        let saved = stty(&["-g"])?;
        stty(&["-icanon", "-echo", "-isig", "min", "0", "time", "1"])?;
        let tty = std::fs::File::open("/dev/tty").ok()?;
        Some(Raw {
            saved: saved.trim().to_string(),
            tty,
        })
    }

    /// Next byte, waiting for one. `None` after a short pause (lone Esc).
    fn byte(&mut self, wait: bool) -> Option<u8> {
        let mut b = [0u8];
        loop {
            match self.tty.read(&mut b) {
                Ok(1) => return Some(b[0]),
                Ok(_) if wait => continue,
                _ => return None,
            }
        }
    }

    fn key(&mut self) -> Key {
        match self.byte(true) {
            Some(b'\r' | b'\n') => Key::Enter,
            Some(b'k') => Key::Up,
            Some(b'j') => Key::Down,
            Some(b'q' | 3 | 4) => Key::Cancel,
            Some(d @ b'1'..=b'9') => Key::Digit((d - b'0') as usize),
            Some(27) => match (self.byte(false), self.byte(false)) {
                (None, _) => Key::Cancel,
                (Some(b'[' | b'O'), Some(b'A')) => Key::Up,
                (Some(b'[' | b'O'), Some(b'B')) => Key::Down,
                _ => Key::Other,
            },
            _ => Key::Other,
        }
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        stty(&[&self.saved]);
    }
}

fn stty(args: &[&str]) -> Option<String> {
    let tty = std::fs::File::open("/dev/tty").ok()?;
    let out = Command::new("stty")
        .args(args)
        .stdin(tty)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}
