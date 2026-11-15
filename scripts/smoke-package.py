#!/usr/bin/env python3
"""验证打包后的真实 TCP 调用；Smoke-test packaged binaries over real TCP."""

import argparse
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time


def main():
    """独立端口与临时配置，不依赖本机服务；Use an isolated port and config."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server", type=Path)
    parser.add_argument("--cli", type=Path)
    parser.add_argument("--appimage", type=Path)
    args = parser.parse_args()
    if args.appimage:
        server = [str(args.appimage.resolve()), "--appimage-extract-and-run"]
        cli = [*server, "--cli"]
    elif args.server and args.cli:
        server = [str(args.server.resolve())]
        cli = [str(args.cli.resolve())]
    else:
        parser.error("provide --server and --cli, or --appimage")
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    env = os.environ.copy()
    env.pop("LINGER_PASSWORD", None)
    with tempfile.TemporaryDirectory(prefix="linger-package-") as directory:
        root = Path(directory)
        config = root / "linger.toml"
        config.write_text(
            f'[server]\nhost="127.0.0.1"\nport={port}\n'
            '[engine]\nshard_count=2\n', encoding="utf-8"
        )
        with (root / "server.log").open("w+") as log:
            process = subprocess.Popen(
                [*server, "--config", str(config)], cwd=root, env=env,
                stdout=log, stderr=subprocess.STDOUT,
            )
            try:
                deadline = time.monotonic() + 30
                while True:
                    if process.poll() is not None:
                        raise RuntimeError("server exited before becoming ready")
                    try:
                        with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                            break
                    except OSError:
                        if time.monotonic() >= deadline:
                            raise TimeoutError("server did not become ready")
                        time.sleep(0.1)

                def command(source):
                    """非零退出即失败，保留 stderr；Fail on nonzero status with stderr."""
                    result = subprocess.run(
                        [*cli, "--port", str(port), "-c", source], cwd=root,
                        env=env, check=True, capture_output=True, text=True, timeout=30,
                    )
                    return result.stdout.strip()

                assert command("PING") == '"PONG"'
                command("SET package-smoke int(42) PX 60000")
                assert command("GET package-smoke") == "int(42)"
                ttl = command("PTTL package-smoke")
                assert ttl.startswith("int(") and 0 < int(ttl[4:-1]) <= 60000, ttl
                # SHUTDOWN 关闭当前连接时可能结果未知；必须实际观察服务退出。
                # SHUTDOWN may disconnect before replying; observe the actual process exit.
                shutdown = subprocess.run(
                    [*cli, "--port", str(port), "-c", "SHUTDOWN"], cwd=root,
                    env=env, capture_output=True, text=True, timeout=30,
                )
                if shutdown.returncode:
                    assert "outcome unknown" in shutdown.stderr, shutdown.stderr
                assert process.wait(timeout=15) == 0
                print("Packaged TCP PING / SET / GET / PTTL / SHUTDOWN passed")
            except BaseException:
                log.flush()
                log.seek(0)
                print(log.read())
                raise
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=15)


if __name__ == "__main__":
    main()
