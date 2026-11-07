"""Scriptable OpenAI-compatible model server for thread's BDD tests.

    python3 mock_llm.py <port>      env: MOCK_LOG=<jsonl path>, MOCK_MODELS=a,b,c

Behavior is driven by phrases in the latest user message (harness context blocks
like <repo>, <thread>, <memory> are stripped first):

    spawn thread <name>: <task>[, worktree]   main calls spawn_thread
    spawn thread <name> with model <m>: <task>  spawn_thread with a model
    move <name> to model <m>                  main calls set_thread_model
    think about <x>                           replies with reasoning_content (3 lines)
    run: <shell command>                      calls bash, then replies "ran: <output>" (or "denied: ...")
    remember <entity> <attribute> <value>     calls remember, then echoes the tool's answer
    a task containing "slow"                  background thread waits 8s before its first reply
    a task containing "late report"           background thread's handoff takes 4s (its report reaches main late)
    a conversation containing "broken handoff"   handoffs (main's and threads') fail with HTTP 500
    a conversation containing "garbled handoff"  handoffs answer prose instead of JSON
    a background thread's first task          calls bash `pwd`, then replies "Report: done"
    anything else                             "echo: <text>"
    json_schema requests (nap, handoff)       a valid object for the schema
    a model whose name contains "notools"    rejects requests with tools, as Ollama does

It also serves Ollama's native API (GET /api/tags, POST /api/chat) over the same script.
"""
import json, os, re, sys, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOG = os.environ.get("MOCK_LOG")
MODELS = [m.strip() for m in os.environ.get("MOCK_MODELS", "mock-large,mock-small").split(",") if m.strip()]
BLOCKS = re.compile(r"<(repo|thread|memory|skills|earlier_in_this_conversation|harness_instructions)\b[^>]*>.*?</\1>", re.S)

def text_of(c):
    if isinstance(c, str): return c
    return "\n".join(b.get("text", "") for b in (c or []) if isinstance(b, dict))

def user_text(msgs):
    for m in reversed(msgs):
        if m["role"] == "user": return BLOCKS.sub("", text_of(m["content"])).strip()
    return ""

def reply(content=None, calls=None):
    msg = {"role": "assistant", "content": content}
    if calls: msg["tool_calls"] = calls
    return {"choices": [{"index": 0, "message": msg, "finish_reason": "tool_calls" if calls else "stop"}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 10}}

def call(name, args):
    return [{"id": f"c{time.time_ns()}", "type": "function", "function": {"name": name, "arguments": json.dumps(args)}}]

HANDOFF = ("This run is ending", "The user is stepping away")

def respond(body):
    if "notools" in (body.get("model") or "") and body.get("tools"):
        return 400, {"error": f"registry.ollama.ai/library/{body['model']} does not support tools"}
    msgs = body["messages"]
    if body.get("response_format") and any(h in text_of(msgs[-1].get("content")) for h in HANDOFF):
        convo = "\n".join(text_of(x.get("content")) for x in msgs)
        if "broken handoff" in convo:
            return 500, {"error": "mock: handoff exploded"}
        if "garbled handoff" in convo:
            return 200, reply("Sure! Here is how things stand: all good.")
    return 200, script(body)

def script(body):
    msgs = body["messages"]
    system = msgs[0]["content"] if msgs and msgs[0]["role"] == "system" else ""
    m = re.search(r"You are `([^`]+)`", system)
    thread = m.group(1) if m else "?"
    is_main = thread == "main"
    if LOG:
        with open(LOG, "a") as f:
            f.write(json.dumps({"thread": thread, "model": body.get("model"),
                                "conversation": "\n".join(text_of(x.get("content")) for x in msgs)}) + "\n")

    rf = body.get("response_format")
    if rf:
        if not is_main and "late report" in "\n".join(text_of(x.get("content")) for x in msgs):
            time.sleep(4)
        props = rf.get("json_schema", {}).get("schema", {}).get("properties") or {"state": 0, "summary": 0, "facts": 0}
        full = {"state": f"{thread} state", "outcome": "success", "task_type": "probe", "summary": f"{thread} summary", "facts": []}
        return reply(json.dumps({k: v for k, v in full.items() if k in props}))

    last = msgs[-1]
    if last["role"] == "tool":
        prev = next((x for x in reversed(msgs) if x["role"] == "assistant" and x.get("tool_calls")), None)
        tool = prev["tool_calls"][0]["function"]["name"] if prev else ""
        out = text_of(last["content"])
        if tool == "bash":
            if out.startswith("[error]"): return reply("denied: " + out[:120])
            return reply("ran: " + ((out.strip().splitlines() or [""])[0]))
        if tool == "spawn_thread": return reply("Spawned it.")
        if tool == "remember": return reply(out)
        return reply("Report: done" if not is_main else "ok")

    text = user_text(msgs)
    first_turn = sum(1 for x in msgs if x["role"] == "user") == 1

    mv = re.search(r"move (\S+) to model (\S+)", text)
    if is_main and mv:
        return reply(None, call("set_thread_model", {"name": mv.group(1), "provider": "keep", "model": mv.group(2)}))
    spm = re.search(r"spawn thread (\S+) with model (\S+):\s*(.+)", text)
    if is_main and spm:
        return reply(None, call("spawn_thread", {"name": spm.group(1), "task": spm.group(3).strip(), "worktree": False, "provider": "default", "model": spm.group(2)}))
    sp = re.search(r"spawn thread ([^:]+):\s*(.+)", text)
    if is_main and sp:
        task = sp.group(2).strip()
        wt = task.endswith(", worktree")
        task = task[: -len(", worktree")] if wt else task
        return reply(None, call("spawn_thread", {"name": sp.group(1).strip(), "task": task, "worktree": wt, "provider": "default"}))
    run = re.search(r"run:\s*(.+)", text)
    if run:
        return reply(None, call("bash", {"command": run.group(1).strip()}))
    rem = re.search(r"remember (\S+) (\S+) (.+)", text)
    if rem:
        return reply(None, call("remember", {"entity": rem.group(1), "attribute": rem.group(2), "value": rem.group(3).strip()}))
    th = re.search(r"think about (.+)", text)
    if th:
        r = reply("thought it through")
        r["choices"][0]["message"]["reasoning_content"] = f"**Weighing {th.group(1).strip()}**\nfirst consideration\nsecond consideration"
        return r
    if not is_main and first_turn:
        if "slow" in text: time.sleep(8)
        return reply(None, call("bash", {"command": "pwd"}))
    return reply("echo: " + text[:100].replace("\n", " "))

# ---- Ollama's native /api/chat, translated onto the same script

def from_ollama(body):
    msgs = []
    for m in body["messages"]:
        m = dict(m)
        if m.get("tool_calls"):
            m["tool_calls"] = [{"id": f"o{i}", "type": "function", "function": {"name": c["function"]["name"],
                                "arguments": json.dumps(c["function"]["arguments"])}} for i, c in enumerate(m["tool_calls"])]
        msgs.append(m)
    req = {"model": body.get("model"), "messages": msgs, "tools": body.get("tools")}
    fmt = body.get("format")
    if isinstance(fmt, dict): req["response_format"] = {"type": "json_schema", "json_schema": {"schema": fmt}}
    elif fmt == "json": req["response_format"] = {"type": "json_object"}
    return req

def to_ollama(model, r):
    m = r["choices"][0]["message"]
    msg = {"role": "assistant", "content": m.get("content") or ""}
    if m.get("reasoning_content"): msg["thinking"] = m["reasoning_content"]
    if m.get("tool_calls"):
        msg["tool_calls"] = [{"function": {"name": c["function"]["name"], "arguments": json.loads(c["function"]["arguments"])}} for c in m["tool_calls"]]
    return {"model": model, "message": msg, "done": True, "done_reason": "stop", "prompt_eval_count": 100, "eval_count": 10}

def tags():
    return {"models": [{"name": m, "model": m, "modified_at": f"2026-01-{28 - i:02d}T00:00:00Z", "details": {"parameter_size": "8B"},
                        "capabilities": ["completion"] + ([] if "notools" in m else ["tools"])} for i, m in enumerate(MODELS)]}

class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def send(self, obj, status=200):
        data = json.dumps(obj).encode()
        self.send_response(status); self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data))); self.end_headers()
        try: self.wfile.write(data)
        except BrokenPipeError: pass
    def do_GET(self):
        if self.path.startswith("/api/tags"): return self.send(tags())
        self.send({"object": "list", "data": [{"id": m, "created": len(MODELS) - i, "owned_by": "mock"} for i, m in enumerate(MODELS)]})
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["content-length"])))
        if self.path.startswith("/api/chat"):
            status, r = respond(from_ollama(body))
            return self.send(to_ollama(body.get("model"), r) if status == 200 else r, status)
        status, r = respond(body)
        if status != 200: r = {"error": {"message": r["error"]}}
        self.send(r, status)

ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
