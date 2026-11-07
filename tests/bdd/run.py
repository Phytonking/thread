#!/usr/bin/env python3
"""BDD runner for thread: parses the .feature files (Gherkin) and drives the real
`thread` binary, one isolated git repo + config dir + mock model server per scenario.

    python3 tests/bdd/run.py                    # all @offline scenarios
    python3 tests/bdd/run.py --tags live        # real subscriptions (claude, codex)
    python3 tests/bdd/run.py --tags claude features/20_live_providers.feature
    python3 tests/bdd/run.py -k "inherits"      # scenarios whose name matches

Env: THREAD_BIN (default target/release/thread), BDD_WAIT (seconds per step, default 20).
"""
import argparse, json, os, re, shlex, shutil, signal, socket, sqlite3, subprocess, sys, tempfile, threading, time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
BIN = Path(os.environ.get("THREAD_BIN", ROOT / "target" / "release" / "thread"))
WAIT = float(os.environ.get("BDD_WAIT", "20"))
ANSI = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]")

# ---------------------------------------------------------------- parsing

class Step:
    def __init__(self, kw, text, line): self.kw, self.text, self.line = kw, text, line

class Scenario:
    def __init__(self, feature, name, tags, steps, background):
        self.feature, self.name, self.tags, self.steps, self.background = feature, name, tags, steps, background

STEP_KW = ("Given", "When", "Then", "And", "But")

def parse(path):
    lines = Path(path).read_text().splitlines()
    feature, feature_tags, background, scenarios = "", set(), [], []
    pending_tags, current, outline, examples, mode = set(), None, False, None, None
    def flush():
        nonlocal current, examples
        if current is None: return
        name, tags, steps = current
        if outline:
            header, rows = examples[0], examples[1:]
            for row in rows:
                vals = dict(zip(header, row))
                sub = lambda s: re.sub(r"<([^>]+)>", lambda m: vals.get(m.group(1), m.group(0)), s)
                scenarios.append(Scenario(feature, f"{name} [{', '.join(row)}]", tags, [Step(s.kw, sub(s.text), s.line) for s in steps], background))
        else:
            scenarios.append(Scenario(feature, name, tags, steps, background))
        current, examples = None, None
    for i, raw in enumerate(lines, 1):
        line = raw.strip()
        if not line or line.startswith("#"): continue
        if line.startswith("@"):
            pending_tags |= {t.lstrip("@") for t in line.split()}
        elif line.startswith("Feature:"):
            feature, feature_tags, pending_tags = line[8:].strip(), pending_tags, set()
        elif line.startswith("Background:"):
            mode = "background"
        elif line.startswith(("Scenario Outline:", "Scenario:")):
            flush()
            outline = line.startswith("Scenario Outline:")
            name = line.split(":", 1)[1].strip()
            current, mode = (name, feature_tags | pending_tags, []), "scenario"
            pending_tags = set()
        elif line.startswith("Examples:"):
            examples, mode = [], "examples"
        elif line.startswith("|") and mode == "examples":
            examples.append([c.strip() for c in line.strip("|").split("|")])
        elif line.split(" ", 1)[0] in STEP_KW:
            kw, text = line.split(" ", 1)
            step = Step(kw, text.strip(), i)
            (background if mode == "background" else current[2]).append(step)
    flush()
    return scenarios

# ---------------------------------------------------------------- world

def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0)); return s.getsockname()[1]

class World:
    def __init__(self, live):
        self.live = live
        self.tmp = Path(tempfile.mkdtemp(prefix="thread-bdd-"))
        self.repo = self.tmp / "repo"
        self.cfg = self.tmp / "config"
        self.cfg.mkdir()
        self.log = self.tmp / "mock.jsonl"
        self.mock = None
        self.proc = None
        self.out = ""
        self.lock = threading.Lock()
        self.cursor = 0
        self.last = None
        self.env = dict(os.environ)
        self.env["XDG_CONFIG_HOME"] = str(self.cfg)
        for k in ("OPENAI_API_KEY", "THREAD_PROVIDER", "THREAD_MODEL", "OPENAI_BASE_URL", "NO_COLOR"):
            self.env.pop(k, None)

    # -- mock model server
    def start_mock(self, models=None):
        port = free_port()
        env = dict(self.env, MOCK_LOG=str(self.log))
        if models: env["MOCK_MODELS"] = models
        self.mock = subprocess.Popen([sys.executable, "-I", str(HERE / "mock_llm.py"), str(port)], env=env,
                                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(100):
            try:
                socket.create_connection(("127.0.0.1", port), 0.1).close(); break
            except OSError: time.sleep(0.05)
        self.env["OPENAI_BASE_URL"] = f"http://127.0.0.1:{port}/v1"
        self.env["THREAD_PROVIDER"] = "api"

    def requests(self):
        if not self.log.exists(): return []
        return [json.loads(l) for l in self.log.read_text().splitlines() if l.strip()]

    # -- interactive session
    def open(self, args=""):
        if self.proc and self.proc.poll() is None: self.close()
        self.out, self.cursor = "", 0
        self.proc = subprocess.Popen([str(BIN)] + shlex.split(args), cwd=self.repo, env=self.env,
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, bufsize=0)
        def reader(p=self.proc):
            while True:
                chunk = os.read(p.stdout.fileno(), 4096)
                if not chunk: break
                with self.lock: self.out += ANSI.sub("", chunk.decode("utf-8", "replace"))
        threading.Thread(target=reader, daemon=True).start()

    def type(self, line):
        assert self.proc and self.proc.poll() is None, "thread is not running"
        with self.lock: self.cursor = len(self.out)
        self.proc.stdin.write((line + "\n").encode()); self.proc.stdin.flush()

    def since(self):
        with self.lock: return self.out[self.cursor:]

    def wait_for(self, text, timeout=WAIT):
        end = time.time() + timeout
        while time.time() < end:
            if text in self.since(): return True
            time.sleep(0.1)
        return False

    def wait_exit(self, timeout=60):
        try: return self.proc.wait(timeout)
        except subprocess.TimeoutExpired: return None

    def close(self):
        if not self.proc or self.proc.poll() is not None: return
        try:
            self.type("exit")
            end = time.time() + 30
            while time.time() < end and self.proc.poll() is None:
                if "still running" in self.since():
                    self.proc.stdin.write(b"w\n"); self.proc.stdin.flush(); break
                time.sleep(0.1)
        except Exception: pass
        if self.wait_exit(120) is None: self.proc.kill()

    # -- one-shot commands
    def run(self, cmd, timeout=180):
        argv = shlex.split(cmd)
        if argv[0] == "thread": argv[0] = str(BIN)
        p = subprocess.run(argv, cwd=self.repo, env=self.env, capture_output=True, text=True, timeout=timeout, stdin=subprocess.DEVNULL)
        self.last = (p.returncode, ANSI.sub("", p.stdout + p.stderr))

    def db(self):
        return sqlite3.connect(self.repo / ".thread" / "thread.db")

    def cleanup(self):
        for p in (self.proc, self.mock):
            if p and p.poll() is None:
                p.kill()
                try: p.wait(5)
                except Exception: pass
        shutil.rmtree(self.tmp, ignore_errors=True)

# ---------------------------------------------------------------- steps

STEPS = []
def step(pattern):
    def deco(fn): STEPS.append((re.compile(pattern + r"$"), fn)); return fn
    return deco

def git(w, *args):
    return subprocess.run(["git", "-C", str(w.repo), *args], capture_output=True, text=True)

def init_repo(w, files):
    w.repo.mkdir()
    git(w, "init", "-q", "-b", "main")
    for name, body in files.items(): (w.repo / name).write_text(body)
    git(w, "add", ".")
    git(w, "-c", "user.email=bdd@test", "-c", "user.name=bdd", "commit", "-qm", "init")

@step(r"a git repository")
def _(w): init_repo(w, {"README.md": "# test repo\n"})

@step(r"a git repository with a buggy calc\.py")
def _(w): init_repo(w, {"calc.py": "def add(a, b):\n    return a - b\n"})

@step(r"the model server is running")
def _(w): w.start_mock()

@step(r'the model server offers models "([^"]+)"')
def _(w, models): w.start_mock(",".join(m.strip() for m in models.split(",")))

@step(r'the provider "([^"]+)" is not installed')
def _(w, name):
    w.env["PATH"] = os.pathsep.join(d for d in w.env["PATH"].split(os.pathsep) if not (Path(d) / name).exists())

@step(r"I open thread")
def _(w): w.open()

@step(r'I open thread with "([^"]*)"')
def _(w, args): w.open(args)

@step(r"I close thread")
def _(w): w.close()

@step(r'I type "(.*)"')
def _(w, line): w.type(line)

@step(r"I press ctrl-c")
def _(w):
    with w.lock: w.cursor = len(w.out)
    w.proc.send_signal(signal.SIGINT)

@step(r'I see "(.*)"')
def _(w, text):
    assert w.wait_for(text.replace('\\"', '"')), f'never saw "{text}"'

@step(r'within (\d+) seconds I see "(.*)"')
def _(w, secs, text):
    assert w.wait_for(text, float(secs)), f'never saw "{text}" within {secs}s'

@step(r'I do not see "(.*)"')
def _(w, text):
    time.sleep(1.5)
    assert text not in w.since(), f'unexpectedly saw "{text}"'

@step(r"thread has exited")
def _(w): assert w.wait_exit(90) is not None, "thread did not exit"

@step(r"thread exits successfully")
def _(w):
    code = w.wait_exit(90)
    assert code == 0, f"exit code {code}"

@step(r'I run "(.*)" with a (\d+) second limit')
def _(w, cmd, secs):
    try: w.run(cmd, timeout=float(secs))
    except subprocess.TimeoutExpired: raise AssertionError(f"command did not finish within {secs}s")

@step(r'I run "(.*)"')
def _(w, cmd): w.run(cmd)

@step(r"the command succeeds")
def _(w): assert w.last and w.last[0] == 0, f"exit code {w.last[0] if w.last else None}"

@step(r'the output contains "(.*)"')
def _(w, text): assert text in w.last[1], f'"{text}" not in output'

@step(r'branch "([^"]+)" exists')
def _(w, b): assert git(w, "rev-parse", "--verify", "-q", b).returncode == 0, f"no branch {b}"

@step(r'branch "([^"]+)" has a commit touching "([^"]+)"')
def _(w, b, f):
    names = git(w, "log", "--format=", "--name-only", f"main..{b}").stdout.split()
    assert f in names, f"{b} has no commit touching {f} (touched: {names})"

@step(r'worktree "([^"]+)" exists')
def _(w, p): assert (w.repo / p).is_dir(), f"no worktree at {p}"

@step(r'file "([^"]+)" exists')
def _(w, p):
    end = time.time() + WAIT
    while time.time() < end and not (w.repo / p).exists(): time.sleep(0.1)
    assert (w.repo / p).exists(), f"{p} missing"

@step(r'file "([^"]+)" does not exist')
def _(w, p):
    time.sleep(1)
    assert not (w.repo / p).exists(), f"{p} exists"

@step(r'the model received "(.*)" in the conversation')
def _(w, text):
    end = time.time() + WAIT
    while time.time() < end:
        mains = [r for r in w.requests() if r["thread"] == "main"]
        if mains and text in mains[-1]["conversation"]: return
        time.sleep(0.2)
    raise AssertionError(f'latest main request did not include "{text}"')

@step(r'the last request to the model used model "([^"]+)"')
def _(w, model):
    mains = [r for r in w.requests() if r["thread"] == "main"]
    assert mains and mains[-1]["model"] == model, f"last model was {mains[-1]['model'] if mains else None}"

@step(r'a request from thread "([^"]+)" used model "([^"]+)"')
def _(w, thread, model):
    used = {r["model"] for r in w.requests() if r["thread"] == thread}
    assert model in used, f"{thread} used {used or 'nothing'}"

@step(r'the MCP config has server "([^"]+)" but not "([^"]+)"')
def _(w, yes, no):
    servers = json.loads((w.cfg / "thread" / "mcp.json").read_text()).get("servers", {})
    assert yes in servers and no not in servers, f"servers: {list(servers)}"

@step(r'fact "([^"]+)" is "([^"]+)"')
def _(w, fact, status):
    end = time.time() + WAIT
    while time.time() < end:
        w.run("thread facts --all")
        line = next((l for l in w.last[1].splitlines() if fact in l), None)
        if line and f"[{status}" in line: return
        time.sleep(0.3)
    raise AssertionError(f"fact line: {line!r}")

@step(r'thread "([^"]+)" has a memory notice containing "(.*)"')
def _(w, thread, text):
    text = text.replace('\\"', '"')
    end = time.time() + WAIT
    while time.time() < end:
        rows = w.db().execute("SELECT body FROM inbox WHERE thread_id = ? AND sender = 'memory'", (thread,)).fetchall()
        if any(text in r[0] for r in rows): return
        time.sleep(0.2)
    raise AssertionError(f"notices for {thread}: {rows}")

@step(r'thread "([^"]+)" has no live run')
def _(w, name):
    live = w.db().execute("SELECT id FROM runs WHERE thread_id = ? AND ended_at IS NULL", (name,)).fetchall()
    assert not live, f"live runs left on {name}: {[r[0] for r in live]}"

@step(r'thread "([^"]+)" exists')
def _(w, thread):
    assert w.db().execute("SELECT 1 FROM threads WHERE id = ?", (thread,)).fetchone(), f"no thread {thread}"

@step(r'thread "([^"]+)" called a tool matching "([^"]+)"')
def _(w, thread, pat):
    rows = w.db().execute("SELECT e.tool FROM events e JOIN runs r ON r.id = e.run_id WHERE r.thread_id = ? AND e.kind = 'tool_call'", (thread,)).fetchall()
    assert any(pat in (r[0] or "") for r in rows), f"{thread} called: {[r[0] for r in rows]}"

# ---------------------------------------------------------------- run

def run_scenario(sc):
    w = World(live="live" in sc.tags)
    t0 = time.time()
    try:
        for st in sc.background + sc.steps:
            for rx, fn in STEPS:
                m = rx.fullmatch(st.text)
                if m:
                    try: fn(w, *m.groups())
                    except AssertionError as e:
                        return (sc, False, f"{st.kw} {st.text}\n      → {e}", w.out[-1500:], time.time() - t0)
                    break
            else:
                return (sc, False, f"undefined step: {st.kw} {st.text}", "", time.time() - t0)
        return (sc, True, "", "", time.time() - t0)
    except Exception as e:
        return (sc, False, f"error: {type(e).__name__}: {e}", w.out[-1500:], time.time() - t0)
    finally:
        w.cleanup()

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("features", nargs="*")
    ap.add_argument("--tags", default="offline", help="comma-separated; a scenario runs if it has any of them")
    ap.add_argument("-k", default="", help="only scenarios whose name contains this")
    ap.add_argument("-j", "--jobs", type=int, default=6)
    ap.add_argument("-v", "--verbose", action="store_true", help="print output tail for failures")
    a = ap.parse_args()
    if not BIN.exists(): sys.exit(f"build first: cargo build --release ({BIN} missing)")
    files = [Path(f) for f in a.features] or sorted((HERE / "features").glob("*.feature"))
    tags = {t.strip().lstrip("@") for t in a.tags.split(",") if t.strip()}
    scenarios = [s for f in files for s in parse(f) if s.tags & tags and a.k.lower() in s.name.lower()]
    print(f"running {len(scenarios)} scenarios ({', '.join(sorted(tags))}) against {BIN}\n")
    with ThreadPoolExecutor(a.jobs) as ex:
        results = list(ex.map(run_scenario, scenarios))
    feature = None
    failed = 0
    for sc, ok, why, tail, dt in results:
        if sc.feature != feature:
            feature = sc.feature; print(f"Feature: {feature}")
        print(f"  {'✓' if ok else '✗'} {sc.name}  ({dt:.1f}s)")
        if not ok:
            failed += 1
            print(f"      {why}")
            if a.verbose and tail:
                print("      ┌ output tail"); print("\n".join("      │ " + l for l in tail.splitlines()[-25:]))
    print(f"\n{len(results) - failed} passed, {failed} failed")
    sys.exit(1 if failed else 0)

if __name__ == "__main__":
    main()
