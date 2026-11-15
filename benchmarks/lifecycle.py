#!/usr/bin/env python3
"""运行独立 TTL 可见性与回收实验，保留每次观察而不宣称调度器精度。
Runs isolated TTL visibility/reclamation experiments retaining observations, not claiming scheduler precision.
"""
import argparse
import json
from pathlib import Path
import socket
import shutil
import subprocess
import sys
import time
sys.dont_write_bytecode = True
from run import ROOT, stop
from stage7 import environment


def main():
    """Fast / Reliable 各运行三十次，使用独立服务进程与日志目录。
    Runs thirty probes in each mode with separate service processes and Journal directories.
    """
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=False)
    subprocess.run(["cargo", "build", "--release", "-p", "linger-server", "--bin", "linger-server", "--example", "lifecycle"], cwd=ROOT, check=True)
    # 与矩阵工具一样固定二进制和来源，隔离实验期间的后续编译。
    # Freeze binaries and provenance like the matrix runner, isolating subsequent builds.
    (args.output / "bin").mkdir()
    shutil.copy2(ROOT / "target/release/linger-server", args.output / "bin/linger-server")
    shutil.copy2(ROOT / "target/release/examples/lifecycle", args.output / "bin/lifecycle")
    (args.output / "environment.json").write_text(json.dumps(environment(args), ensure_ascii=False, indent=2) + "\n")
    results = {}
    for mode in ("fast", "reliable"):
        directory = args.output / mode
        directory.mkdir()
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
        config = f'[server]\nport={port}\n[engine]\nshard_count=2\n[events]\nmode="{mode}"\n[log]\nlevel="error"\n'
        if mode == "reliable":
            config += '[event_journal]\ndir="journal"\n'
        (directory / "linger.toml").write_text(config)
        with (directory / "server.log").open("w") as log:
            server = subprocess.Popen([str(args.output / "bin/linger-server"), "--config", str(directory / "linger.toml")], cwd=ROOT, stdout=log, stderr=log)
            try:
                deadline = time.monotonic() + 10
                while True:
                    if server.poll() is not None:
                        raise RuntimeError(f"server failed: {directory}")
                    try:
                        with socket.create_connection(("127.0.0.1", port), timeout=0.1):
                            break
                    except OSError:
                        if time.monotonic() > deadline: raise TimeoutError("startup")
                        time.sleep(0.02)
                command = [str(args.output / "bin/lifecycle"), f"127.0.0.1:{port}"]
                result = subprocess.run(command, cwd=ROOT, capture_output=True, text=True, timeout=60)
                (directory / "stderr").write_text(result.stderr)
                result.check_returncode()
                results[mode] = json.loads(result.stdout)
                results[mode].update(config=config, command=command)
            finally:
                stop(server)
    (args.output / "results.json").write_text(json.dumps(results, indent=2) + "\n")
    print("TTL probes passed:", ", ".join(f"{mode}={len(value['samples'])}" for mode,value in results.items()))


if __name__ == "__main__":
    main()
