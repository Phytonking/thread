//! The zone: the directory `thread` was opened in. Every thread is confined to the zone it
//! was created under (recorded once, stored with the thread, so a resumed thread keeps it),
//! plus its own git worktree if it has one.
//!
//! - file tools: every path is resolved (`..`, symlinks, missing tails) and checked here
//! - shell and CLI providers: on macOS, run under `sandbox-exec` with a profile that denies
//!   file writes outside the zone (plus temp dirs and what git and the CLI itself need).
//!   Reads are not restricted at the OS level.

use crate::threads::Thread;
use anyhow::{Result, bail};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

#[derive(Clone, Debug, PartialEq)]
pub struct Zone {
    /// Canonical roots; the first is the zone root, the rest (a worktree) are also inside.
    roots: Vec<PathBuf>,
}

impl Zone {
    pub fn new(roots: impl IntoIterator<Item = PathBuf>) -> Self {
        let mut v: Vec<PathBuf> = vec![];
        for r in roots {
            let r = resolve(&absolute(&r)).unwrap_or(r);
            if !v.contains(&r) {
                v.push(r);
            }
        }
        Zone { roots: v }
    }

    /// A thread's zone: the zone it was created under, and its worktree.
    pub fn for_thread(t: &Thread) -> Self {
        let root = if t.zone.is_empty() {
            &t.workdir
        } else {
            &t.zone
        };
        let mut roots = vec![PathBuf::from(root)];
        if t.branch.is_some() {
            roots.push(PathBuf::from(&t.workdir));
        }
        Self::new(roots)
    }

    pub fn root(&self) -> &Path {
        &self.roots[0]
    }

    /// True if `resolved` (an already resolved path) is in the zone.
    pub fn contains(&self, resolved: &Path) -> bool {
        self.roots.iter().any(|r| resolved.starts_with(r))
    }

    /// Resolve `path` (relative to `base`) and return it if it is in the zone.
    pub fn check(&self, base: &Path, path: &Path) -> Result<PathBuf> {
        let full = if path.is_absolute() {
            path.to_path_buf()
        } else {
            base.join(path)
        };
        let r = resolve(&full)?;
        if !self.contains(&r) {
            bail!(
                "{} is outside this thread's zone{}; file access is limited to {}",
                path.display(),
                if r != full {
                    format!(" (it resolves to {})", r.display())
                } else {
                    String::new()
                },
                self.describe()
            );
        }
        Ok(r)
    }

    /// Where commands run: `wanted` if it is in the zone, else the zone root.
    pub fn cwd(&self, wanted: &Path) -> PathBuf {
        match resolve(&absolute(wanted)) {
            Ok(w) if self.contains(&w) && w.is_dir() => w,
            _ => self.root().to_path_buf(),
        }
    }

    pub fn describe(&self) -> String {
        self.roots
            .iter()
            .map(|r| r.display().to_string())
            .collect::<Vec<_>>()
            .join(" and ")
    }

    /// Everything a sandboxed process may write: the zone, the git directories its repos
    /// share (a worktree commits into the main repo's .git), temp dirs, and `extra`.
    pub fn writable(&self, extra: &[PathBuf]) -> Vec<PathBuf> {
        let mut w: Vec<PathBuf> = self.roots.clone();
        for r in &self.roots {
            w.extend(git_common_dir(r));
        }
        w.extend(temp_dirs());
        w.extend(extra.iter().cloned());
        if let Ok(v) = std::env::var("THREAD_ZONE_WRITABLE") {
            w.extend(std::env::split_paths(&v).filter(|p| p.is_absolute()));
        }
        let mut out: Vec<PathBuf> = vec![];
        for p in w {
            let p = resolve(&p).unwrap_or(p);
            if !out.iter().any(|o| p.starts_with(o)) {
                out.retain(|o| !o.starts_with(&p));
                out.push(p);
            }
        }
        out
    }
}

fn absolute(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    }
}

/// The real location of an absolute path: `..` and symlinks resolved, like `realpath`, except
/// that the path need not exist. Past the nearest existing ancestor the rest is appended as
/// is; a dangling symlink is followed to where it would create its target; `..` after a
/// missing directory is refused (the OS would refuse it too).
pub fn resolve(path: &Path) -> Result<PathBuf> {
    resolve_depth(path, 0)
}

fn resolve_depth(path: &Path, depth: u32) -> Result<PathBuf> {
    if depth > 40 {
        bail!("{}: too many levels of symbolic links", path.display());
    }
    if !path.is_absolute() {
        bail!("{} is not an absolute path", path.display());
    }
    let mut out = PathBuf::from("/");
    let mut missing = false;
    for c in path.components() {
        match c {
            Component::RootDir | Component::Prefix(_) => out = PathBuf::from("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                if missing {
                    bail!(
                        "{}: '..' after a directory that does not exist",
                        path.display()
                    );
                }
                out.pop();
            }
            Component::Normal(name) => {
                let next = out.join(name);
                if missing {
                    out = next;
                    continue;
                }
                match std::fs::symlink_metadata(&next) {
                    Ok(m) if m.file_type().is_symlink() => {
                        let target = std::fs::read_link(&next)?;
                        // `out` is already resolved, so a relative target is relative to it
                        out = resolve_depth(&out.join(target), depth + 1)?;
                        missing = std::fs::symlink_metadata(&out).is_err();
                    }
                    Ok(_) => out = next,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        missing = true;
                        out = next;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }
    Ok(out)
}

/// The repo's shared git dir when it lies outside `dir` (e.g. a worktree's main `.git`).
pub fn git_common_dir(dir: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    (p.is_absolute() && !p.starts_with(dir)).then_some(p)
}

/// Temp dirs: /tmp, /var/tmp and the per-user dir holding $TMPDIR (with its cache sibling).
fn temp_dirs() -> Vec<PathBuf> {
    let mut v = vec![
        PathBuf::from("/private/tmp"),
        PathBuf::from("/private/var/tmp"),
    ];
    let t = std::env::temp_dir();
    let t = resolve(&t).unwrap_or(t);
    match t.parent() {
        Some(p) if p.starts_with("/private/var/folders") => v.push(p.to_path_buf()),
        _ => v.push(t),
    }
    v
}

/// Whether commands are OS-sandboxed: macOS with sandbox-exec, unless THREAD_NO_SANDBOX=1.
pub fn sandbox_available() -> bool {
    cfg!(target_os = "macos")
        && !std::env::var("THREAD_NO_SANDBOX").is_ok_and(|v| !v.is_empty() && v != "0")
        && Path::new(SANDBOX_EXEC).exists()
}

/// What a sandboxed process may write.
#[derive(Default, Debug)]
pub struct Rules {
    /// Directory trees (and single files).
    pub write: Vec<PathBuf>,
    /// Paths starting with these (e.g. `~/.claude.json` and its backups and temp files).
    pub write_prefix: Vec<PathBuf>,
    /// Re-denied inside the writable ones (e.g. a CLI's settings file, which could add hooks).
    pub deny: Vec<PathBuf>,
}

/// A seatbelt profile allowing everything but file writes outside the rules (and /dev).
/// Paths go in as parameters (W0, D0, …), so they need no escaping; prefixes become regexes.
pub fn profile(r: &Rules) -> (String, Vec<(String, PathBuf)>) {
    let mut params = vec![];
    let mut allow = String::new();
    for (i, w) in r.write.iter().enumerate() {
        allow.push_str(&format!(" (subpath (param \"W{i}\"))"));
        params.push((format!("W{i}"), w.clone()));
    }
    for w in &r.write_prefix {
        let s = w.display().to_string();
        if !s.contains('"') {
            allow.push_str(&format!(" (regex #\"^{}\")", regex_escape(&s)));
        }
    }
    let mut p = format!(
        "(version 1)\n(allow default)\n(deny file-write*)\n(allow file-write* (subpath \"/dev\"){allow})\n"
    );
    if !r.deny.is_empty() {
        let mut d = String::new();
        for (i, x) in r.deny.iter().enumerate() {
            d.push_str(&format!(" (subpath (param \"D{i}\"))"));
            params.push((format!("D{i}"), x.clone()));
        }
        p.push_str(&format!("(deny file-write*{d})\n"));
    }
    (p, params)
}

fn regex_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if r"\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `cmd` run under a sandbox that only lets it write what `rules` allow, or unchanged when
/// sandboxing is off. Keeps its arguments, environment and working directory.
pub fn wrap(cmd: Command, rules: &Rules) -> Command {
    if !sandbox_available() {
        return cmd;
    }
    let (prof, params) = profile(rules);
    let mut c = Command::new(SANDBOX_EXEC);
    c.arg("-p").arg(prof);
    for (k, v) in params {
        let mut d = OsString::from(format!("{k}="));
        d.push(v.as_os_str());
        c.arg("-D").arg(d);
    }
    c.arg(cmd.get_program()).args(cmd.get_args());
    for (k, v) in cmd.get_envs() {
        match v {
            Some(v) => c.env(k, v),
            None => c.env_remove(k),
        };
    }
    if let Some(d) = cmd.get_current_dir() {
        c.current_dir(d);
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("thread-zone-{}", crate::db::new_id()));
        std::fs::create_dir_all(&d).unwrap();
        resolve(&d).unwrap()
    }

    #[test]
    fn inside_is_allowed_escapes_are_not() {
        let base = tmp();
        let zone_dir = base.join("zone");
        std::fs::create_dir_all(zone_dir.join("src")).unwrap();
        std::fs::write(base.join("secret.txt"), "x").unwrap();
        let z = Zone::new([zone_dir.clone()]);
        // inside: existing, new, nested-new, `..` that stays inside
        assert!(z.check(&zone_dir, Path::new("src")).is_ok());
        assert_eq!(
            z.check(&zone_dir, Path::new("src/new/deep.rs")).unwrap(),
            zone_dir.join("src/new/deep.rs")
        );
        assert!(z.check(&zone_dir, Path::new("src/../a.txt")).is_ok());
        // `..` escapes, absolute paths outside, `..` after a missing dir
        let e = z.check(&zone_dir, Path::new("../secret.txt")).unwrap_err();
        assert!(e.to_string().contains("outside this thread's zone"), "{e}");
        assert!(
            z.check(&zone_dir, Path::new("src/../../secret.txt"))
                .is_err()
        );
        assert!(z.check(&zone_dir, Path::new("/etc/passwd")).is_err());
        assert!(z.check(&zone_dir, &base.join("secret.txt")).is_err());
        assert!(z.check(&zone_dir, Path::new("nope/../../x")).is_err());
        // a sibling whose name starts with the zone's is not inside it
        std::fs::create_dir_all(base.join("zone2")).unwrap();
        assert!(z.check(&zone_dir, &base.join("zone2/a")).is_err());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn symlinks_out_are_rejected_even_dangling_ones() {
        let base = tmp();
        let zone_dir = base.join("zone");
        std::fs::create_dir_all(&zone_dir).unwrap();
        std::fs::create_dir_all(base.join("out")).unwrap();
        symlink(base.join("out"), zone_dir.join("link-dir")).unwrap();
        symlink(base.join("out/new.txt"), zone_dir.join("dangling")).unwrap();
        symlink("../out", zone_dir.join("rel-link")).unwrap();
        symlink(".", zone_dir.join("self")).unwrap();
        let z = Zone::new([zone_dir.clone()]);
        assert!(z.check(&zone_dir, Path::new("link-dir/x.txt")).is_err());
        assert!(z.check(&zone_dir, Path::new("link-dir")).is_err());
        let e = z.check(&zone_dir, Path::new("dangling")).unwrap_err();
        assert!(e.to_string().contains("resolves to"), "{e}");
        assert!(z.check(&zone_dir, Path::new("rel-link/a")).is_err());
        assert!(z.check(&zone_dir, Path::new("self/self/a.txt")).is_ok());
        // a zone opened through a symlink is the real directory
        symlink(&zone_dir, base.join("zone-alias")).unwrap();
        let z2 = Zone::new([base.join("zone-alias")]);
        assert_eq!(z2.root(), zone_dir.as_path());
        assert!(z2.check(&base, Path::new("zone-alias/a")).is_ok());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_threads_worktree_is_inside_its_zone_wherever_it_lives() {
        let base = tmp();
        let (zone_dir, wt) = (base.join("repo/sub"), base.join("elsewhere/wt"));
        std::fs::create_dir_all(&zone_dir).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        let t = Thread {
            id: "t".into(),
            title: String::new(),
            kind: "background".into(),
            status: "idle".into(),
            task: String::new(),
            workdir: wt.display().to_string(),
            branch: Some("thread/t".into()),
            parent_id: None,
            state: String::new(),
            trusted: false,
            updated_at: 0,
            provider: String::new(),
            session: None,
            model: None,
            zone: zone_dir.display().to_string(),
        };
        let z = Zone::for_thread(&t);
        assert!(z.check(&wt, Path::new("src/main.rs")).is_ok());
        assert!(z.check(&wt, &zone_dir.join("a")).is_ok());
        assert!(z.check(&wt, Path::new("../other")).is_err());
        assert!(z.check(&wt, &base.join("repo/b")).is_err());
        assert_eq!(z.cwd(&wt), wt);
        // without a branch the workdir grants nothing, and commands start in the zone
        let z = Zone::for_thread(&Thread { branch: None, ..t });
        assert!(z.check(&wt, Path::new("a")).is_err());
        assert_eq!(z.cwd(&wt), zone_dir);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn profile_allows_the_zone_and_denies_the_rest() {
        let (p, params) = profile(&Rules {
            write: vec![PathBuf::from("/z"), PathBuf::from("/t")],
            write_prefix: vec![PathBuf::from("/h/.claude.json")],
            deny: vec![PathBuf::from("/z/.claude")],
        });
        assert!(p.contains("(deny file-write*)") && p.contains("(param \"W1\")"));
        assert!(p.contains(r#"(regex #"^/h/\.claude\.json")"#), "{p}");
        assert!(p.contains("(deny file-write* (subpath (param \"D0\")))"));
        assert_eq!(params[0], ("W0".into(), PathBuf::from("/z")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn sandboxed_shell_cannot_write_outside() {
        if !sandbox_available() {
            return;
        }
        let base = tmp();
        let zone_dir = base.join("zone");
        std::fs::create_dir_all(&zone_dir).unwrap();
        // writable roots that exclude `base` itself (the test's temp dir would allow it)
        let mut c = Command::new("bash");
        c.arg("-c")
            .arg("touch in.txt && (touch ../out.txt 2>/dev/null && echo escaped || echo blocked)")
            .current_dir(&zone_dir);
        let rules = Rules {
            write: vec![zone_dir.clone()],
            ..Default::default()
        };
        let out = wrap(c, &rules).output().unwrap();
        let s = String::from_utf8_lossy(&out.stdout);
        if !out.status.success() && s.is_empty() {
            // already inside a sandbox (sandbox_apply cannot nest): nothing to check here
            std::fs::remove_dir_all(&base).ok();
            return;
        }
        assert_eq!(s.trim(), "blocked");
        assert!(zone_dir.join("in.txt").exists() && !base.join("out.txt").exists());
        std::fs::remove_dir_all(&base).ok();
    }
}
