#!/usr/bin/env python3
"""保存独立引擎与真实 TCP 基线的环境、原始输出和进程指标。
Record environment, raw output, and process metrics for engine and real TCP baselines.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
try:
    import resource
except ImportError:
    # 测量仍要求 Linux；允许在其他平台只运行结果验收测试。
    # Measurements remain Linux-only; allow result-validation tests on other platforms.
    resource = None
import socket
import subprocess
import time


ROOT = Path(__file__).resolve().parents[1]


def capture(*command):
    """只读采集环境；失败保留错误而不伪造结果。
    Capture environment read-only; retain failures instead of fabricating results.
    """
    result = subprocess.run(command, cwd=ROOT, text=True, capture_output=True)
    return result.stdout.strip() if result.returncode == 0 else result.stderr.strip()


def process_metrics(pid):
    """Linux /proc 指标：RSS 与 CPU ticks，进程退出后停止采样。
    Read Linux /proc RSS and CPU ticks, stopping when the process exits.
    """
    try:
        status = Path(f"/proc/{pid}/status").read_text()
        stat = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    except (FileNotFoundError, ProcessLookupError):
        return {"rss_bytes": 0, "peak_rss_bytes": 0, "cpu_seconds": 0}
    fields = dict(line.split(":", 1) for line in status.splitlines() if ":" in line)
    return {
        "rss_bytes": int(fields.get("VmRSS", "0 kB").split()[0]) * 1024,
        "peak_rss_bytes": int(fields.get("VmHWM", "0 kB").split()[0]) * 1024,
        "cpu_seconds": (int(stat[11]) + int(stat[12])) / os.sysconf("SC_CLK_TCK"),
    }


def stop(process):
    """只终止本脚本启动的进程，并回收退出状态。
    Stop only a process started by this script and reap its exit status.
    """
    if process is not None and process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


def run_case(args, mode, write_percent, repeat):
    """每次 TCP 实验新建服务端，避免跨次缓存和分配器状态混入。
    Start a fresh server for each TCP experiment to avoid cross-run cache and allocator state.
    """
    name = f"{mode}-write{write_percent}-{repeat}"
    server = None
    address = "127.0.0.1:6380"
    log = (args.output / f"{name}.server.log").open("w")
    before = {"cpu_seconds": 0, "rss_bytes": 0}
    try:
        if mode == "tcp":
            with socket.socket() as probe:
                probe.bind(("127.0.0.1", 0))
                port = probe.getsockname()[1]
            address = f"127.0.0.1:{port}"
            config = args.output / f"{name}.toml"
            config.write_text(f'''[server]
host = "127.0.0.1"
port = {port}
max_connections = 32
max_request_bytes = 1048576
max_response_bytes = 1048576

[database]
databases = 16

[engine]
shard_count = 4
max_memory_bytes = 268435456

[events]
enabled = false

[eviction]
policy = "noeviction"
sample_size = 5
''')
            server = subprocess.Popen(
                [ROOT / "target/release/ling", "--config", config],
                cwd=ROOT, stdout=log, stderr=log,
            )
            deadline = time.monotonic() + 10
            while True:
                if server.poll() is not None:
                    raise RuntimeError(f"{name}: server exited; see {log.name}")
                try:
                    with socket.create_connection(("127.0.0.1", port), timeout=0.1):
                        break
                except OSError:
                    if time.monotonic() > deadline:
                        raise RuntimeError(f"{name}: server startup timed out")
                    time.sleep(0.02)
            before = process_metrics(server.pid)
        command = [str(ROOT / "target/release/examples/baseline"), "--address", address,
                   "--write-percent", str(write_percent), "--operations", str(args.operations)]
        if mode == "engine":
            command.append("--engine")
        cpu_before = resource.getrusage(resource.RUSAGE_CHILDREN)
        started = time.monotonic()
        client_peak = 0
        server_peak = 0
        # 输出写入文件，避免大直方图堵塞 PIPE 而改变实验负载。
        # Write output to files so a large histogram cannot block a PIPE and alter the load.
        with (args.output / f"{name}.json").open("w") as output, \
                (args.output / f"{name}.stderr").open("w") as errors:
            client = subprocess.Popen(command, cwd=ROOT, stdout=output, stderr=errors)
            try:
                deadline = started + 300
                while client.poll() is None:
                    client_peak = max(client_peak, process_metrics(client.pid)["peak_rss_bytes"])
                    if server:
                        server_peak = max(server_peak, process_metrics(server.pid)["peak_rss_bytes"])
                    if time.monotonic() > deadline:
                        raise RuntimeError(f"{name}: benchmark exceeded five minutes")
                    time.sleep(0.02)
            finally:
                stop(client)
        if client.returncode != 0:
            raise RuntimeError(f"{name}: benchmark failed; inspect raw output")
        wall = time.monotonic() - started
        cpu_after = resource.getrusage(resource.RUSAGE_CHILDREN)
        result = json.loads((args.output / f"{name}.json").read_text())
        after = process_metrics(server.pid) if server else None
        metrics = {
            "name": name, "command": command, "full_client_phase_seconds": wall,
            "client_peak_rss_bytes_sampled": client_peak,
            "client_cpu_seconds": cpu_after.ru_utime + cpu_after.ru_stime - cpu_before.ru_utime - cpu_before.ru_stime,
            "server_initial_rss_bytes": before["rss_bytes"] if server else None,
            "server_final_rss_bytes": after["rss_bytes"] if server else None,
            "server_peak_rss_bytes": max(server_peak, after["peak_rss_bytes"]) if server else None,
            "server_cpu_seconds": after["cpu_seconds"] - before["cpu_seconds"] if server else None,
            "server_config": config.read_text() if server else None,
            "result": result,
        }
        print(f"{name}: {result['successes_per_second']:.0f} ops/s, p99 {result['p99_us']:.1f} us", flush=True)
        return metrics
    finally:
        stop(server)
        log.close()


def main():
    """输出目录必须新建；构建 release 后保留三类负载的各次原始结果。
    Require a new output directory and retain each raw result after a release build.
    """
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--operations", type=int, default=10000)
    args = parser.parse_args()
    if args.repeats <= 0 or args.operations <= 0:
        parser.error("repeats and operations must be positive")
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=False)
    subprocess.run(["cargo", "build", "--release", "-p", "ling-server",
                    "--bin", "ling", "--example", "baseline"], cwd=ROOT, check=True)
    environment = {
        "date_utc": capture("date", "-u", "+%Y-%m-%dT%H:%M:%SZ"),
        "revision": capture("git", "rev-parse", "HEAD"),
        "working_tree": capture("git", "status", "--short"),
        "rustc": capture("rustc", "-Vv"), "kernel": capture("uname", "-a"),
        "cargo_lock_sha256": hashlib.sha256((ROOT / "Cargo.lock").read_bytes()).hexdigest(),
        "build_profile": "cargo release defaults; opt-level=3; no custom RUSTFLAGS",
        "rustflags": os.environ.get("RUSTFLAGS", ""),
        "cpu": capture("lscpu"), "memory": Path("/proc/meminfo").read_text(),
        "storage": capture("lsblk", "-o", "NAME,TYPE,SIZE,ROTA,MODEL"),
        "affinity_cpus": sorted(os.sched_getaffinity(0)), "cpu_binding": "none",
        "topology": "same-host loopback; client and server share CPUs; no TLS, auth, TTL, events or journal",
        "sampling_interval_seconds": 0.02, "repeats": args.repeats,
        "cpu_metric_scope": "whole client process, including setup, warmup and output",
    }
    (args.output / "environment.json").write_text(json.dumps(environment, ensure_ascii=False, indent=2) + "\n")
    results = []
    for mode in ["engine", "tcp"]:
        for write_percent in [0, 100, 50]:
            for repeat in range(1, args.repeats + 1):
                results.append(run_case(args, mode, write_percent, repeat))
                (args.output / "results.json").write_text(json.dumps(results, indent=2) + "\n")


if __name__ == "__main__":
    main()
