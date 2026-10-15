#!/usr/bin/env python3
import difflib
import json
import os
import re
import shutil
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "scripts" / "compat"
BIN = ROOT / "target" / "debug" / "nanolb"
DEAD = "http://127.0.0.1:9"

ADMIN_OPS = [
    "status", "accounts_list", "account_import", "account_add_tokens", "account_add_openrouter", "login_poll",
    "account_update", "account_remove", "account_refresh", "account_warmup", "keys_list", "key_create", "key_update",
    "key_remove", "usage", "requests", "settings_get", "settings_update", "models", "sessions_list", "session_delete",
    "sessions_purge", "reset_credits_list", "reset_credits_consume", "plan", "analytics", "nope",
]

ROUTES = [
    ("POST", "/v1/responses"), ("GET", "/v1/responses"), ("POST", "/backend-api/codex/responses"),
    ("GET", "/backend-api/codex/responses"), ("GET", "/backend-api/codex/models"), ("POST", "/v1/chat/completions"),
    ("POST", "/api/v1/chat/completions"), ("GET", "/v1/models"), ("GET", "/api/v1/models"), ("GET", "/api/v1/key"),
    ("GET", "/api/v1/auth/key"), ("POST", "/v1/messages"), ("POST", "/v1/messages/count_tokens"),
    ("GET", "/backend-api/wham/usage"), ("GET", "/api/codex/usage"), ("GET", "/v1/usage"), ("POST", "/backend-api/files"),
    ("POST", "/backend-api/files/file_x/uploaded"), ("POST", "/backend-api/transcribe"), ("POST", "/v1/audio/transcriptions"),
    ("POST", "/v1/images/generations"), ("POST", "/backend-api/codex/images/generations"), ("POST", "/v1/images/edits"),
    ("POST", "/backend-api/codex/images/edits"), ("POST", "/v1/embeddings"), ("POST", "/backend-api/codex/realtime/calls"),
    ("POST", "/v1/realtime/calls"), ("GET", "/v1/realtime"), ("GET", "/v1/live/rtc_x"), ("GET", "/backend-api/codex/rtc_x"),
    ("GET", "/admin/status"), ("POST", "/admin/status"), ("GET", "/health"), ("POST", "/mcp"), ("GET", "/nope"),
    ("GET", "/v1/chat/completions"), ("DELETE", "/v1/responses"),
]

JSON_POSTS = [
    "/v1/responses", "/backend-api/codex/responses", "/v1/chat/completions", "/api/v1/chat/completions", "/v1/messages",
    "/v1/messages/count_tokens", "/v1/images/generations", "/backend-api/codex/images/generations",
    "/backend-api/codex/images/edits", "/v1/embeddings",
]

KEYED_GETS = [
    "/v1/models", "/api/v1/models", "/api/v1/key", "/api/v1/auth/key", "/backend-api/codex/models",
    "/backend-api/wham/usage", "/api/codex/usage", "/v1/usage", "/v1/responses", "/v1/realtime", "/v1/live/rtc_x",
    "/v1/live/..", "/backend-api/codex/rtc_x",
]

VOLATILE = {"created", "created_at", "started_at", "uptime_secs", "label", "prefix", "generated_at"}
ISO = re.compile(r"^\d{4}-\d\d-\d\dT")


def mask(v, key=None):
    if key in VOLATILE and not isinstance(v, (dict, list)):
        return f"<{key}>"
    if isinstance(v, dict):
        return {k: mask(x, k) for k, x in v.items()}
    if isinstance(v, list):
        return [mask(x) for x in v]
    if isinstance(v, str) and ISO.match(v):
        return "<ts>"
    return v


def shape(v):
    if isinstance(v, dict):
        return {"object": sorted(v)}
    if isinstance(v, list):
        return {"list": len(v)}
    return {type(v).__name__: v}


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def base_env(home):
    env = {k: v for k, v in os.environ.items() if not k.startswith("NANOLB_") and "PROXY" not in k.upper()}
    env.update({
        "NANOLB_HOME": str(home), "HTTPS_PROXY": DEAD, "HTTP_PROXY": DEAD, "ALL_PROXY": DEAD,
        "NO_PROXY": "127.0.0.1,localhost", "NANOLB_LOG": "warn", "NO_COLOR": "1",
    })
    return env


def run(args, env, stdin=None):
    p = subprocess.run([str(BIN), *args], env=env, input=stdin, capture_output=True, text=True, timeout=60)
    return p.returncode, p.stdout, p.stderr


def subcommands(help_text):
    out, inside = [], False
    for line in help_text.splitlines():
        if line.startswith("Commands:"):
            inside = True
            continue
        if inside:
            if not line.strip():
                break
            name = line.split()[0]
            if name != "help":
                out.append(name)
    return out


def cli_help(env, dest):
    dest.mkdir(parents=True)
    stack = [[]]
    while stack:
        path = stack.pop()
        code, out, err = run([*path, "--help"], env)
        name = " ".join(["nanolb", *path])
        (dest / (name.replace(" ", "_") + ".txt")).write_text(f"$ {name} --help\nexit {code}\n{out}{err}")
        stack.extend([*path, s] for s in reversed(subcommands(out)))
    code, out, err = run(["--version"], env)
    (dest / "nanolb_--version.txt").write_text(f"exit {code}\n{out}{err}")


def http(method, url, body=None, headers=None):
    req = urllib.request.Request(url, data=body, method=method, headers=headers or {})
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.status, dict(r.headers), r.read()
    except urllib.error.HTTPError as e:
        return e.code, dict(e.headers), e.read()
    except Exception as e:
        return 0, {}, f"{type(e).__name__}".encode()


def decode(b):
    try:
        return json.loads(b)
    except Exception:
        return b.decode(errors="replace") if isinstance(b, bytes) else b


def probe(method, url, body=None, headers=None):
    status, hs, raw = http(method, url, body, headers)
    v = decode(raw)
    rec = {"status": status}
    if "retry-after" in {k.lower() for k in hs}:
        rec["retry_after"] = True
    rec["body"] = mask(v) if isinstance(v, (dict, list)) else v
    return rec


def wait_health(base, proc):
    for _ in range(300):
        if proc.poll() is not None:
            raise SystemExit("nanolb exited during start")
        if http("GET", base + "/health")[0] == 200:
            return
        time.sleep(0.1)
    raise SystemExit("nanolb did not become healthy")


def mcp(env):
    msgs = [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize",
         "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "compat", "version": "0"}}},
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
        {"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "nope", "arguments": {}}},
        {"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "account_remove", "arguments": {}}},
        {"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "settings_get", "arguments": {}}},
    ]
    p = subprocess.Popen([str(BIN), "mcp"], env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=subprocess.DEVNULL, text=True)
    got = {}
    for m in msgs:
        p.stdin.write(json.dumps(m) + "\n")
        p.stdin.flush()
        if "id" not in m:
            continue
        while True:
            line = p.stdout.readline()
            if not line:
                raise SystemExit("mcp stdio closed")
            r = json.loads(line)
            if r.get("id") == m["id"]:
                got[m["id"]] = r
                break
    p.stdin.close()
    p.wait(timeout=10)
    init = got[1]["result"]
    init.get("serverInfo", {}).pop("version", None)
    tools = {t["name"]: {"description": t.get("description"), "inputSchema": t.get("inputSchema")}
             for t in got[2]["result"]["tools"]}
    calls = {}
    for i in (3, 4, 5):
        res = got[i]["result"]
        text = res["content"][0]["text"]
        calls[msgs[i]["params"]["name"]] = {"isError": res.get("isError"), "text": mask(decode(text))}
    return {"initialize": init, "tools": tools, "tool_names": sorted(tools), "calls": calls}


def capture(dest):
    if dest.exists():
        shutil.rmtree(dest)
    dest.mkdir(parents=True)
    subprocess.run(["cargo", "build", "--quiet"], cwd=ROOT, check=True)
    home = Path(tempfile.mkdtemp(prefix="nanolb-compat-"))
    try:
        env = base_env(home)
        cli_help(env, dest / "cli")
        port = free_port()
        base = f"http://127.0.0.1:{port}"
        log = open(home / "serve.log", "w")
        proc = subprocess.Popen([str(BIN), "serve", "--host", "127.0.0.1", "--port", str(port)], env=env,
                                stdout=log, stderr=log)
        try:
            wait_health(base, proc)
            token = (home / "admin_token").read_text().strip()
            admin = {"Authorization": f"Bearer {token}", "Content-Type": "application/json"}

            routes = {}
            for method, path in ROUTES:
                routes[f"{method} {path}"] = probe(method, base + path)
            routes["GET /v1/models anthropic-version"] = probe("GET", base + "/v1/models", headers={"anthropic-version": "2023-06-01"})
            routes["POST /admin/status bad-token"] = probe("POST", base + "/admin/status", b"{}", {"Authorization": "Bearer nope"})
            routes["POST /admin/nope"] = probe("POST", base + "/admin/nope", b"{}", admin)
            routes["POST /admin/usage invalid-json"] = probe("POST", base + "/admin/usage", b"{bad", admin)
            routes["GET /admin/usage?since=1h&group=model"] = probe("GET", base + "/admin/usage?since=1h&group=model", headers=admin)
            routes["POST /admin/settings_update unknown"] = probe("POST", base + "/admin/settings_update", b'{"nope":1}', admin)
            routes["POST /admin/analytics bad-view"] = probe("POST", base + "/admin/analytics", b'{"view":"nope"}', admin)
            routes["POST /admin/key_update missing"] = probe("POST", base + "/admin/key_update", b'{"id":"nope"}', admin)
            routes["POST /admin/account_update missing"] = probe("POST", base + "/admin/account_update", b'{"id":"nope"}', admin)
            routes["POST /admin/login_poll missing"] = probe("POST", base + "/admin/login_poll", b'{"login_id":"nope"}', admin)
            routes["POST /admin/sessions_purge all"] = probe("POST", base + "/admin/sessions_purge", b'{"all":true}', admin)

            ops = {}
            for op in ADMIN_OPS:
                status, _, raw = http("POST", f"{base}/admin/{op}", b"{}", admin)
                v = decode(raw)
                ops[op] = {"status": status, **(shape(v) if status < 300 else {"error": v})}
            settings = decode(http("POST", f"{base}/admin/settings_get", b"{}", admin)[2])

            created = decode(http("POST", f"{base}/admin/key_create", json.dumps({"name": "compat"}).encode(), admin)[2])
            key = {"Authorization": f"Bearer {created['key']}", "Content-Type": "application/json"}
            keyed = {"key_create": shape(created)}
            for path in JSON_POSTS:
                keyed[f"POST {path} invalid-json"] = probe("POST", base + path, b"{not json", key)
                keyed[f"POST {path} empty-object"] = probe("POST", base + path, b"{}", key)
                keyed[f"POST {path} array-body"] = probe("POST", base + path, b"[1]", key)
                body = {"model": "gpt-5.4", "input": "hi", "messages": [{"role": "user", "content": "hi"}],
                        "max_tokens": 8, "prompt": "a cat"}
                keyed[f"POST {path} no-accounts"] = probe("POST", base + path, json.dumps(body).encode(), key)
            keyed["POST /v1/embeddings slash-model"] = probe(
                "POST", base + "/v1/embeddings", json.dumps({"model": "openai/text-embedding-3-small", "input": "x"}).encode(), key)
            keyed["POST /v1/chat/completions openrouter-model"] = probe(
                "POST", base + "/v1/chat/completions",
                json.dumps({"model": "z-ai/glm-5.3-flash", "messages": [{"role": "user", "content": "hi"}]}).encode(), key)
            for path in ["/v1/realtime/calls", "/backend-api/codex/realtime/calls"]:
                keyed[f"POST {path}"] = probe("POST", base + path, b"v=0", {**key, "Content-Type": "application/sdp"})
            for path in ["/backend-api/transcribe", "/v1/audio/transcriptions", "/v1/images/edits"]:
                keyed[f"POST {path} not-multipart"] = probe("POST", base + path, b"x", key)
            for path in KEYED_GETS:
                keyed[f"GET {path}"] = probe("GET", base + path, headers=key)
            keyed["GET /v1/models anthropic"] = probe("GET", base + "/v1/models", headers={**key, "anthropic-version": "2023-06-01"})

            cli = {}
            cenv = {**env, "NANOLB_URL": base, "NANOLB_ADMIN_TOKEN": token}
            for args in (["call", "nope"], ["call", "status", "{bad"], ["keys", "create", "x", "--limit", "bad"],
                         ["settings", "get"], ["accounts", "rm", "nobody"], ["sessions", "purge"]):
                code, out, err = run(args, cenv)
                cli[" ".join(args)] = {"exit": code, "stdout": mask(decode(out)) if out else "", "stderr": err}
            code, out, err = run(["status"], {**env, "NANOLB_URL": DEAD, "NANOLB_ADMIN_TOKEN": "x"})
            cli["status unreachable"] = {"exit": code, "stdout": out, "stderr": err}

            tools = mcp(cenv)
        finally:
            proc.send_signal(signal.SIGINT)
            try:
                proc.wait(timeout=15)
            except subprocess.TimeoutExpired:
                proc.kill()
            log.close()
        db = sqlite3.connect(home / "nanolb.db")
        schema = [r[0] for r in db.execute("SELECT sql FROM sqlite_master ORDER BY name")]
        db.close()
        for name, data in {"routes": routes, "admin_ops": ops, "settings_get": settings, "keyed": keyed, "cli_runs": cli,
                           "mcp": tools, "db_schema": schema}.items():
            (dest / f"{name}.json").write_text(json.dumps(data, indent=2, sort_keys=True, ensure_ascii=False) + "\n")
    finally:
        shutil.rmtree(home, ignore_errors=True)


def files(d):
    return sorted(p.relative_to(d) for p in d.rglob("*") if p.is_file())


def check():
    capture(OUT / "current")
    base, cur = OUT / "baseline", OUT / "current"
    diffs = 0
    for rel in sorted(set(files(base)) | set(files(cur))):
        a = (base / rel).read_text().splitlines(keepends=True) if (base / rel).exists() else []
        b = (cur / rel).read_text().splitlines(keepends=True) if (cur / rel).exists() else []
        if a != b:
            diffs += 1
            sys.stdout.writelines(difflib.unified_diff(a, b, f"baseline/{rel}", f"current/{rel}"))
    print(f"compat: {diffs} file(s) differ" if diffs else "compat: no differences")
    return 1 if diffs else 0


def main():
    mode = sys.argv[1] if len(sys.argv) > 1 else "check"
    if mode == "baseline":
        capture(OUT / "baseline")
        print(f"baseline written to {OUT / 'baseline'}")
        return 0
    if mode == "check":
        return check()
    print("usage: compat.py baseline|check", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())
