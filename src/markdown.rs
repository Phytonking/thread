//! Markdown → terminal text. Model replies are markdown; printed raw they show
//! literal `**`, backticks and fences, and one long line per paragraph that the
//! terminal soft-wraps (so copying it out glues words together at the wrap points).
//! This renders the common subset — headings, emphasis, inline code, fenced code,
//! lists, quotes, links, rules and GFM tables — and word-wraps prose to the terminal
//! width with real newlines. Hand-written on purpose: the dependency list stays short.
//!
//! Output modes (see [`for_fd`]):
//! - a TTY: styled with ANSI escapes and wrapped;
//! - a TTY with `NO_COLOR` set: the same layout with the markdown syntax stripped but
//!   no escapes;
//! - not a TTY (pipes, scripts, tests): the text passes through unchanged, so nothing
//!   downstream loses information.

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const ITALIC: &str = "\x1b[3m";
const STRIKE: &str = "\x1b[9m";
const CODE: &str = "\x1b[36m";
const HEADING: &str = "\x1b[1;35m";
const LINK: &str = "\x1b[4;34m";

/// Fallback width when the terminal size is unknown, and the readable maximum.
const DEFAULT_WIDTH: usize = 100;
const MAX_WIDTH: usize = 100;
const MIN_WIDTH: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Style {
    /// emit ANSI escapes
    pub color: bool,
    /// wrap prose to this many columns
    pub width: usize,
}

/// What to do with markdown headed for this file descriptor: `None` passes it through
/// raw (not a terminal), otherwise render it with the returned style.
pub fn for_fd(fd: i32) -> Option<Style> {
    // SAFETY: isatty only inspects the descriptor.
    if unsafe { libc::isatty(fd) } != 1 {
        return None;
    }
    let color = std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
    Some(Style {
        color,
        width: term_width(fd),
    })
}

fn term_width(fd: i32) -> usize {
    // SAFETY: TIOCGWINSZ fills a winsize struct we own.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let cols = if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0 {
        ws.ws_col as usize
    } else {
        DEFAULT_WIDTH
    };
    cols.clamp(MIN_WIDTH, MAX_WIDTH)
}

/// Markdown as it should appear on stdout.
pub fn for_stdout(src: &str) -> String {
    match for_fd(libc::STDOUT_FILENO) {
        Some(st) => render(src, st),
        None => src.to_string(),
    }
}

/// Render markdown with the given style. The result has no trailing newline.
pub fn render(src: &str, st: Style) -> String {
    let mut r = Renderer {
        st,
        out: vec![],
        para: None,
        list: vec![],
    };
    r.blocks(src);
    while r.out.last().is_some_and(|l| l.is_empty()) {
        r.out.pop();
    }
    r.out.join("\n")
}

// ---------------------------------------------------------------- blocks

struct Para {
    first: String,
    rest: String,
    text: String,
}

struct Renderer {
    st: Style,
    out: Vec<String>,
    /// the paragraph being collected (consecutive text lines join with spaces)
    para: Option<Para>,
    /// open list items: (marker column, prefix for continuation lines)
    list: Vec<(usize, String)>,
}

impl Renderer {
    fn paint(&self, code: &str, s: &str) -> String {
        if self.st.color && !s.is_empty() {
            format!("{code}{s}{RESET}")
        } else {
            s.to_string()
        }
    }

    fn flush(&mut self) {
        if let Some(p) = self.para.take() {
            let text = inline(p.text.trim_end(), self.st.color);
            let lines = wrap(&text, &p.first, &p.rest, self.st.width, self.st.color);
            self.out.extend(lines);
        }
    }

    fn blank(&mut self) {
        self.flush();
        if self.out.last().is_some_and(|l| !l.is_empty()) {
            self.out.push(String::new());
        }
    }

    /// Prefix for blocks nested under the current list item (if `indent` puts them there).
    fn nested_prefix(&self, indent: usize) -> String {
        match self.list.last() {
            Some((col, rest)) if indent > *col => rest.clone(),
            _ => String::new(),
        }
    }

    fn blocks(&mut self, src: &str) {
        let lines: Vec<&str> = src.lines().collect();
        let mut i = 0;
        while i < lines.len() {
            let line = lines[i].trim_end_matches('\r');
            let indent = indent_of(line);
            let body = line.trim_start();

            if body.is_empty() {
                self.blank();
                i += 1;
                continue;
            }

            // fenced code: verbatim until the closing fence
            if let Some((ch, n, lang)) = fence_open(body) {
                self.flush();
                let prefix = self.nested_prefix(indent);
                if !lang.is_empty() {
                    let label = self.paint(DIM, lang);
                    self.out.push(format!("{prefix}  {label}"));
                }
                i += 1;
                while i < lines.len() {
                    let l = lines[i].trim_end_matches('\r');
                    if is_fence_close(l.trim_start(), ch, n) {
                        i += 1;
                        break;
                    }
                    let code = strip_indent(l, indent);
                    let painted = self.paint(CODE, code);
                    self.out.push(if code.is_empty() {
                        String::new()
                    } else {
                        format!("{prefix}    {painted}")
                    });
                    i += 1;
                }
                continue;
            }

            // a non-indented, non-item line ends any open list
            let item = list_item(line);
            if indent == 0 && item.is_none() && self.para.is_none() {
                self.list.clear();
            }

            // indented code (outside lists and paragraphs)
            if indent >= 4 && self.para.is_none() && self.list.is_empty() {
                self.flush();
                while i < lines.len() {
                    let l = lines[i].trim_end_matches('\r');
                    if !l.trim().is_empty() && indent_of(l) < 4 {
                        break;
                    }
                    let code = strip_indent(l, 4);
                    let painted = self.paint(CODE, code);
                    self.out.push(if code.is_empty() {
                        String::new()
                    } else {
                        format!("    {painted}")
                    });
                    i += 1;
                }
                continue;
            }

            if let Some(level) = heading(body) {
                self.flush();
                self.list.clear();
                let text = body[level..].trim().trim_end_matches('#').trim_end();
                let rendered = inline(text, self.st.color);
                let rendered = if self.st.color {
                    // reapply the heading style after any inline reset
                    let code = if level <= 2 { HEADING } else { BOLD };
                    format!(
                        "{code}{}{RESET}",
                        rendered.replace(RESET, &format!("{RESET}{code}"))
                    )
                } else {
                    rendered
                };
                self.out
                    .extend(wrap(&rendered, "", "", self.st.width, self.st.color));
                i += 1;
                continue;
            }

            if is_rule(body) {
                self.flush();
                let prefix = self.nested_prefix(indent);
                let n = self.st.width.saturating_sub(prefix.chars().count()).min(40);
                let rule = self.paint(DIM, &"─".repeat(n));
                self.out.push(format!("{prefix}{rule}"));
                i += 1;
                continue;
            }

            if body.starts_with('>') {
                self.flush();
                let prefix = self.nested_prefix(indent);
                let mut inner = String::new();
                while i < lines.len() {
                    let b = lines[i].trim();
                    let Some(rest) = b.strip_prefix('>') else {
                        break;
                    };
                    inner.push_str(rest.strip_prefix(' ').unwrap_or(rest));
                    inner.push('\n');
                    i += 1;
                }
                let bar = format!("{prefix}{}", self.paint(DIM, "│ "));
                let st = Style {
                    width: self.st.width.saturating_sub(display_width(&bar)).max(10),
                    ..self.st
                };
                let body = render(&inner, st);
                for l in body.lines() {
                    if l.is_empty() {
                        self.out.push(bar.trim_end().to_string());
                    } else if self.st.color {
                        self.out.push(format!(
                            "{bar}{ITALIC}{}{RESET}",
                            l.replace(RESET, &format!("{RESET}{ITALIC}"))
                        ));
                    } else {
                        self.out.push(format!("{bar}{l}"));
                    }
                }
                continue;
            }

            if let Some(n) = table_at(&lines, i) {
                self.flush();
                let prefix = self.nested_prefix(indent);
                let tbl = self.table(&lines[i..i + n]);
                self.out
                    .extend(tbl.into_iter().map(|l| format!("{prefix}{l}")));
                i += n;
                continue;
            }

            if let Some((marker, text)) = item {
                self.flush();
                while self.list.last().is_some_and(|(col, _)| *col >= indent) {
                    self.list.pop();
                }
                let depth = self.list.len();
                let base = self.list.last().map(|(_, r)| r.clone()).unwrap_or_default();
                let shown = match marker.as_str() {
                    "-" | "*" | "+" => ["•", "◦", "▪"][depth % 3].to_string(),
                    m => m.to_string(),
                };
                let (text, shown) = match task_box(text) {
                    Some((done, rest)) => {
                        (rest, format!("{shown} {}", if done { "☑" } else { "☐" }))
                    }
                    None => (text, shown),
                };
                let first = format!("{base}{} ", self.paint(DIM, &shown));
                let rest = format!("{base}{}", " ".repeat(shown.chars().count() + 1));
                self.list.push((indent, rest.clone()));
                self.para = Some(Para {
                    first,
                    rest,
                    text: hard_break(text),
                });
                i += 1;
                continue;
            }

            // plain text: continue the open paragraph, or start one
            match &mut self.para {
                Some(p) => {
                    if !p.text.ends_with('\n') {
                        p.text.push(' ');
                    }
                    p.text.push_str(&hard_break(body));
                }
                None => {
                    let prefix = self.nested_prefix(indent);
                    self.para = Some(Para {
                        first: prefix.clone(),
                        rest: prefix,
                        text: hard_break(body),
                    });
                }
            }
            i += 1;
        }
        self.flush();
    }

    fn table(&self, rows: &[&str]) -> Vec<String> {
        let align: Vec<char> = split_row(rows[1])
            .iter()
            .map(|c| {
                let c = c.trim();
                match (c.starts_with(':'), c.ends_with(':')) {
                    (true, true) => 'c',
                    (false, true) => 'r',
                    _ => 'l',
                }
            })
            .collect();
        let cells: Vec<Vec<String>> = std::iter::once(rows[0])
            .chain(rows[2..].iter().copied())
            .map(|r| {
                split_row(r)
                    .iter()
                    .map(|c| inline(c.trim(), self.st.color))
                    .collect()
            })
            .collect();
        let ncol = align
            .len()
            .max(cells.iter().map(Vec::len).max().unwrap_or(0));
        let mut widths = vec![0; ncol];
        for row in &cells {
            for (j, c) in row.iter().enumerate() {
                widths[j] = widths[j].max(display_width(c));
            }
        }
        let sep = self.paint(DIM, " │ ");
        let fmt_row = |row: &Vec<String>, header: bool| -> String {
            let mut parts = vec![];
            for (j, w) in widths.iter().enumerate() {
                let c = row.get(j).map(String::as_str).unwrap_or("");
                let pad = w - display_width(c);
                let c = if header && self.st.color && !c.is_empty() {
                    format!(
                        "{BOLD}{}{RESET}",
                        c.replace(RESET, &format!("{RESET}{BOLD}"))
                    )
                } else {
                    c.to_string()
                };
                let (l, r) = match align.get(j).copied().unwrap_or('l') {
                    'r' => (pad, 0),
                    'c' => (pad / 2, pad - pad / 2),
                    _ => (0, pad),
                };
                parts.push(format!("{}{c}{}", " ".repeat(l), " ".repeat(r)));
            }
            parts.join(&sep).trim_end().to_string()
        };
        let mut out = vec![fmt_row(&cells[0], true)];
        let rule: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
        out.push(self.paint(DIM, &rule.join("─┼─")));
        for row in &cells[1..] {
            out.push(fmt_row(row, false));
        }
        out
    }
}

fn indent_of(line: &str) -> usize {
    line.chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum()
}

/// Drop up to `n` columns of leading whitespace.
fn strip_indent(line: &str, n: usize) -> &str {
    let mut col = 0;
    for (i, c) in line.char_indices() {
        if col >= n || !(c == ' ' || c == '\t') {
            return &line[i..];
        }
        col += if c == '\t' { 4 } else { 1 };
    }
    ""
}

/// "  \n" and "\\\n" are hard line breaks in markdown; keep them as '\n'.
fn hard_break(line: &str) -> String {
    if let Some(s) = line.strip_suffix('\\') {
        format!("{s}\n")
    } else if line.ends_with("  ") {
        format!("{}\n", line.trim_end())
    } else {
        line.trim_end().to_string()
    }
}

fn fence_open(body: &str) -> Option<(char, usize, &str)> {
    let ch = body.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let n = body.chars().take_while(|c| *c == ch).count();
    if n < 3 {
        return None;
    }
    let info = body[n..].trim();
    if ch == '`' && info.contains('`') {
        return None;
    }
    Some((ch, n, info.split_whitespace().next().unwrap_or("")))
}

fn is_fence_close(body: &str, ch: char, n: usize) -> bool {
    let run = body.chars().take_while(|c| *c == ch).count();
    run >= n && body[run * ch.len_utf8()..].trim().is_empty()
}

fn heading(body: &str) -> Option<usize> {
    let n = body.chars().take_while(|c| *c == '#').count();
    ((1..=6).contains(&n) && (body.len() == n || body[n..].starts_with([' ', '\t']))).then_some(n)
}

fn is_rule(body: &str) -> bool {
    let Some(ch) = body.chars().next().filter(|c| matches!(c, '-' | '*' | '_')) else {
        return false;
    };
    body.chars().all(|c| c == ch || c == ' ') && body.chars().filter(|c| *c == ch).count() >= 3
}

/// A list item: its marker ("-", "*", "+", "3." ...) and the text after it.
fn list_item(line: &str) -> Option<(String, &str)> {
    let body = line.trim_start();
    let mut chars = body.char_indices();
    let (_, c) = chars.next()?;
    let marker_end = if matches!(c, '-' | '*' | '+') {
        1
    } else if c.is_ascii_digit() {
        let digits = body.chars().take_while(char::is_ascii_digit).count();
        if digits > 9 || !body[digits..].starts_with(['.', ')']) {
            return None;
        }
        digits + 1
    } else {
        return None;
    };
    let rest = &body[marker_end..];
    if rest.is_empty() {
        return Some((body[..marker_end].to_string(), ""));
    }
    if !rest.starts_with([' ', '\t']) {
        return None;
    }
    Some((body[..marker_end].to_string(), rest.trim_start()))
}

fn task_box(text: &str) -> Option<(bool, &str)> {
    let rest = text
        .strip_prefix("[ ] ")
        .map(|r| (false, r))
        .or_else(|| text.strip_prefix("[x] ").map(|r| (true, r)))
        .or_else(|| text.strip_prefix("[X] ").map(|r| (true, r)))?;
    Some(rest)
}

/// If a GFM table starts at line `i`, how many lines it spans.
fn table_at(lines: &[&str], i: usize) -> Option<usize> {
    let head = lines[i].trim();
    let delim = lines.get(i + 1)?.trim();
    if !head.contains('|') || !is_delim_row(delim) {
        return None;
    }
    if split_row(head).len() != split_row(delim).len() {
        return None;
    }
    let mut n = 2;
    while lines
        .get(i + n)
        .is_some_and(|l| l.contains('|') && !l.trim().is_empty())
    {
        n += 1;
    }
    Some(n)
}

fn is_delim_row(s: &str) -> bool {
    s.contains('-')
        && s.contains(['|', ':'])
        && split_row(s).iter().all(|c| {
            let c = c.trim();
            let c = c.strip_prefix(':').unwrap_or(c);
            let c = c.strip_suffix(':').unwrap_or(c);
            !c.is_empty() && c.chars().all(|ch| ch == '-')
        })
}

/// Split a table row on unescaped pipes outside code spans, dropping the outer pipes.
fn split_row(row: &str) -> Vec<String> {
    let row = row.trim();
    let row = row.strip_prefix('|').unwrap_or(row);
    let row = if row.ends_with('|') && !row.ends_with("\\|") {
        &row[..row.len() - 1]
    } else {
        row
    };
    let mut cells = vec![String::new()];
    let mut in_code = false;
    let mut chars = row.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cells.last_mut().unwrap().push('|');
                chars.next();
            }
            '`' => {
                in_code = !in_code;
                cells.last_mut().unwrap().push(c);
            }
            '|' if !in_code => cells.push(String::new()),
            _ => cells.last_mut().unwrap().push(c),
        }
    }
    cells
}

// ---------------------------------------------------------------- inline

/// Render inline markdown (emphasis, code, links) to ANSI, or to plain text when
/// `color` is off. Anything that doesn't form a proper span is left as written, so
/// snake_case, `a * b`, globs and paths come through untouched.
pub fn inline(s: &str, color: bool) -> String {
    let mut out = String::new();
    Inline {
        chars: s.chars().collect(),
        color,
        stack: vec![],
    }
    .run(0, usize::MAX, &mut out);
    out
}

struct Inline {
    chars: Vec<char>,
    color: bool,
    /// active styles, so closing one can restore the outer ones
    stack: Vec<&'static str>,
}

impl Inline {
    fn push(&mut self, code: &'static str, out: &mut String) {
        if self.color {
            self.stack.push(code);
            out.push_str(code);
        }
    }

    fn pop(&mut self, out: &mut String) {
        if self.color {
            self.stack.pop();
            out.push_str(RESET);
            for c in &self.stack {
                out.push_str(c);
            }
        }
    }

    fn at(&self, i: usize) -> Option<char> {
        self.chars.get(i).copied()
    }

    fn starts(&self, i: usize, pat: &str) -> bool {
        let mut j = i;
        for p in pat.chars() {
            if self.at(j) != Some(p) {
                return false;
            }
            j += 1;
        }
        true
    }

    fn run(&mut self, start: usize, end: usize, out: &mut String) {
        let end = end.min(self.chars.len());
        let mut i = start;
        while i < end {
            let c = self.chars[i];
            let prev = if i > start { self.at(i - 1) } else { None };
            let word_start = prev.is_none_or(|p| p.is_whitespace() || "([{<\"'".contains(p));

            // bare URLs pass through verbatim (they often hold _ and *)
            if word_start && (self.starts(i, "http://") || self.starts(i, "https://")) {
                while i < end && !self.chars[i].is_whitespace() {
                    out.push(self.chars[i]);
                    i += 1;
                }
                continue;
            }

            match c {
                '\\' if self.at(i + 1).is_some_and(|n| n.is_ascii_punctuation()) && i + 1 < end => {
                    out.push(self.chars[i + 1]);
                    i += 2;
                }
                '`' => {
                    let n = self.run_len(i, '`');
                    match self.find_backticks(i + n, end, n) {
                        Some(close) => {
                            let code: String = self.chars[i + n..close].iter().collect();
                            let code =
                                if code.len() > 2 && code.starts_with(' ') && code.ends_with(' ') {
                                    &code[1..code.len() - 1]
                                } else {
                                    &code
                                };
                            self.push(CODE, out);
                            out.push_str(code);
                            self.pop(out);
                            i = close + n;
                        }
                        None => {
                            for _ in 0..n {
                                out.push('`');
                            }
                            i += n;
                        }
                    }
                }
                '<' => match self.autolink(i, end) {
                    Some((url, next)) => {
                        self.push(LINK, out);
                        out.push_str(&url);
                        self.pop(out);
                        i = next;
                    }
                    None => {
                        out.push(c);
                        i += 1;
                    }
                },
                '!' if self.at(i + 1) == Some('[') => match self.link(i + 1, end) {
                    Some((ts, te, url, next)) => {
                        self.emit_link(ts, te, &url, out);
                        i = next;
                    }
                    None => {
                        out.push(c);
                        i += 1;
                    }
                },
                '[' => match self.link(i, end) {
                    Some((ts, te, url, next)) => {
                        self.emit_link(ts, te, &url, out);
                        i = next;
                    }
                    None => {
                        out.push(c);
                        i += 1;
                    }
                },
                '*' | '_' | '~' => {
                    let run = self.run_len(i, c);
                    let mut done = false;
                    // try the longest delimiter this run can open: ** before *
                    let options: &[usize] = match (c, run) {
                        ('~', r) if r >= 2 => &[2],
                        ('~', _) => &[],
                        (_, 1) => &[1],
                        _ => &[2, 1],
                    };
                    for &n in options {
                        if let Some(close) = self.emphasis(i, n, end, prev) {
                            let code = match (c, n) {
                                ('~', _) => STRIKE,
                                (_, 2) => BOLD,
                                _ => ITALIC,
                            };
                            self.push(code, out);
                            self.run(i + n, close, out);
                            self.pop(out);
                            i = close + n;
                            done = true;
                            break;
                        }
                    }
                    if !done {
                        for _ in 0..run {
                            out.push(c);
                        }
                        i += run;
                    }
                }
                _ => {
                    out.push(c);
                    i += 1;
                }
            }
        }
    }

    fn run_len(&self, i: usize, c: char) -> usize {
        self.chars[i..].iter().take_while(|x| **x == c).count()
    }

    fn find_backticks(&self, from: usize, end: usize, n: usize) -> Option<usize> {
        let mut j = from;
        while j < end {
            if self.chars[j] == '`' {
                let r = self.run_len(j, '`');
                if r == n && j + r <= end {
                    return Some(j);
                }
                j += r;
            } else {
                j += 1;
            }
        }
        None
    }

    /// Where the delimiter of length `n` opened at `i` closes, if it is a real span.
    fn emphasis(&self, i: usize, n: usize, end: usize, prev: Option<char>) -> Option<usize> {
        let d = self.chars[i];
        let run = self.run_len(i, d);
        let next = self.at(i + n)?;
        let after_run = self.at(i + run)?;
        // opener: followed by text, not intraword (protects snake_case and 2*3*4),
        // not the start of a glob or path (`*.rs`, `/__init__`)
        // (a `***` run opens bold+italic, so there `next` may be the delimiter)
        if after_run.is_whitespace()
            || (next == d && run != 3)
            || (n == 1 && ".,;:/\\)".contains(next))
        {
            return None;
        }
        if prev.is_some_and(|p| p.is_alphanumeric() || p == '/' || p == '\\' || p == d) {
            return None;
        }
        let mut j = i + n + 1;
        while j < end {
            let c = self.chars[j];
            if c == '`' {
                let r = self.run_len(j, '`');
                j = self.find_backticks(j + r, end, r).map_or(j + r, |k| k + r);
                continue;
            }
            if c == d {
                let r = self.run_len(j, d).min(end - j);
                // a run longer than needed closes with its last n chars (***x*** → bold(*x*))
                let k = j + r.saturating_sub(n);
                let before = self.chars[k - 1];
                let after = self.at(k + n).filter(|_| k + n < end);
                let after2 = self.at(k + n + 1);
                let closes = r >= n
                    && !before.is_whitespace()
                    && before != '/'
                    && !after.is_some_and(|a| a.is_alphanumeric())
                    && !(after == Some('.') && after2.is_some_and(|a| a.is_alphanumeric()));
                if closes && (r == n || before != d || k > i + n) {
                    // `__init__`, `__dunder_name__`: a Python name, not bold
                    let ident = self.chars[i + n..k]
                        .iter()
                        .all(|c| c.is_alphanumeric() || *c == '_');
                    if d == '_' && n == 2 && ident {
                        return None;
                    }
                    return Some(k);
                }
                j += r;
                continue;
            }
            j += 1;
        }
        None
    }

    /// `[text](url)` starting at `i` (the '['): (text start, text end, url, next index).
    fn link(&self, i: usize, end: usize) -> Option<(usize, usize, String, usize)> {
        let mut depth = 0;
        let mut j = i;
        let close = loop {
            if j >= end {
                return None;
            }
            match self.chars[j] {
                '\\' => j += 1,
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        break j;
                    }
                }
                _ => {}
            }
            j += 1;
        };
        if self.at(close + 1) != Some('(') {
            return None;
        }
        let mut depth = 0;
        let mut k = close + 1;
        let url_end = loop {
            if k >= end {
                return None;
            }
            match self.chars[k] {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        break k;
                    }
                }
                c if c.is_whitespace() && depth == 1 => {
                    // `(url "title")`: keep only the url
                }
                _ => {}
            }
            k += 1;
        };
        let raw: String = self.chars[close + 2..url_end].iter().collect();
        let url = raw
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_matches(['<', '>']);
        Some((i + 1, close, url.to_string(), url_end + 1))
    }

    fn emit_link(&mut self, ts: usize, te: usize, url: &str, out: &mut String) {
        let text: String = self.chars[ts..te].iter().collect();
        let bare = url.strip_prefix("mailto:").unwrap_or(url);
        self.push(LINK, out);
        if text.is_empty() {
            out.push_str(url);
        } else {
            self.run(ts, te, out);
        }
        self.pop(out);
        if !text.is_empty() && !url.is_empty() && text != url && text != bare {
            out.push_str(" (");
            self.push(DIM, out);
            out.push_str(url);
            self.pop(out);
            out.push(')');
        }
    }

    fn autolink(&self, i: usize, end: usize) -> Option<(String, usize)> {
        let close = (i + 1..end).find(|&j| self.chars[j] == '>')?;
        let inner: String = self.chars[i + 1..close].iter().collect();
        let ok = !inner.contains(char::is_whitespace)
            && (inner.starts_with("http://")
                || inner.starts_with("https://")
                || inner.starts_with("mailto:"));
        ok.then_some((inner, close + 1))
    }
}

// ---------------------------------------------------------------- wrapping

/// Columns `s` takes on screen: chars, not bytes, and ANSI escapes count as zero.
pub fn display_width(s: &str) -> usize {
    let mut n = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            skip_escape(&mut chars);
        } else {
            n += 1;
        }
    }
    n
}

fn skip_escape(chars: &mut std::str::Chars) {
    match chars.next() {
        Some('[') => {
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
        Some(']') => {
            // OSC: until BEL or ST
            while let Some(c) = chars.next() {
                if c == '\x07' || (c == '\x1b' && chars.next() == Some('\\')) {
                    break;
                }
            }
        }
        _ => {}
    }
}

/// Word-wrap `text` (which may hold ANSI escapes and '\n' hard breaks) to `width`
/// columns, `first` prefixing the first line and `rest` the others. Styles open at a
/// line break are closed before it and reopened after the next prefix, so prefixes
/// never inherit them. Words longer than a line stay whole.
pub fn wrap(text: &str, first: &str, rest: &str, width: usize, color: bool) -> Vec<String> {
    let mut lines = vec![];
    let mut active: Vec<String> = vec![];
    for (n, hard) in text.split('\n').enumerate() {
        let mut line = if n == 0 {
            first.to_string()
        } else {
            rest.to_string()
        };
        let reopen = |line: &mut String, active: &[String]| {
            for a in active {
                line.push_str(a);
            }
        };
        reopen(&mut line, &active);
        let mut used = display_width(&line);
        let mut empty = true;
        for word in hard.split(' ').filter(|w| !w.is_empty()) {
            let w = display_width(word);
            if !empty && used + 1 + w > width {
                if color && !active.is_empty() {
                    line.push_str(RESET);
                }
                lines.push(line);
                line = rest.to_string();
                reopen(&mut line, &active);
                used = display_width(&line);
                empty = true;
            }
            if !empty {
                line.push(' ');
                used += 1;
            }
            line.push_str(word);
            used += w;
            empty = false;
            track_sgr(word, &mut active);
        }
        if color && !active.is_empty() {
            line.push_str(RESET);
        }
        lines.push(line.trim_end().to_string());
    }
    lines
}

/// Follow SGR escapes in `s`: a reset clears the active set, anything else joins it.
fn track_sgr(s: &str, active: &mut Vec<String>) {
    let mut rest = s;
    while let Some(p) = rest.find("\x1b[") {
        let tail = &rest[p + 2..];
        let Some(e) = tail.find(|c: char| ('@'..='~').contains(&c)) else {
            break;
        };
        if tail.as_bytes()[e] == b'm' {
            let params = &tail[..e];
            if params.is_empty() || params == "0" {
                active.clear();
            } else {
                active.push(format!("\x1b[{params}m"));
            }
        }
        rest = &tail[e + 1..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: Style = Style {
        color: false,
        width: 80,
    };
    const ANSI: Style = Style {
        color: true,
        width: 80,
    };

    fn plain(s: &str) -> String {
        render(s, PLAIN)
    }

    fn strip(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                skip_escape(&mut chars);
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn headings_drop_hashes_and_are_styled() {
        assert_eq!(
            plain("# Title\n\n## Sub ##\n### Third"),
            "Title\n\nSub\nThird"
        );
        let a = render("# Title", ANSI);
        assert!(a.contains(HEADING) && a.contains("Title") && !a.contains('#'));
        assert!(render("### Small", ANSI).starts_with(BOLD));
        assert_eq!(plain("#hashtag"), "#hashtag");
    }

    #[test]
    fn emphasis() {
        assert_eq!(
            plain("a **bold** and *it* and _it2_ and ~~gone~~"),
            "a bold and it and it2 and gone"
        );
        let a = render("**b** *i*", ANSI);
        assert_eq!(a, format!("{BOLD}b{RESET} {ITALIC}i{RESET}"));
        assert_eq!(plain("***both***"), "both");
        assert_eq!(
            strip(&render("**bold with `code` inside**", ANSI)),
            "bold with code inside"
        );
        assert_eq!(plain("the __init__ method"), "the __init__ method");
        assert_eq!(plain("__really important__"), "really important");
    }

    #[test]
    fn nested_styles_restore_outer() {
        let a = render("**a `c` b**", ANSI);
        assert_eq!(a, format!("{BOLD}a {CODE}c{RESET}{BOLD} b{RESET}"));
    }

    #[test]
    fn not_markdown_is_left_alone() {
        for s in [
            "snake_case_names and more_snake_case",
            "compute a * b * c",
            "2*3*4 = 24",
            "src/*.rs and lib/*.rs",
            "see /tmp/__init__.py and foo/__main__.py",
            "visit https://example.com/some_path_with_under_scores_/x*y*z",
            "a_b *",
            "price is 5*",
            "C:\\path\\to_file_name",
            "x = [1, 2] and arr[0]",
            "under_score_ and _leading",
            "use ** for powers: 2 ** 8",
        ] {
            assert_eq!(plain(s), s, "mangled: {s}");
        }
    }

    #[test]
    fn inline_code() {
        assert_eq!(plain("run `cargo test` now"), "run cargo test now");
        assert_eq!(render("`x`", ANSI), format!("{CODE}x{RESET}"));
        assert_eq!(plain("``a ` b``"), "a ` b");
        assert_eq!(plain("`**not bold**`"), "**not bold**");
        assert_eq!(plain("a ` lone tick"), "a ` lone tick");
    }

    #[test]
    fn links() {
        assert_eq!(plain("[docs](https://x.io/a_b)"), "docs (https://x.io/a_b)");
        assert_eq!(plain("[https://x.io](https://x.io)"), "https://x.io");
        assert_eq!(plain("<https://x.io/a_b_c>"), "https://x.io/a_b_c");
        assert_eq!(plain("![logo](img.png)"), "logo (img.png)");
        assert_eq!(plain("[a](u \"title\")"), "a (u)");
        assert_eq!(plain("[not a link] here"), "[not a link] here");
        assert_eq!(plain("[**b**](u)"), "b (u)");
    }

    #[test]
    fn fenced_code_is_verbatim() {
        let src = "before\n\n```rust\nfn main() { let a_b = 2 * 3; }\n// **not bold** # no heading\n- not a list\n\n    indented\n```\nafter";
        let out = plain(src);
        assert_eq!(
            out,
            "before\n\n  rust\n    fn main() { let a_b = 2 * 3; }\n    // **not bold** # no heading\n    - not a list\n\n        indented\nafter"
        );
        // never wrapped, however long
        let long = format!("```\n{}\n```", "x ".repeat(200));
        assert!(plain(&long).lines().any(|l| l.len() > 300));
        // colored: code lines get the code color, no markdown processing
        let a = render("```\n**x**\n```", ANSI);
        assert_eq!(a, format!("    {CODE}**x**{RESET}"));
        // unterminated fence runs to the end
        assert_eq!(plain("```\na\nb"), "    a\n    b");
        assert_eq!(plain("~~~\nz\n~~~"), "    z");
    }

    #[test]
    fn bullet_and_numbered_lists_nest() {
        let src =
            "- one\n- two\n  - nested\n    - deeper\n- three\n\n1. first\n2. second\n   1. inner";
        assert_eq!(
            plain(src),
            "• one\n• two\n  ◦ nested\n    ▪ deeper\n• three\n\n1. first\n2. second\n   1. inner"
        );
        assert_eq!(plain("* star\n+ plus"), "• star\n• plus");
        assert_eq!(plain("- [ ] todo\n- [x] done"), "• ☐ todo\n• ☑ done");
    }

    #[test]
    fn list_items_wrap_with_hanging_indent() {
        let st = Style {
            color: false,
            width: 20,
        };
        let out = render("- alpha beta gamma delta epsilon zeta", st);
        assert_eq!(out, "• alpha beta gamma\n  delta epsilon zeta");
        let out = render("10. alpha beta gamma delta epsilon", st);
        assert_eq!(out, "10. alpha beta gamma\n    delta epsilon");
        // lazy continuation joins the item
        assert_eq!(plain("- a\ncontinued"), "• a continued");
        // indented paragraph after a blank line stays under the item
        assert_eq!(plain("- a\n\n  more\n\nout"), "• a\n\n  more\n\nout");
    }

    #[test]
    fn blockquotes() {
        assert_eq!(plain("> quoted **text**\n> more"), "│ quoted text more");
        let st = Style {
            color: false,
            width: 20,
        };
        assert_eq!(
            render("> alpha beta gamma delta epsilon", st),
            "│ alpha beta gamma\n│ delta epsilon"
        );
        assert_eq!(plain("> a\n>\n> - item"), "│ a\n│\n│ • item");
    }

    #[test]
    fn rules() {
        assert_eq!(
            plain("a\n\n---\n\nb"),
            format!("a\n\n{}\n\nb", "─".repeat(40))
        );
        assert_eq!(plain("***"), "─".repeat(40));
        assert_eq!(plain("- - -"), "─".repeat(40));
    }

    #[test]
    fn tables_align() {
        let src = "| Name | Qty | Note |\n|:-----|----:|:----:|\n| apple | 3 | `red` |\n| kiwi | 12 | ok |";
        assert_eq!(
            plain(src),
            "Name  │ Qty │ Note\n──────┼─────┼─────\napple │   3 │ red\nkiwi  │  12 │  ok"
        );
        // pipes inside code and escaped pipes don't split cells
        assert_eq!(
            plain("a | b\n--|--\n`x|y` | c\\|d"),
            "a   │ b\n────┼────\nx|y │ c|d"
        );
        // not a table without a delimiter row
        assert_eq!(plain("a | b\nc | d"), "a | b c | d");
    }

    #[test]
    fn paragraphs_wrap_at_word_boundaries() {
        let st = Style {
            color: false,
            width: 20,
        };
        let out = render(
            "all offer when it's ready to go and then some more words",
            st,
        );
        for l in out.lines() {
            assert!(display_width(l) <= 20, "too wide: {l:?}");
        }
        // every word survives intact (no glued or split words)
        assert_eq!(
            out.split_whitespace().collect::<Vec<_>>().join(" "),
            "all offer when it's ready to go and then some more words"
        );
        // a long word stays whole on its own line
        let out = render("a https://example.com/very/long/path/that/exceeds b", st);
        assert_eq!(out, "a\nhttps://example.com/very/long/path/that/exceeds\nb");
        // soft line breaks join; hard breaks keep
        assert_eq!(plain("one\ntwo"), "one two");
        assert_eq!(plain("one  \ntwo"), "one\ntwo");
        assert_eq!(plain("one\\\ntwo"), "one\ntwo");
    }

    #[test]
    fn wrap_ignores_ansi_and_counts_chars() {
        let st = Style {
            color: true,
            width: 20,
        };
        let out = render("**héllo wörld** `naïve` café crème brûlée façade", st);
        for l in out.lines() {
            assert!(display_width(l) <= 20, "too wide: {l:?}");
        }
        assert_eq!(strip(&out), "héllo wörld naïve\ncafé crème brûlée\nfaçade");
        assert_eq!(display_width("\x1b[1mabc\x1b[0m"), 3);
        assert_eq!(display_width("\x1b]8;;http://x\x1b\\ab\x1b]8;;\x1b\\"), 2);
        assert_eq!(display_width("日本語"), 3);
    }

    #[test]
    fn styles_close_at_line_breaks_and_reopen() {
        let st = Style {
            color: true,
            width: 20,
        };
        let out = render("> **aaaa bbbb cccc dddd eeee ffff gggg hhhh**", st);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        for l in &lines {
            assert!(l.ends_with(RESET), "unterminated style: {l:?}");
            assert!(strip(l).starts_with("│ "));
        }
        assert!(lines[1].contains(BOLD), "bold not reopened: {:?}", lines[1]);
    }

    #[test]
    fn plain_mode_has_no_escapes() {
        let src =
            "# H\n**b** `c` [l](u)\n\n- x\n\n> q\n\n```\nz\n```\n\n---\n\n|a|b|\n|-|-|\n|1|2|";
        assert!(!plain(src).contains('\x1b'));
        assert!(render(src, ANSI).contains('\x1b'));
    }

    #[test]
    fn blank_lines_collapse_and_trim() {
        assert_eq!(plain("\n\na\n\n\n\nb\n\n"), "a\n\nb");
        assert_eq!(plain(""), "");
    }

    #[test]
    fn indented_code_block() {
        assert_eq!(
            plain("text\n\n    let x = a_b * c;\n\nmore"),
            "text\n\n    let x = a_b * c;\n\nmore"
        );
    }
}
