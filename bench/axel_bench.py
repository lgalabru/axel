#!/usr/bin/env python3
"""Axel's speed, measured against plain Claude Code.

    python3 -I bench/axel_bench.py [--model haiku] [--runs 3] [--repo PATH]
                                   [--throttle airport] [--skip-model] [--json OUT]

Everything runs in a throwaway Axel home (AXEL_HOME), never ~/.axel, with the
caller's Rex and Axel session variables removed, so nothing opens in your
windows and nothing touches your threads. What it measures:

  startup     axel commands (process start + database) vs `claude --version`
  hook        the guard hook Axel runs before every tool call
  checkpoint  the snapshot Axel takes at each turn, on --repo (default: this
              repo); git writes into a temporary object directory, so the
              measured repo is only read
  bytes       the first request Claude Code sends, plain and under Axel,
              caught by a local listener that answers 503 (no tokens spent)
  model       time to the reply: plain `claude -p`, a cold `axel expedite`,
              a warm `expedite -c`, a spare session (`expedite --warm`) when
              this axel has it, and the cold run again throttled (--throttle)
              when this axel has AXEL_THROTTLE.  Spends a few cents.

Results print as a table and, with --json, are written for comparison
between releases. Standard library only.
"""

import argparse
import http.server
import json
import os
import shutil
import sqlite3
import statistics
import subprocess
import sys
import tempfile
import threading
import time

# Runs of the quick measurements: enough for a stable median.
FAST_RUNS = 20
# A model call that takes longer than this is reported as a timeout.
MODEL_TIMEOUT_S = 180
# Longest the byte-catching listener waits for the harness's first request.
CATCH_TIMEOUT_S = 30


def clean_env(home):
    """The caller's environment without Rex or Axel session variables, with a
    throwaway Axel home."""
    env = {k: v for k, v in os.environ.items() if not (k.startswith("AXEL_") or k.startswith("REX_"))}
    env.pop("ANTHROPIC_BASE_URL", None)
    env.update(AXEL_HOME=home, AXEL_KEYSTORE="file")
    return env


def ms(seconds):
    return round(seconds * 1000, 1)


def expedite(cmd, env, cwd):
    """Time an `axel expedite` run; a failed run stops the benchmark rather
    than be timed as if it had answered."""
    took, proc = timed(cmd, env=env, cwd=cwd)
    if proc.returncode != 0:
        tail = proc.stderr.decode(errors="replace").strip().splitlines()[-3:]
        raise SystemExit(f"{' '.join(cmd[1:])} failed ({proc.returncode}): " + " / ".join(tail))
    return took


def timed(cmd, env=None, cwd=None, stdin=subprocess.DEVNULL, timeout=MODEL_TIMEOUT_S, input_bytes=None):
    start = time.monotonic()
    kwargs = dict(env=env, cwd=cwd, capture_output=True, timeout=timeout)
    if input_bytes is None:
        kwargs["stdin"] = stdin
    else:
        kwargs["input"] = input_bytes
    proc = subprocess.run(cmd, **kwargs)
    return time.monotonic() - start, proc


def median_ms(samples):
    return ms(statistics.median(samples))


def p90_ms(samples):
    ordered = sorted(samples)
    return ms(ordered[min(len(ordered) - 1, int(len(ordered) * 0.9))])


def bench_startup(axel, env):
    out = {}
    for name, cmd in [
        ("axel --version", [axel, "--version"]),
        ("axel thread ls", [axel, "thread", "ls"]),
        ("axel agent ls", [axel, "agent", "ls"]),
        ("claude --version", ["claude", "--version"]),
    ]:
        samples = [timed(cmd, env=env)[0] for _ in range(FAST_RUNS)]
        out[name] = {"median_ms": median_ms(samples), "p90_ms": p90_ms(samples)}
    return out


def bench_hook(axel, env):
    payload = json.dumps({
        "session_id": "bench", "hook_event_name": "PreToolUse", "tool_name": "Bash",
        "tool_input": {"command": "ls -la"}, "cwd": "/tmp", "transcript_path": "/tmp/none.jsonl",
    }).encode()
    samples = [timed([axel, "hook", "claude", "--guard-only"], env=env, input_bytes=payload)[0] for _ in range(FAST_RUNS)]
    return {"guard hook": {"median_ms": median_ms(samples), "p90_ms": p90_ms(samples)}}


def bench_checkpoint(repo):
    """Axel's snapshot (read-tree HEAD, add -A, write-tree) into a temporary
    object directory that borrows the repo's objects: the repo is read only."""
    git_dir = subprocess.run(["git", "-C", repo, "rev-parse", "--absolute-git-dir"], capture_output=True, text=True)
    if git_dir.returncode != 0:
        return {"skipped": f"{repo} is not a git repository"}
    objects = os.path.join(git_dir.stdout.strip(), "objects")
    tracked = subprocess.run(["git", "-C", repo, "ls-files"], capture_output=True, text=True).stdout.count("\n")
    samples = []
    for _ in range(5):
        scratch = tempfile.mkdtemp(prefix="axel-bench-git-")
        env = dict(os.environ, GIT_INDEX_FILE=os.path.join(scratch, "index"), GIT_OBJECT_DIRECTORY=os.path.join(scratch, "objects"),
                   GIT_ALTERNATE_OBJECT_DIRECTORIES=objects)
        os.makedirs(env["GIT_OBJECT_DIRECTORY"])
        start = time.monotonic()
        for step in (["read-tree", "HEAD"], ["add", "-A", "."], ["write-tree"]):
            subprocess.run(["git", "-C", repo, *step], env=env, capture_output=True, check=True)
        samples.append(time.monotonic() - start)
        shutil.rmtree(scratch, ignore_errors=True)
    one = statistics.median(samples)
    return {"repo": repo, "tracked_files": tracked, "snapshot_median_ms": ms(one), "per_turn_ms": ms(2 * one)}


class _Catch(http.server.BaseHTTPRequestHandler):
    """Records the first request's size and answers 503: no model is called."""
    sizes = []

    def do_POST(self):
        length = int(self.headers.get("content-length") or 0)
        self.rfile.read(length)
        if "/messages" in self.path:
            _Catch.sizes.append(length)
        self.send_response(503)
        self.end_headers()

    do_GET = do_POST

    def log_message(self, *args):
        pass


def catch_first_request(run):
    """`run(base_url)` starts a harness pointed at a local listener; the size
    of the first model request it sends."""
    _Catch.sizes = []
    server = http.server.HTTPServer(("127.0.0.1", 0), _Catch)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    base = f"http://127.0.0.1:{server.server_address[1]}"
    proc = run(base)
    deadline = time.monotonic() + CATCH_TIMEOUT_S
    while not _Catch.sizes and time.monotonic() < deadline and proc.poll() is None:
        time.sleep(0.1)
    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
    server.shutdown()
    return _Catch.sizes[0] if _Catch.sizes else None


def bench_bytes(axel, env, work, model):
    plain = catch_first_request(lambda base: subprocess.Popen(
        ["claude", "-p", "--model", model, "say ok"], cwd=work, env=dict(env, ANTHROPIC_BASE_URL=base),
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
    return {"plain claude, first full request": plain}


def has_flag(axel, env, sub, flag):
    help_text = subprocess.run([axel, *sub, "--help"], env=env, capture_output=True, text=True).stdout
    return flag in help_text


def session_ids(home):
    db = os.path.join(home, "axel.db")
    if not os.path.exists(db):
        return []
    with sqlite3.connect(db) as conn:
        return [r[0] for r in conn.execute("SELECT id FROM sessions WHERE ended_at IS NULL")]


def stop_sessions(axel, env, home):
    """End the throwaway home's live sessions, each by its own id."""
    for sid in session_ids(home):
        subprocess.run(["pkill", "-f", f"axel session host {sid}"], capture_output=True)


def axel_request_bytes(home):
    db = os.path.join(home, "axel.db")
    with sqlite3.connect(db) as conn:
        rows = conn.execute("SELECT request_bytes FROM model_requests WHERE endpoint = '/v1/messages' ORDER BY started_at_ms").fetchall()
    return [r[0] for r in rows]


def bench_model(axel, env, home, work, model, runs, throttle):
    out = {}
    prompt = "reply with exactly: ok"
    plain = [timed(["claude", "-p", "--model", model, prompt], env=env, cwd=work)[0] for _ in range(runs)]
    out["plain claude -p"] = {"median_s": round(statistics.median(plain), 2), "runs_s": [round(s, 2) for s in plain]}
    keep = ["--keep", "0"] if has_flag(axel, env, ["expedite"], "--keep") else []
    cold = []
    for _ in range(runs):
        cold.append(expedite([axel, "expedite", *keep, prompt], dict(env, AXEL_INSPECT="1"), work))
    out["axel expedite, cold"] = {"median_s": round(statistics.median(cold), 2), "runs_s": [round(s, 2) for s in cold]}
    sizes = axel_request_bytes(home)
    if sizes:
        # Claude Code sends small side requests (titles) besides the real one.
        out["axel expedite, largest request bytes"] = max(sizes)
    if keep:
        expedite([axel, "expedite", "--keep", "5m", prompt], env, work)
        warm = [expedite([axel, "expedite", "--keep", "5m", "-c", "again: " + prompt], env, work) for _ in range(runs)]
        out["axel expedite -c, warm"] = {"median_s": round(statistics.median(warm), 2), "runs_s": [round(s, 2) for s in warm]}
    else:
        resumed = [expedite([axel, "expedite", "-c", "again: " + prompt], env, work) for _ in range(runs)]
        out["axel expedite -c, resumed"] = {"median_s": round(statistics.median(resumed), 2), "runs_s": [round(s, 2) for s in resumed]}
    if has_flag(axel, env, ["expedite"], "--warm"):
        spare = []
        for _ in range(runs):
            expedite([axel, "expedite", "--warm", "--keep", "5m"], env, work)
            spare.append(expedite([axel, "expedite", "--keep", "0", prompt], env, work))
        out["axel expedite, spare"] = {"median_s": round(statistics.median(spare), 2), "runs_s": [round(s, 2) for s in spare]}
    if throttle:
        took, proc = timed([axel, "expedite", *keep, prompt], env=dict(env, AXEL_THROTTLE=throttle), cwd=work)
        ok = proc.returncode == 0
        out[f"axel expedite, throttled ({throttle})"] = {"s": round(took, 2), "ok": ok}
    stop_sessions(axel, env, home)
    return out


def print_table(results):
    for section, rows in results.items():
        if not isinstance(rows, dict):
            continue
        print(f"\n{section}")
        for name, value in rows.items():
            if isinstance(value, dict):
                shown = ", ".join(f"{k} {v}" for k, v in value.items() if k != "runs_s")
            else:
                shown = str(value)
            print(f"  {name:<42} {shown}")


def main():
    here = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--axel", default=shutil.which("axel") or "axel")
    parser.add_argument("--model", default="haiku")
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--repo", default=here)
    parser.add_argument("--throttle", default=None, help="an AXEL_THROTTLE preset or spec, e.g. airport")
    parser.add_argument("--skip-model", action="store_true", help="only what spends no tokens")
    parser.add_argument("--json", default=None)
    args = parser.parse_args()
    assert 1 <= args.runs <= 20, "--runs between 1 and 20"

    home = tempfile.mkdtemp(prefix="axel-bench-")
    work = os.path.join(home, "work")
    os.makedirs(work)
    subprocess.run(["git", "init", "-q"], cwd=work, check=True)
    with open(os.path.join(work, "main.rs"), "w") as f:
        f.write('fn main() { println!("hi"); }\n')
    env = clean_env(home)
    version = subprocess.run([args.axel, "--version"], env=env, capture_output=True, text=True).stdout.strip()
    subprocess.run([args.axel, "agent", "new", "bench", f"claude/{args.model}"], env=env, capture_output=True, check=True)

    results = {"axel": version, "when": time.strftime("%Y-%m-%d %H:%M:%S"), "model": args.model}
    results["startup"] = bench_startup(args.axel, env)
    results["hook"] = bench_hook(args.axel, env)
    results["checkpoint"] = bench_checkpoint(os.path.abspath(args.repo))
    results["bytes"] = bench_bytes(args.axel, env, work, args.model)
    if not args.skip_model:
        try:
            results["replies"] = bench_model(args.axel, env, home, work, args.model, args.runs, args.throttle)
        finally:
            stop_sessions(args.axel, env, home)
    print(f"{version} · model {args.model} · {results['when']}")
    print_table(results)
    if args.json:
        with open(args.json, "w") as f:
            json.dump(results, f, indent=2)
        print(f"\nwritten to {args.json}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
