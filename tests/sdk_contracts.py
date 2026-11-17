#!/usr/bin/env python3
"""三语言逐夹具验收与防漏跑；per-fixture three-language acceptance rejects missing execution."""
from pathlib import Path
import argparse
import os
import socket
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "tests/fixtures/protocol"
SCENARIOS = ("batch", "subscription", "redelivery", "ack-cancel", "pool")


def execute(args, cwd=ROOT, env=None, timeout=180):
    """保留失败输出以供计数；capture failed output so fixture results remain countable."""
    result = subprocess.run(args, cwd=cwd, env=env, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, text=True, encoding="utf-8",
                            errors="replace", timeout=timeout, check=False)
    print(result.stdout, end="", flush=True)
    return result


def run(args, cwd=ROOT, env=None):
    """真实构建失败即停止；stop on actual build failures."""
    execute(args, cwd, env).check_returncode()


def fixture_plan(root=FIXTURES, sessions=False):
    """ID 来自共享输入，不以测试报数反推计划；derive IDs from inputs, never reported totals."""
    plan = set()
    scopes = (("sessions", "sessions"),) if sessions else (
        ("values", "values"), ("invalid-values", "invalid-values"), ("frames", "frames"),
        ("commands", "commands"), ("info-fields", "info-fields"), ("live-values", "values"))
    for scope, name in scopes:
        for line in (root / f"{name}.tsv").read_text(encoding="utf-8").splitlines():
            if not line.strip() or line.startswith("#"):
                continue
            row = [field.strip() for field in line.split("|")]
            identifier = ".".join(row[:2]) if scope == "info-fields" else row[0]
            key = (scope, identifier)
            if not identifier or key in plan:
                raise ValueError(f"duplicate/empty fixture ID: {scope}/{identifier}")
            plan.add(key)
    if not sessions:
        plan.update(("scenario", identifier) for identifier in SCENARIOS)
    return plan


def inspect_reports(output, plan):
    """计划 ID 恰好一个结果，失败/跳过/多报/漏报均拒绝。
    Require exactly one result per planned ID; reject failures, skips, extras, and omissions.
    """
    seen = {}
    problems = []
    for line in output.splitlines():
        if not line.startswith("LING_CONTRACT\t"):
            continue
        fields = line.split("\t")
        if len(fields) != 4 or fields[3] not in ("passed", "failed", "skipped"):
            problems.append(f"malformed report: {line}")
            continue
        _, scope, identifier, status = fields
        key = (scope, identifier)
        if key in seen:
            problems.append(f"duplicate report: {scope}/{identifier}")
        else:
            seen[key] = status
        if key not in plan:
            problems.append(f"unplanned report: {scope}/{identifier}")
    counts = {status: sum(status == actual for key, actual in seen.items() if key in plan)
              for status in ("passed", "failed", "skipped")}
    missing = plan - seen.keys()
    counts.update(planned=len(plan), executed=counts["passed"] + counts["failed"], missing=len(missing))
    if missing:
        problems.append("missing reports: " + ", ".join(f"{scope}/{identifier}" for scope, identifier in sorted(missing)))
    if counts["failed"] or counts["skipped"]:
        problems.append(f"failed={counts['failed']} skipped={counts['skipped']}")
    return counts, problems


def contracts(language, transport, args, plan, table, cwd=ROOT, env=None):
    """进程与逐 ID 均成功才通过；require both process and per-ID success."""
    result = execute(args, cwd, env)
    counts, problems = inspect_reports(result.stdout, plan)
    table.append((language, transport, counts))
    if result.returncode:
        problems.append(f"process exit={result.returncode}")
    if problems:
        raise RuntimeError(f"{language} / {transport}: " + "; ".join(problems))


def print_counts(table, planned, session_count):
    """失败时也显示结果，local-peer 不冒充真实 TLS。
    Show results even on failure; local-peer never claims real TLS coverage.
    """
    print("\nSDK fixture counts (real-server TCP/mutual TLS; controlled local-peer separately)", flush=True)
    print("language | transport | planned | executed | failed | skipped | missing", flush=True)
    measured = {(language, transport): counts for language, transport, counts in table}
    for transport in ("local-peer", "TCP", "mutual TLS"):
        for language in ("Rust", "Go", "Java"):
            size = session_count if transport == "local-peer" else planned
            counts = measured.get((language, transport), dict(planned=size, executed=0, failed=0, skipped=0, missing=size))
            print(f"{language} | {transport} | " + " | ".join(str(counts[key]) for key in
                  ("planned", "executed", "failed", "skipped", "missing")), flush=True)


def free_port():
    """只为隔离测试选择临时端口；select an ephemeral port solely for isolated tests."""
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def executable(path):
    """Windows 使用实际 EXE 路径；use the actual EXE path on Windows."""
    return str(path.with_suffix(".exe") if os.name == "nt" else path)


def main():
    """默认 Reliable；Windows Fast 回归必须显式选择。
    Default to Reliable; Windows Fast regression requires an explicit choice.
    """
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--event-mode", choices=("reliable", "fast"), default="reliable")
    parser.add_argument("--no-go-race", action="store_true", help="explicitly disable Go race detector for local environments without cgo")
    args = parser.parse_args()
    if os.name == "nt" and args.event_mode == "reliable":
        parser.error("Reliable requires Linux/macOS; use --event-mode fast for explicit Windows Fast contracts")
    print(f"SDK acceptance mode: events={args.event_mode}; Go race={'disabled (explicit --no-go-race)' if args.no_go_race else 'enabled'}", flush=True)
    go_test = ["go", "test"] + ([] if args.no_go_race else ["-race"])
    plan, sessions = fixture_plan(), fixture_plan(sessions=True)
    run(["cargo", "build", "--locked", "-p", "ling-client", "--example", "contracts"])
    run(["cargo", "build", "--locked", "-p", "ling-server"])
    classes = ROOT / "target/java-contracts"
    classes.mkdir(parents=True, exist_ok=True)
    sources = sorted((ROOT / "sdks/java/src").rglob("*.java"))
    run(["javac", "-Xlint:all", "-Werror", "-d", str(classes), *map(str, sources)])
    env = {k: v for k, v in os.environ.items() if not k.startswith("LINGER_TEST_")}
    table = []
    try:
        contracts("Rust", "local-peer", ["cargo", "test", "--locked", "-p", "ling-client", "--test", "sessions", "--", "--nocapture"], sessions, table, env=env)
        contracts("Go", "local-peer", [*go_test, "-count=1", "-v", "-run", "^TestSessions$", "./..."], sessions, table, cwd=ROOT / "sdks/go", env=env)
        contracts("Java", "local-peer", ["java", "-ea", "-cp", str(classes), "io.linger.cache.SessionContracts", str(FIXTURES)], sessions, table, env=env)
        for mutual in (False, True):
            with tempfile.TemporaryDirectory(prefix="linger-sdk-") as temporary:
                directory = Path(temporary)
                port = free_port()
                config = directory / "linger.toml"
                text = (f'[server]\nport={port}\n[engine]\nshard_count=2\n[events]\nmode="{args.event_mode}"\n'
                        'retry_interval_ms=10\n[log]\nlevel="error"\n')
                if args.event_mode == "reliable":
                    text += '[event_journal]\ndir="journal"\n'
                transport_env = env.copy()
                transport_env["LINGER_TEST_ADDR"] = f"127.0.0.1:{port}"
                if mutual:
                    ca, cert, key = (ROOT / "tests/fixtures/tls" / name for name in ("ca.pem", "identity.pem", "identity-key.pem"))
                    identity = directory / "identity.p12"
                    run(["openssl", "pkcs12", "-export", "-in", str(cert), "-inkey", str(key), "-certfile", str(ca), "-out", str(identity), "-passout", "pass:test-only"])
                    text += f'[tls]\nenabled=true\ncert_file="{cert.as_posix()}"\nkey_file="{key.as_posix()}"\nca_file="{ca.as_posix()}"\n'
                    transport_env.update(LINGER_TEST_CA=str(ca), LINGER_TEST_CERT=str(cert), LINGER_TEST_KEY=str(key), LINGER_TEST_PKCS12=str(identity))
                config.write_text(text, encoding="utf-8")
                with (directory / "server.log").open("w+", encoding="utf-8") as log:
                    server = subprocess.Popen([executable(ROOT / "target/debug/ling"), "--config", str(config)], cwd=directory, stdout=log, stderr=log)
                    try:
                        deadline = time.monotonic() + 10
                        while True:
                            if server.poll() is not None:
                                log.seek(0)
                                raise RuntimeError(log.read())
                            try:
                                with socket.create_connection(("127.0.0.1", port), timeout=0.1):
                                    break
                            except OSError:
                                if time.monotonic() > deadline:
                                    raise TimeoutError("server startup")
                                time.sleep(0.02)
                        transport = "mutual TLS" if mutual else "TCP"
                        contracts("Rust", transport, [executable(ROOT / "target/debug/examples/contracts")], plan, table, env=transport_env)
                        contracts("Go", transport, [*go_test, "-count=1", "-v", "-skip", "^TestSessions$", "./..."], plan, table, cwd=ROOT / "sdks/go", env=transport_env)
                        contracts("Java", transport, ["java", "-ea", "-cp", str(classes), "io.linger.cache.Contracts", str(FIXTURES)], plan, table, env=transport_env)
                    finally:
                        server.terminate()
                        try:
                            server.wait(timeout=10)
                        except subprocess.TimeoutExpired:
                            server.kill()
                            server.wait()
    finally:
        print_counts(table, len(plan), len(sessions))


if __name__ == "__main__":
    main()
