#!/usr/bin/env python3
"""三语言共用真实 Rust 服务端契约；仅使用现有 Go、JDK、OpenSSL 工具。
Three-language contracts against a real Rust server using existing Go, JDK, and OpenSSL tools.
"""
from pathlib import Path
import os
import socket
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]


def run(args, cwd=ROOT, env=None):
    """真实命令失败立即退出，不把建议命令当作成功。 Fail on real command errors; suggestions never count as verification."""
    subprocess.run(args, cwd=cwd, env=env, check=True, timeout=180)


def free_port():
    """只为隔离测试选择临时端口。 Selects an ephemeral port solely for isolated tests."""
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def main():
    """先构建，再分别验证明文与双向 TLS；每轮使用全新 Journal。 Builds first, then tests plaintext and mutual TLS with a fresh Journal per run."""
    run(["cargo", "build", "-p", "ling-server"])
    classes = ROOT / "target/java-contracts"
    classes.mkdir(parents=True, exist_ok=True)
    sources = sorted((ROOT / "sdks/java/src").rglob("*.java"))
    run(["javac", "-Xlint:all", "-Werror", "-d", str(classes), *map(str, sources)])
    fixtures = ROOT / "tests/fixtures"
    for mutual in (False, True):
        with tempfile.TemporaryDirectory(prefix="linger-sdk-") as temporary:
            directory = Path(temporary)
            port = free_port()
            config = directory / "linger.toml"
            text = f'[server]\nport={port}\n[engine]\nshard_count=2\n[events]\nmode="reliable"\nretry_interval_ms=10\n[event_journal]\ndir="journal"\n[log]\nlevel="error"\n'
            env = {k: v for k, v in os.environ.items() if not k.startswith("LINGER_TEST_")}
            env["LINGER_TEST_ADDR"] = f"127.0.0.1:{port}"
            if mutual:
                ca, cert, key = (fixtures / "tls" / name for name in ("ca.pem", "identity.pem", "identity-key.pem"))
                identity = directory / "identity.p12"
                run(["openssl", "pkcs12", "-export", "-in", str(cert), "-inkey", str(key), "-certfile", str(ca), "-out", str(identity), "-passout", "pass:test-only"])
                text += f'[tls]\nenabled=true\ncert_file="{cert}"\nkey_file="{key}"\nca_file="{ca}"\n'
                env.update(LINGER_TEST_CA=str(ca), LINGER_TEST_CERT=str(cert), LINGER_TEST_KEY=str(key), LINGER_TEST_PKCS12=str(identity))
            config.write_text(text)
            with (directory / "server.log").open("w+") as log:
                server = subprocess.Popen([str(ROOT / "target/debug/ling"), "--config", str(config)], cwd=ROOT, stdout=log, stderr=log)
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
                    print("SDK contracts:", "mutual TLS" if mutual else "TCP", flush=True)
                    run(["go", "test", "-race", "-count=1", "./..."], cwd=ROOT / "sdks/go", env=env)
                    run(["java", "-ea", "-cp", str(classes), "io.linger.cache.Contracts", str(fixtures / "protocol")], env=env)
                finally:
                    server.terminate()
                    try:
                        server.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        server.kill()
                        server.wait()


if __name__ == "__main__":
    main()
