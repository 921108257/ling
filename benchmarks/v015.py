#!/usr/bin/env python3
"""可复现的 0.1.5 基线与指标开销测量；旧 stage7 参数保持不变。
Reproducible 0.1.5 baseline and metrics-cost measurements, leaving stage7 CLI unchanged.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import random
import signal
import shutil
import socket
import statistics
import subprocess
import sys
import time
import urllib.request

sys.dont_write_bytecode = True
from stage7 import CASES
from run import process_metrics, stop

TOOLS = Path(__file__).resolve().parents[1]


def write_json(path, value):
    """逐项保存真实结果；Persist actual results after each case."""
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def digest(path):
    """记录精确输入字节；Record exact input bytes."""
    return hashlib.sha256(path.read_bytes()).hexdigest()


def command(root, *args):
    """采集失败也是环境证据；Capture failures as environment evidence."""
    try:
        result = subprocess.run(args, cwd=root, text=True, capture_output=True, timeout=30)
        return {"command": list(args), "returncode": result.returncode,
                "stdout": result.stdout.strip(), "stderr": result.stderr.strip()}
    except (OSError, subprocess.TimeoutExpired) as error:
        return {"command": list(args), "returncode": None, "error": str(error)}


def consumer_policy(mode, consumer):
    """逐场景声明允许的消费终止；Declare acceptable consumer termination per case."""
    return {"required": consumer != "none", "must_run_until_stop": mode != "fast",
            "expected_errors": ["ERR_OFFSET_EXPIRED"] if mode == "fast" else [],
            "lag_allowed": consumer != "none", "retention_loss_allowed": mode == "fast"}


def validate_result(result, mode, require_status=True, expected_consumer=None):
    """生产成功不能掩盖消费失败；Producer success never masks consumer failure."""
    consumer = result.get("consumer", "none") if expected_consumer is None else expected_consumer
    policy = consumer_policy(mode, consumer)
    failures, expected = [], {}
    if expected_consumer is not None and result.get("consumer") != expected_consumer:
        failures.append("consumer result does not match declared scenario")
    if not result.get("successes", 0) or result.get("errors") or result.get("warmup_errors"):
        failures.append("producer errors or no successful requests")
    consumption = result.get("consumption")
    status = result.get("consumer_status")
    if policy["required"]:
        if not isinstance(consumption, list) or len(consumption) != 3:
            failures.append("missing consumer result")
        else:
            count, errors, _ = consumption
            if not isinstance(errors, dict):
                failures.append("invalid consumer errors")
            else:
                for error, amount in errors.items():
                    code = error.split(":", 1)[0].strip()
                    if code in policy["expected_errors"]:
                        expected[error] = amount
                    else:
                        failures.append(f"unexpected consumer error: {error}")
                if not count and not expected:
                    failures.append("consumer acknowledged no events")
            if status is None and require_status:
                failures.append("missing consumer termination status")
            elif status is not None:
                termination = status.get("termination")
                if termination != "requested_stop" and not (
                        termination == "error" and expected and not failures):
                    failures.append("consumer exited before requested stop")
                if policy["must_run_until_stop"] and termination != "requested_stop":
                    failures.append("continuous consumer exited early")
    elif consumption is not None:
        failures.append("unexpected consumer result")
    return {"valid": not failures, "failures": failures, "policy": policy,
            "expected_fast_lag": expected, "termination_verified": status is not None}


def schedule(repeats, seed, only="", metrics=False):
    """固定种子打散全部重复，避免按场景顺序漂移；Shuffle all repetitions with a fixed seed."""
    selected = set(only.split(",")) if only else {name for name, _, _ in CASES}
    if selected - {name for name, _, _ in CASES}:
        raise ValueError("unknown case")
    cases = [(name, mode, flags, "disabled") for name, mode, flags in CASES if name in selected]
    if metrics:
        cases = [(f"metrics-{name}-{variant}", "off", flags, variant)
                 for name, mode, flags in CASES if name in ("get", "set", "mixed")
                 for variant in ("disabled", "enabled", "scraped")]
    jobs = [(name, mode, flags, repeat, variant) for name, mode, flags, variant in cases
            for repeat in range(1, repeats + 1)]
    random.Random(seed).shuffle(jobs)
    return jobs


def summary(results):
    """并列中位数和极差，不推断不同环境的性能升降；Report median and range without causal claims."""
    groups = {}
    for result in results:
        groups.setdefault(result["name"], []).append(result)
    return {name: {"runs": len(rows), "valid_runs": sum(r["acceptance"]["valid"] for r in rows)
                  if all("acceptance" in r for r in rows) else None,
                   "measurements": {field: {"median": statistics.median(values), "min": min(values), "max": max(values)}
                                    for field in ("successes_per_second", "p50_us", "p99_us", "p999_us",
                                                  "client_cpu_seconds", "server_cpu_seconds")
                                    if (values := [r[field] for r in rows if isinstance(r.get(field), (int, float))])}}
            for name, rows in groups.items()}


def metrics_comparison(results):
    """采集开销和抓取开销分别报告，保留极差；Separate collection from scraping cost and retain ranges."""
    grouped = summary(results)
    comparisons = {}
    for workload in ("get", "set", "mixed"):
        names = [f"metrics-{workload}-{variant}" for variant in ("disabled", "enabled", "scraped")]
        if not all(name in grouped for name in names):
            continue
        variants = {name.rsplit("-", 1)[1]: grouped[name] for name in names}
        base = variants["disabled"]["measurements"]["successes_per_second"]["median"]
        enabled = variants["enabled"]["measurements"]["successes_per_second"]["median"]
        scraped = variants["scraped"]["measurements"]["successes_per_second"]["median"]
        comparisons[workload] = {"variants": variants,
                                 "collection_throughput_change_percent": 100 * (enabled / base - 1),
                                 "scraping_additional_throughput_change_percent": 100 * (scraped / enabled - 1),
                                 "interpretation": "raw same-host comparison; no significance or improvement claim"}
    return comparisons


def cpu_set(text, available):
    """校验 taskset 列表，拒绝越界与重叠；Validate taskset lists before checking overlap."""
    cpus = set()
    for part in text.split(","):
        bounds = part.split("-")
        if not 1 <= len(bounds) <= 2 or any(not value.isdecimal() for value in bounds):
            raise ValueError("invalid CPU list")
        low, high = int(bounds[0]), int(bounds[-1])
        if high < low or high > max(available):
            raise ValueError("invalid CPU range")
        cpus.update(range(low, high + 1))
    if not cpus or not cpus <= set(available):
        raise ValueError("CPU list is outside allowed affinity")
    return cpus


def free_port():
    """每次使用隔离端口；Use an isolated ephemeral port per run."""
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def build(args):
    """先构建干净源码服务，再覆盖测量例程并还原；Build clean server before overlaying only the driver."""
    root = args.source_root
    args.source_status = command(root, "git", "status", "--porcelain", "--untracked-files=all")["stdout"]
    if args.source_status and not args.allow_dirty:
        raise RuntimeError("measurement source checkout must be clean")
    args.source_revision = command(root, "git", "rev-parse", "HEAD")["stdout"]
    bins = args.output / "bin"
    bins.mkdir()
    subprocess.run(["cargo", "build", "--locked", "--release", "-p", "ling-server", "--bin", "ling"], cwd=root, check=True)
    shutil.copy2(root / "target/release/ling", bins / "ling")
    example = root / "crates/server/examples/baseline.rs"
    original = example.read_bytes()
    try:
        example.write_bytes((TOOLS / "crates/server/examples/baseline.rs").read_bytes())
        subprocess.run(["cargo", "build", "--locked", "--release", "-p", "ling-server", "--example", "baseline"], cwd=root, check=True)
        shutil.copy2(root / "target/release/examples/baseline", bins / "baseline")
    finally:
        example.write_bytes(original)
    if command(root, "git", "status", "--porcelain", "--untracked-files=all")["stdout"] != args.source_status:
        raise RuntimeError("measurement source changed during build")


def environment(args):
    """源码、测量工具、CPU绑定及剖析限制分别留证；Separate source, tools, affinity and profiling limits."""
    root = args.source_root
    perf = command(root, "perf", "stat", "-e", "cycles,instructions", "--", "true")
    return {"date_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "revision": args.source_revision, "working_tree": args.source_status,
            "tools_revision": command(TOOLS, "git", "rev-parse", "HEAD")["stdout"],
            "measurement_driver_overlay": "server built from source checkout; baseline example replaced only for client build, then restored",
            "tool_sha256": {name: digest(TOOLS / name) for name in
                            ("benchmarks/v015.py", "benchmarks/stage7.py", "benchmarks/run.py", "crates/server/examples/baseline.rs")},
            "source_sha256": {str(p.relative_to(root)): digest(p) for folder in ("crates", "sdks/rust")
                              for p in (root / folder).rglob("*") if p.suffix in (".rs", ".toml")},
            "binary_sha256": {p.name: digest(p) for p in (args.output / "bin").iterdir()},
            "cargo_lock_sha256": digest(root / "Cargo.lock"),
            "rustc": command(root, "rustc", "-Vv"), "kernel": command(root, "uname", "-a"),
            "cpu": command(root, "lscpu"), "memory": Path("/proc/meminfo").read_text(),
            "storage": command(root, "lsblk", "-o", "NAME,TYPE,SIZE,ROTA,MODEL"),
            "filesystem": command(root, "df", "-T", str(args.output)),
            "perf_event_paranoid": Path("/proc/sys/kernel/perf_event_paranoid").read_text().strip(),
            "perf_probe": perf,
            "build_profile": "Cargo release defaults; RUSTFLAGS=" + os.environ.get("RUSTFLAGS", ""),
            "affinity_cpus": sorted(os.sched_getaffinity(0)),
            "binding": {"server": args.server_cpus, "client": args.client_cpus},
            "topology": "same-host loopback; disjoint CPU affinity; pipeline=1",
            "resource_sampling_seconds": 0.02,
            "cpu_scope": "whole processes including preload/warmup/cleanup; exact client wait4 CPU, sampled server CPU before shutdown",
            "reliable": "fsync each transaction; 7-day retention; cache and TTL do not recover",
            "arguments": {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()}}


def run_case(args, job, seconds, profile=False):
    """固定二进制、隔离进程、原始错误与精确客户端CPU；Freeze binaries and retain isolated raw outcomes."""
    name, mode, flags, repeat, variant = job
    directory = args.output / f"{name}-{repeat}"
    directory.mkdir()
    port, metrics_port = free_port(), free_port()
    config = f'''[server]
port={port}
max_connections=32
max_request_bytes=1048576
max_response_bytes=1048576
[engine]
shard_count=4
max_memory_bytes=134217728
[events]
enabled={str(mode != "off").lower()}
mode="{mode if mode != "off" else "fast"}"
buffer_bytes=8388608
max_pending_events=128
max_groups=32
retry_interval_ms=5000
[metrics]
enabled={str(variant != "disabled").lower()}
port={metrics_port}
[log]
level="error"
'''
    if mode == "reliable":
        config += '[event_journal]\ndir="journal"\nmax_bytes=67108864\n'
    command_line = ["taskset", "-c", args.client_cpus, str(args.output / "bin/baseline"),
                    "--address", f"127.0.0.1:{port}", "--warmup", "100", "--clients", "4",
                    "--seed", str(args.seed), *flags]
    if "--keys" not in flags:
        command_line += ["--keys", "1000"]
    command_line += ["--operations", "100000"] if "--engine" in flags else ["--seconds", str(seconds)]
    if name == "tls":
        fixture = args.source_root / "tests/fixtures/tls"
        config += f'[tls]\nenabled=true\ncert_file="{fixture / "identity.pem"}"\nkey_file="{fixture / "identity-key.pem"}"\n'
        command_line += ["--tls-server-name", "localhost", "--tls-ca", str(fixture / "ca.pem")]
    (directory / "ling.toml").write_text(config, encoding="utf-8")
    server = client = recorder = None
    samples, scrapes, usage = [], [], None
    started = time.monotonic()
    with (directory / "server.log").open("w") as log, (directory / "result.json").open("w") as output, (directory / "stderr").open("w") as errors, (directory / "perf.stderr").open("w") as perf_errors:
        try:
            if "--engine" not in flags:
                server = subprocess.Popen(["taskset", "-c", args.server_cpus, str(args.output / "bin/ling"), "--config", str(directory / "ling.toml")], cwd=directory, stdout=log, stderr=log)
                deadline = time.monotonic() + 10
                while True:
                    if server.poll() is not None:
                        raise RuntimeError(f"server exited during startup: {directory}")
                    try:
                        with socket.create_connection(("127.0.0.1", port), timeout=0.1):
                            break
                    except OSError:
                        if time.monotonic() > deadline:
                            raise TimeoutError("server startup")
                        time.sleep(0.02)
            if profile:
                # 剖析单独运行，不混入矩阵的吞吐与延迟样本。
                # Profile separately so instrumentation never enters matrix samples.
                recorder = subprocess.Popen(["perf", "record", "-F", "99", "--call-graph", "dwarf",
                                             "-o", str(directory / "perf.data"), "-p", str(server.pid)],
                                            stdout=perf_errors, stderr=perf_errors)
            client = subprocess.Popen(command_line, cwd=directory, stdout=output, stderr=errors)
            deadline, next_scrape = time.monotonic() + seconds + 120, time.monotonic()
            while True:
                pid, status, usage = os.wait4(client.pid, os.WNOHANG)
                if pid:
                    client.returncode = os.waitstatus_to_exitcode(status)
                    break
                row = {"seconds": time.monotonic() - started, "client": process_metrics(client.pid)}
                if server:
                    if server.poll() is not None:
                        raise RuntimeError(f"server exited during workload: {directory}")
                    row["server"] = process_metrics(server.pid)
                    row["journal_bytes"] = sum(p.stat().st_size for p in (directory / "journal").glob("*") if p.is_file())
                samples.append(row)
                if variant == "scraped" and time.monotonic() >= next_scrape:
                    before = time.monotonic()
                    with urllib.request.urlopen(f"http://127.0.0.1:{metrics_port}/metrics", timeout=5) as response:
                        body = response.read(2 * 1024 * 1024 + 1)
                    if len(body) > 2 * 1024 * 1024:
                        raise RuntimeError("metrics response exceeded budget")
                    scrapes.append({"seconds": before - started, "elapsed_seconds": time.monotonic() - before, "bytes": len(body)})
                    next_scrape = before + 1
                if time.monotonic() > deadline:
                    raise TimeoutError(f"client deadline: {directory}")
                time.sleep(0.02)
            if server:
                samples.append({"seconds": time.monotonic() - started, "server": process_metrics(server.pid)})
        finally:
            if recorder is not None and recorder.poll() is None:
                recorder.send_signal(signal.SIGINT)
                try:
                    recorder.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    stop(recorder)
            stop(client)
            stop(server)
            write_json(directory / "resources.json", samples)
            write_json(directory / "scrapes.json", scrapes)
    if client.returncode:
        raise RuntimeError(f"benchmark process failed: {directory}")
    result = json.loads((directory / "result.json").read_text())
    result.update(name=name, repeat=repeat, event_mode=mode, command=command_line, config=config,
                  resources=samples, metrics_variant=variant, metrics_scrapes=scrapes,
                  client_cpu_seconds=usage.ru_utime + usage.ru_stime,
                  server_cpu_seconds=samples[-1]["server"]["cpu_seconds"] if server else None,
                  full_phase_seconds=time.monotonic() - started)
    expected_consumer = flags[flags.index("--consumer") + 1] if "--consumer" in flags else "none"
    result["acceptance"] = validate_result(result, mode, expected_consumer=expected_consumer)
    if profile:
        result["profile"] = {"returncode": recorder.returncode,
                             "stderr": (directory / "perf.stderr").read_text(),
                             "scope": "separate server-only GET workload, including its warmup; 99Hz DWARF stacks"}
        if recorder.returncode == 0:
            with (directory / "perf.script").open("w") as script:
                parsed = subprocess.run(["perf", "script", "-i", str(directory / "perf.data")],
                                        stdout=script, stderr=subprocess.PIPE, text=True, timeout=30)
            result["profile"]["script_returncode"] = parsed.returncode
            result["profile"]["script_stderr"] = parsed.stderr
    write_json(directory / "validated.json", result)
    print(f"{name}-{repeat}: {result['successes_per_second']:.0f} valid/s; acceptance={result['acceptance']}", flush=True)
    return result


def main():
    """Linux上执行有界矩阵和持续运行，任一场景失败使整次验收失败。
    Run bounded Linux matrix/soaks and fail the acceptance if any case fails.
    """
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--source-root", type=Path, default=TOOLS)
    parser.add_argument("--suite", choices=("full", "matrix", "soak", "metrics"), default="full")
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--seconds", type=int, default=3)
    parser.add_argument("--soak-seconds", type=int, default=600)
    parser.add_argument("--seed", type=int, default=20261117)
    parser.add_argument("--only", default="")
    parser.add_argument("--server-cpus")
    parser.add_argument("--client-cpus")
    parser.add_argument("--allow-dirty", action="store_true")
    args = parser.parse_args()
    if not sys.platform.startswith("linux"):
        parser.error("actual measurements require Linux /proc and taskset")
    if not 1 <= args.repeats <= 10 or not 1 <= args.seconds <= 60 or not 1 <= args.soak_seconds <= 3600:
        parser.error("invalid repeat or duration budget")
    available = sorted(os.sched_getaffinity(0))
    if len(available) < 2:
        parser.error("disjoint client/server CPU affinity requires at least two CPUs")
    split = max(1, len(available) // 2)
    args.server_cpus = args.server_cpus or ",".join(map(str, available[:split]))
    args.client_cpus = args.client_cpus or ",".join(map(str, available[split:]))
    try:
        server_cpus = cpu_set(args.server_cpus, available)
        client_cpus = cpu_set(args.client_cpus, available)
    except ValueError as error:
        parser.error(str(error))
    if server_cpus & client_cpus:
        parser.error("client/server CPU sets must be disjoint")
    args.output, args.source_root = args.output.resolve(), args.source_root.resolve()
    args.output.mkdir(parents=True, exist_ok=False)
    build(args)
    metadata = environment(args)
    metadata["profile"] = {"status": "unavailable", "reason": metadata["perf_probe"]}
    write_json(args.output / "environment.json", metadata)
    if metadata["perf_probe"]["returncode"] == 0:
        profiled = run_case(args, ("profile-get", "off", ["--write-percent", "0"], 1, "disabled"), 10, profile=True)
        metadata["profile"] = profiled["profile"]
        write_json(args.output / "environment.json", metadata)
    jobs = [] if args.suite == "soak" else schedule(args.repeats, args.seed, args.only, args.suite == "metrics")
    if args.suite in ("full", "soak"):
        jobs += [(f"soak-{mode}", mode, ["--write-percent", "50", "--ttl-ms", "1000", "--hot", "--rate", "20"], 1, "disabled") for mode in ("fast", "reliable")]
    write_json(args.output / "schedule.json", jobs)
    results = []
    for job in jobs:
        results.append(run_case(args, job, args.soak_seconds if job[0].startswith("soak-") else args.seconds))
        write_json(args.output / "results.json", results)
        write_json(args.output / "summary.json", summary(results))
        if args.suite == "metrics":
            write_json(args.output / "metrics-comparison.json", metrics_comparison(results))
    historical = json.loads((TOOLS / "benchmarks/results/stage7-2026-11-15-results.json").read_text())
    write_json(args.output / "historical-summary.json", {key: summary(rows) for key, rows in historical.items() if isinstance(rows, list)})
    if any(not row["acceptance"]["valid"] for row in results):
        raise SystemExit("one or more benchmark acceptance checks failed; inspect validated.json")


if __name__ == "__main__":
    main()
