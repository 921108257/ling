#!/usr/bin/env python3
"""阶段七场景矩阵与有界持续运行；保留配置、直方图、CPU/RSS 和磁盘采样。
Stage-seven scenario matrix and bounded soak runs retaining configs, histograms, CPU/RSS, and disk samples.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import socket
import shutil
import subprocess
import sys
import time
sys.dont_write_bytecode = True
from run import ROOT, capture, process_metrics, stop


# 对照只改变声明变量，事件消费者成本独立记录。
# Controls change only declared variables; consumer costs are reported separately.
CASES = [
    ("engine-get", "off", ["--engine", "--write-percent", "0"]),
    ("engine-set", "off", ["--engine", "--write-percent", "100"]),
    ("engine-mixed", "off", ["--engine", "--write-percent", "50"]),
    ("get", "off", ["--write-percent", "0"]),
    ("set", "off", ["--write-percent", "100"]),
    ("mixed", "off", ["--write-percent", "50"]),
    ("miss", "off", ["--write-percent", "0", "--miss-percent", "100"]),
    ("hot", "off", ["--write-percent", "50", "--hot"]),
    ("medium", "off", ["--write-percent", "50", "--value-bytes", "4096"]),
    ("large", "off", ["--write-percent", "50", "--value-bytes", "65536", "--keys", "256"]),
    ("ttl", "off", ["--write-percent", "50", "--ttl-ms", "100"]),
    ("fast", "fast", ["--write-percent", "50"]),
    ("reliable", "reliable", ["--write-percent", "50"]),
    ("fast-consumer", "fast", ["--write-percent", "50", "--consumer", "fast"]),
    ("fast-slow", "fast", ["--write-percent", "50", "--consumer", "slow"]),
    ("reliable-consumer", "reliable", ["--write-percent", "50", "--consumer", "fast"]),
    ("reliable-slow", "reliable", ["--write-percent", "50", "--consumer", "slow"]),
    ("tls", "off", ["--write-percent", "50"]),
]


def environment(args):
    """原始环境与精确构建状态，不用默认语义代替实际配置。
    Records raw environment and exact build state rather than assuming default semantics.
    """
    return {
        "date_utc": capture("date", "-u", "+%Y-%m-%dT%H:%M:%SZ"),
        "revision": capture("git", "rev-parse", "HEAD"),
        "working_tree": capture("git", "status", "--short"),
        "rustc": capture("rustc", "-Vv"), "kernel": capture("uname", "-a"),
        "cpu": capture("lscpu"), "memory": Path("/proc/meminfo").read_text(),
        "storage": capture("lsblk", "-o", "NAME,TYPE,SIZE,ROTA,MODEL"),
        "filesystem": capture("df", "-T", str(args.output)),
        "cargo_lock_sha256": hashlib.sha256((ROOT / "Cargo.lock").read_bytes()).hexdigest(),
        "source_sha256": {str(p.relative_to(ROOT)): hashlib.sha256(p.read_bytes()).hexdigest()
                          for folder in ("crates", "sdks/rust", "benchmarks")
                          for p in (ROOT / folder).rglob("*") if p.suffix in (".rs", ".py", ".toml")},
        "build_profile": "Cargo release defaults; RUSTFLAGS=" + os.environ.get("RUSTFLAGS", ""),
        "affinity_cpus": sorted(os.sched_getaffinity(0)), "binding": "none",
        "topology": "same-host loopback; client/server share CPU; closed-loop; pipeline=1",
        "redis_version": capture("redis-server", "--version"),
        "redis_comparison": "not run; installed Redis is not the required Redis 8",
        "arguments": {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
        "sample_seconds": 1, "cpu_scope": "whole process including preload, warmup and cleanup",
        "reliable": "fsync every transaction; 7-day retention; no cached-value or TTL recovery",
    }


def run_case(args, name, mode, flags, seconds, repeat):
    """每次新进程及 Journal；输出文件不经管道，采样不触发业务请求。
    Uses fresh processes and Journals; output bypasses pipes and sampling sends no requests.
    """
    directory = args.output / f"{name}-{repeat}"
    directory.mkdir()
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
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
[log]
level="error"
'''
    if mode == "reliable":
        config += '[event_journal]\ndir="journal"\nmax_bytes=67108864\n'
    command = [str(args.output / "bin/baseline"), "--address", f"127.0.0.1:{port}",
               "--keys", "1000", "--warmup", "100", "--clients", "4", *flags]
    if "--engine" not in flags:
        command += ["--seconds", str(seconds)]
    else:
        command += ["--operations", "100000"]
    if name == "tls":
        fixtures = ROOT / "tests/fixtures/tls"
        config += f'[tls]\nenabled=true\ncert_file="{fixtures / "identity.pem"}"\nkey_file="{fixtures / "identity-key.pem"}"\n'
        command += ["--tls-server-name", "localhost", "--tls-ca", str(fixtures / "ca.pem")]
    (directory / "linger.toml").write_text(config)
    server = client = None
    samples = []
    before = time.monotonic()
    with (directory / "server.log").open("w") as log, (directory / "result.json").open("w") as output, (directory / "stderr").open("w") as errors:
        try:
            if "--engine" not in flags:
                server = subprocess.Popen([str(args.output / "bin/linger-server"), "--config", str(directory / "linger.toml")], cwd=ROOT, stdout=log, stderr=log)
                deadline = time.monotonic() + 10
                while True:
                    if server.poll() is not None:
                        raise RuntimeError(f"server failed: {directory}")
                    try:
                        with socket.create_connection(("127.0.0.1", port), timeout=0.1):
                            break
                    except OSError:
                        if time.monotonic() > deadline:
                            raise TimeoutError("server startup")
                        time.sleep(0.02)
            client = subprocess.Popen(command, cwd=ROOT, stdout=output, stderr=errors)
            deadline = time.monotonic() + seconds + 120
            while client.poll() is None:
                row = {"seconds": time.monotonic() - before, "client": process_metrics(client.pid)}
                if server:
                    if server.poll() is not None:
                        raise RuntimeError(f"server exited: {directory}")
                    row["server"] = process_metrics(server.pid)
                    row["journal_bytes"] = sum(p.stat().st_size for p in (directory / "journal").glob("*") if p.is_file())
                samples.append(row)
                if time.monotonic() > deadline:
                    raise TimeoutError(f"client deadline: {directory}")
                time.sleep(1 if seconds >= 30 else 0.02)
            if server:
                samples.append({"seconds": time.monotonic() - before, "server": process_metrics(server.pid)})
        finally:
            stop(client)
            stop(server)
            (directory / "resources.json").write_text(json.dumps(samples, indent=2) + "\n")
    if client.returncode:
        raise RuntimeError(f"benchmark failed: {directory}")
    result = json.loads((directory / "result.json").read_text())
    result.update(name=name, repeat=repeat, event_mode=mode, command=command, config=config,
                  resources=samples, full_phase_seconds=time.monotonic() - before)
    print(f"{name}-{repeat}: {result['successes_per_second']:.0f} valid/s; p99 {result['p99_us']} us; errors {result['errors']}", flush=True)
    return result


def main():
    """预先固定时长与次数；支持筛选用于可比的优化前后实验。
    Fixes durations and repeats in advance and supports matched before/after experiments.
    """
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--seconds", type=int, default=3)
    parser.add_argument("--soak-seconds", type=int, default=600)
    parser.add_argument("--only", default="")
    parser.add_argument("--soak-only", action="store_true")
    args = parser.parse_args()
    if not 1 <= args.repeats <= 10 or not 1 <= args.seconds <= 60 or not 1 <= args.soak_seconds <= 3600:
        parser.error("invalid repeat or duration budget")
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=False)
    subprocess.run(["cargo", "build", "--release", "-p", "linger-server", "--bin", "linger-server", "--example", "baseline"], cwd=ROOT, check=True)
    (args.output / "bin").mkdir()
    # 固定二进制副本，实验期间重建不能改变后续场景的版本。
    # Freeze binary copies so concurrent rebuilds cannot change later scenarios' revisions.
    shutil.copy2(ROOT / "target/release/linger-server", args.output / "bin/linger-server")
    shutil.copy2(ROOT / "target/release/examples/baseline", args.output / "bin/baseline")
    (args.output / "environment.json").write_text(json.dumps(environment(args), ensure_ascii=False, indent=2) + "\n")
    selected = set(args.only.split(",")) if args.only else {name for name, _, _ in CASES}
    if selected - {name for name, _, _ in CASES}:
        parser.error("unknown case")
    results = []
    if not args.soak_only:
        for name, mode, flags in CASES:
            if name in selected:
                for repeat in range(1, args.repeats + 1):
                    results.append(run_case(args, name, mode, flags, args.seconds, repeat))
                    (args.output / "results.json").write_text(json.dumps(results, indent=2) + "\n")
    if args.soak_only or not args.only:
        for mode in ("fast", "reliable"):
            results.append(run_case(args, f"soak-{mode}", mode,
                                    ["--write-percent", "50", "--ttl-ms", "1000", "--hot", "--rate", "20"], args.soak_seconds, 1))
            (args.output / "results.json").write_text(json.dumps(results, indent=2) + "\n")


if __name__ == "__main__":
    main()
