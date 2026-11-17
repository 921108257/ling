#!/usr/bin/env python3
"""0.2 订阅唤醒对照：保存独立进程、原始时延及消费结果。
Compare subscription wakeups with isolated processes, raw latency and consumer outcomes.
"""
import argparse
from collections import deque
import ctypes
from datetime import datetime, timezone
import gzip
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import random
import select
import shutil
import socket
import struct
import subprocess
import sys
import threading
import time

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
HEADER = struct.Struct(">2sBBHQI")
MAX_PAYLOAD = 1024 * 1024


class WireError(RuntimeError):
    """稳定错误码与诊断保留；Retain stable error codes and diagnostics."""


def encode(value):
    """仅编码本实验需要的严格类型；Encode only the experiment's strict types."""
    if value is None:
        return b"\x00"
    if isinstance(value, bool):
        return b"\x01" + bytes([value])
    if isinstance(value, int):
        return b"\x03" + struct.pack(">Q", value)
    if isinstance(value, (str, bytes)):
        data = value.encode("utf-8") if isinstance(value, str) else value
        return bytes([5 if isinstance(value, str) else 6]) + struct.pack(">I", len(data)) + data
    if isinstance(value, (list, tuple)):
        return b"\x07" + struct.pack(">I", len(value)) + b"".join(map(encode, value))
    if isinstance(value, dict):
        return b"\x08" + struct.pack(">I", len(value)) + b"".join(
            encode(key) + encode(item) for key, item in value.items())
    raise WireError("LOCAL_ENCODING: unsupported benchmark value")


def decode(data, offset=0, depth=0):
    """长度先验，UInt 保持完整精度；Bound lengths and preserve full UInt precision."""
    if depth > 16 or offset >= len(data):
        raise WireError("LOCAL_PROTOCOL: truncated or nested Value")
    tag, offset = data[offset], offset + 1
    if tag == 0:
        return None, offset
    if tag == 1:
        if offset >= len(data) or data[offset] > 1:
            raise WireError("LOCAL_PROTOCOL: invalid Bool")
        return bool(data[offset]), offset + 1
    if tag in (2, 3, 4):
        if offset + 8 > len(data):
            raise WireError("LOCAL_PROTOCOL: truncated number")
        return struct.unpack_from({2: ">q", 3: ">Q", 4: ">d"}[tag], data, offset)[0], offset + 8
    if tag not in (5, 6, 7, 8, 9) or offset + 4 > len(data):
        raise WireError("LOCAL_PROTOCOL: invalid Value")
    count = struct.unpack_from(">I", data, offset)[0]
    offset += 4
    if count > len(data) - offset:
        raise WireError("LOCAL_PROTOCOL: impossible Value length")
    if tag in (5, 6):
        value = data[offset:offset + count]
        return (value.decode("utf-8") if tag == 5 else value), offset + count
    if tag == 8:
        result = {}
        for _ in range(count):
            key, offset = decode(data, offset, depth + 1)
            if not isinstance(key, str) or not key or key in result:
                raise WireError("LOCAL_PROTOCOL: invalid Map key")
            result[key], offset = decode(data, offset, depth + 1)
        return result, offset
    result = []
    for _ in range(count):
        value, offset = decode(data, offset, depth + 1)
        result.append(value)
    return result, offset


def packet(kind, request_id, payload):
    """固定 v1 帧且限制负载；Construct bounded protocol-v1 frames."""
    if len(payload) > MAX_PAYLOAD:
        raise WireError("LOCAL_LIMIT: benchmark frame too large")
    return HEADER.pack(b"LG", 1, kind, 0, request_id, len(payload)) + payload


def exact(stream, size):
    """处理半包，EOF 不是成功；Handle fragmented reads without treating EOF as success."""
    result = bytearray()
    while len(result) < size:
        part = stream.recv(size - len(result))
        if not part:
            raise WireError("LOCAL_EOF: connection closed before complete frame")
        result.extend(part)
    return bytes(result)


def receive(stream):
    """解析响应、事件和心跳，拒绝未知封套；Validate response, event and heartbeat envelopes."""
    magic, version, kind, flags, request_id, size = HEADER.unpack(exact(stream, HEADER.size))
    if magic != b"LG" or version != 1 or flags or kind not in (2, 3, 5, 6) or size > MAX_PAYLOAD:
        raise WireError("LOCAL_PROTOCOL: invalid frame header")
    payload = exact(stream, size)
    if kind == 5:
        if request_id or payload:
            raise WireError("LOCAL_PROTOCOL: invalid heartbeat")
        return kind, request_id, None
    if kind == 6 or (kind == 2 and payload[:1] == b"\x01"):
        start = int(kind == 2)
        code, offset = decode(payload, start)
        message, offset = decode(payload, offset)
        if offset != size or not isinstance(code, str) or not isinstance(message, str):
            raise WireError("LOCAL_PROTOCOL: invalid error")
        raise WireError(f"{code}: {message}")
    start = int(kind == 2)
    if kind == 2 and payload[:1] != b"\x00":
        raise WireError("LOCAL_PROTOCOL: invalid response status")
    value, offset = decode(payload, start)
    if offset != size or (kind == 3 and request_id) or (kind == 2 and not request_id):
        raise WireError("LOCAL_PROTOCOL: invalid response envelope")
    return kind, request_id, value


class Client:
    """串行控制请求及有界事件缓存；Serial control calls with a bounded event inbox."""
    def __init__(self, port):
        self.stream = socket.create_connection(("127.0.0.1", port), timeout=10)
        self.stream.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.request_id = 0
        self.events = deque()
        self.subscribed = False
        try:
            self.hello = self.request("HELLO", [1])
        except BaseException:
            self.close()
            raise

    def request(self, command, arguments=(), kind=1):
        """保留服务器错误且绝不重放；Preserve server errors and never replay requests."""
        self.request_id += 1
        body = encode(arguments) if kind == 4 else encode(command) + encode(list(arguments))
        self.stream.sendall(packet(kind, self.request_id, body))
        while True:
            frame_kind, request_id, value = receive(self.stream)
            if frame_kind == 5:
                continue
            if frame_kind == 3 and self.subscribed:
                if len(self.events) >= 256:
                    raise WireError("LOCAL_LIMIT: event inbox exceeded 256")
                self.events.append(value)
                continue
            if frame_kind != 2 or request_id != self.request_id:
                raise WireError("LOCAL_PROTOCOL: response ID or state mismatch")
            return value

    def subscribe(self, pattern, name):
        """命名组用于同时覆盖 Reliable 持久确认；Named groups include Reliable durable ACK costs."""
        self.request("SUBSCRIBE", [[pattern], "GROUP", name])
        self.subscribed = True

    def event(self):
        """返回完整事件，控制帧不冒充事件；Return full events without disguising control frames."""
        if self.events:
            return self.events.popleft()
        while True:
            kind, _, value = receive(self.stream)
            if kind == 3:
                return value
            if kind != 5:
                raise WireError("LOCAL_PROTOCOL: unexpected idle response")

    def close(self):
        """仅关闭本客户端拥有的套接字；Close only the socket owned by this client."""
        self.stream.close()


def write_json(path, value):
    """原始结果采用 UTF-8 和稳定缩进；Write raw results as readable UTF-8 JSON."""
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def capture(root, *command):
    """失败环境探针保留原因；Retain unsuccessful environment probes."""
    try:
        result = subprocess.run(command, cwd=root, capture_output=True, text=True,
                                encoding="utf-8", errors="replace", timeout=30)
        return {"command": list(command), "returncode": result.returncode,
                "stdout": result.stdout.strip(), "stderr": result.stderr.strip()}
    except (OSError, subprocess.TimeoutExpired) as error:
        return {"command": list(command), "error": str(error)}


def process_snapshot(pid):
    """Linux /proc 与 Windows 原生进程指标；Read native process CPU and RSS on both platforms."""
    try:
        if sys.platform.startswith("linux"):
            root = Path(f"/proc/{pid}")
            stat = (root / "stat").read_text().rsplit(")", 1)[1].split()
            fields = dict(line.split(":", 1) for line in (root / "status").read_text().splitlines() if ":" in line)
            return {"cpu_seconds": (int(stat[11]) + int(stat[12])) / os.sysconf("SC_CLK_TCK"),
                    "rss_bytes": int(fields["VmRSS"].split()[0]) * 1024,
                    "threads": int(fields["Threads"]), "handles": len(list((root / "fd").iterdir()))}
        if sys.platform == "win32":
            from ctypes import wintypes
            kernel = ctypes.WinDLL("kernel32", use_last_error=True)
            kernel.OpenProcess.restype = wintypes.HANDLE
            kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
            kernel.GetProcessTimes.argtypes = [wintypes.HANDLE] + [ctypes.POINTER(wintypes.FILETIME)] * 4
            kernel.CloseHandle.argtypes = [wintypes.HANDLE]
            handle = kernel.OpenProcess(0x0410, False, pid)
            if not handle:
                raise ctypes.WinError(ctypes.get_last_error())
            try:
                times = [wintypes.FILETIME() for _ in range(4)]
                if not kernel.GetProcessTimes(handle, *(ctypes.byref(value) for value in times)):
                    raise ctypes.WinError(ctypes.get_last_error())
                cpu = sum((value.dwHighDateTime << 32) | value.dwLowDateTime for value in times[2:]) / 10_000_000

                class Memory(ctypes.Structure):
                    _fields_ = [("cb", wintypes.DWORD), ("faults", wintypes.DWORD)] + [
                        (name, ctypes.c_size_t) for name in ("peak", "rss", "paged_peak", "paged", "nonpaged_peak", "nonpaged", "pagefile", "pagefile_peak", "private")]
                memory = Memory()
                memory.cb = ctypes.sizeof(memory)
                psapi = ctypes.WinDLL("psapi", use_last_error=True)
                psapi.GetProcessMemoryInfo.argtypes = [wintypes.HANDLE, ctypes.POINTER(Memory), wintypes.DWORD]
                rss = memory.rss if psapi.GetProcessMemoryInfo(handle, ctypes.byref(memory), memory.cb) else None
                handles = wintypes.DWORD()
                kernel.GetProcessHandleCount.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD)]
                count = handles.value if kernel.GetProcessHandleCount(handle, ctypes.byref(handles)) else None
                return {"cpu_seconds": cpu, "rss_bytes": rss, "handles": count, "threads": None,
                        "threads_unavailable": "not sampled by the Windows process API"}
            finally:
                kernel.CloseHandle(handle)
        return {"unavailable": "native process sampling supports Linux and Windows"}
    except (OSError, KeyError, ValueError) as error:
        return {"unavailable": str(error)}


def idle_watch(clients, stop, result):
    """一条线程监视空闲订阅，关闭或错误不能藏在 PING 成功里。
    One thread monitors idle subscribers; closure and errors cannot hide behind successful PINGs.
    """
    streams = [client.stream for client in clients]
    result.update(errors={}, unexpected_events=0, termination="running")
    try:
        while not stop.is_set():
            ready, _, _ = select.select(streams, [], [], 0.05)
            for stream in ready:
                kind, _, _ = receive(stream)
                if kind != 5:
                    result["unexpected_events"] += 1
                    raise WireError("LOCAL_PROTOCOL: unexpected data on idle subscription")
        result["termination"] = "requested_stop"
    except Exception as error:
        result["errors"][str(error)] = 1
        result["termination"] = "error"


def quantiles(samples):
    """最近秩分位数，不合并独立重复；Nearest-rank quantiles, never pooled across repeats."""
    ordered = sorted(samples)
    return {name: ordered[max(0, math.ceil(len(ordered) * quantile) - 1)] / 1000 if ordered else None
            for name, quantile in (("p50_us", .5), ("p99_us", .99), ("p999_us", .999))}


def acceptance(row):
    """消费状态、错误和负载完成度独立判定；Validate consumers independently from producer success."""
    failures = []
    if row.get("errors"):
        failures.append("request_or_setup_error")
    consumer = row.get("consumer", {})
    if consumer.get("errors") or consumer.get("termination") not in ("requested_stop", "backlog_drained"):
        failures.append("consumer_failed_or_missing")
    if row["scenario"].startswith("idle"):
        if not row.get("successes") or consumer.get("unexpected_events"):
            failures.append("ping_or_idle_subscription_failed")
        if row.get("elapsed_seconds", 0) < row["requested_seconds"] * .95:
            failures.append("measurement_ended_early")
        if row["notification_mode"] == "on" and row.get("subscription_poll_delta") != 0:
            failures.append("notification_mode_dispatched_idle_polls")
    elif not consumer.get("acks") or consumer.get("acks") != consumer.get("received") or consumer.get("duplicates"):
        failures.append("missing_or_unconfirmed_delivery")
    return {"valid": not failures, "failures": failures}


def run_case(args, binary, mode, scenario, repeat):
    """每次全新服务端和 Journal，失败也保留结果；Fresh server and Journal; retain failed runs too."""
    directory = args.output / f"{mode}-{scenario}-{repeat}"
    directory.mkdir()
    row = {"event_mode": mode, "scenario": scenario, "repeat": repeat,
           "notification_mode": args.notification_mode, "requested_seconds": args.seconds,
           "errors": {}, "successes": 0, "consumer": {"termination": "not_started"}}
    clients, server, watcher = [], None, None
    stop = threading.Event()
    latencies = []
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
    config = f'''[server]
port={port}
max_connections=128
idle_timeout_ms=600000
request_timeout_ms=30000
max_request_bytes=1048576
max_response_bytes=1048576
[engine]
shard_count=4
max_memory_bytes=268435456
[events]
mode="{mode}"
buffer_bytes=67108864
max_groups=128
max_pending_events=128
retry_interval_ms=600000
consumer_timeout_ms=600000
'''
    if args.notification_mode != "legacy":
        config += f"notification_enabled={str(args.notification_mode == 'on').lower()}\n"
    config += '[metrics]\nenabled=true\nport=' + str(free_port()) + '\n[log]\nlevel="error"\n'
    if mode == "reliable":
        config += '[event_journal]\ndir="journal"\nmax_bytes=268435456\n'
    (directory / "ling.toml").write_text(config, encoding="utf-8")
    with (directory / "server.log").open("w", encoding="utf-8") as log:
        try:
            server = subprocess.Popen([str(binary), "--config", str(directory / "ling.toml")],
                                      cwd=directory, stdout=log, stderr=log)
            if args.server_cpus is not None:
                os.sched_setaffinity(server.pid, args.server_cpus)
            deadline = time.monotonic() + 15
            while True:
                try:
                    observer = Client(port)
                    clients.append(observer)
                    break
                except OSError:
                    if server.poll() is not None or time.monotonic() > deadline:
                        raise WireError("LOCAL_STARTUP: server did not become ready")
                    time.sleep(.02)
            row["server_version"] = observer.hello["server_version"]
            if scenario.startswith("idle"):
                count = int(scenario.split("-")[1])
                subscribers = []
                for number in range(count):
                    client = Client(port)
                    clients.append(client)
                    client.subscribe("idle:*", f"idle-{number}")
                    subscribers.append(client)
                row["subscribers"] = count
                row["consumer"] = {"termination": "requested_stop", "errors": {}, "unexpected_events": 0}
                if subscribers:
                    watcher = threading.Thread(target=idle_watch, args=(subscribers, stop, row["consumer"]))
                    watcher.start()
                for _ in range(args.warmup):
                    if observer.request("PING") != "PONG":
                        raise WireError("LOCAL_RESULT: unexpected PING result")
                before = observer.request("INFO", ["all"])
                row["resources_before"] = process_snapshot(server.pid)
                cpu_before, started = time.process_time(), time.perf_counter()
                while time.perf_counter() - started < args.seconds:
                    tick = time.perf_counter_ns()
                    if observer.request("PING") != "PONG":
                        raise WireError("LOCAL_RESULT: unexpected PING result")
                    latencies.append(time.perf_counter_ns() - tick)
                    row["successes"] += 1
                    if len(latencies) >= 2_000_000:
                        raise WireError("LOCAL_LIMIT: raw latency sample budget exhausted")
                row["elapsed_seconds"] = time.perf_counter() - started
                row["client_cpu_seconds"] = time.process_time() - cpu_before
                row["resources_after"] = process_snapshot(server.pid)
                after = observer.request("INFO", ["all"])
                row["subscription_poll_delta"] = after["events"]["subscription_polls"] - before["events"]["subscription_polls"]
            else:
                subscriber = Client(port)
                clients.append(subscriber)
                subscriber.subscribe("event:*", "backlog")
                for start in range(0, args.backlog_events, 128):
                    values = []
                    for number in range(start, min(start + 128, args.backlog_events)):
                        values.extend([f"event:{number:08x}", b"12345678"])
                    if observer.request("MSET", values) is not True:
                        raise WireError("LOCAL_RESULT: preload MSET was not confirmed")
                row["backlog_events"] = args.backlog_events
                row["preload_complete"] = True
                before = observer.request("INFO", ["all"])
                row["resources_before"] = process_snapshot(server.pid)
                consumer = row["consumer"] = {"received": 0, "acks": 0, "duplicates": 0,
                                               "errors": {}, "termination": "running"}
                seen = set()
                cpu_before, started = time.process_time(), time.perf_counter()
                while time.perf_counter() - started < args.seconds and len(seen) < args.backlog_events:
                    event = subscriber.event()
                    identity = event["event_id"]
                    stable = (identity["stream_id"], identity["db"], identity["shard"], identity["sequence"])
                    consumer["received"] += 1
                    consumer["duplicates"] += int(stable in seen)
                    seen.add(stable)
                    if subscriber.request(None, identity, kind=4) is not True:
                        raise WireError("LOCAL_RESULT: ACK was not confirmed")
                    consumer["acks"] += 1
                row["elapsed_seconds"] = time.perf_counter() - started
                row["client_cpu_seconds"] = time.process_time() - cpu_before
                consumer["termination"] = "backlog_drained" if len(seen) == args.backlog_events else "requested_stop"
                consumer["unique"] = len(seen)
                row["successes"] = consumer["acks"]
                row["resources_after"] = process_snapshot(server.pid)
                after = observer.request("INFO", ["all"])
            row["info_before"], row["info_after"] = before, after
        except Exception as error:
            row["errors"][str(error)] = 1
            if row["consumer"].get("termination") == "running":
                row["consumer"]["termination"] = "error"
                row["consumer"]["errors"][str(error)] = 1
        finally:
            stop.set()
            if watcher:
                watcher.join(timeout=12)
                if watcher.is_alive():
                    row["errors"]["LOCAL_TIMEOUT: idle watcher did not stop"] = 1
            for client in clients:
                client.close()
            if server is not None:
                if server.poll() is not None:
                    row["errors"][f"LOCAL_EXIT: server exited {server.returncode}"] = 1
                else:
                    server.terminate()
                    try:
                        server.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        server.kill()
                        server.wait(timeout=10)
                row["server_exit_after_cleanup"] = server.returncode
    row.update(quantiles(latencies))
    row["successes_per_second"] = row["successes"] / row["elapsed_seconds"] if row.get("elapsed_seconds") else None
    if "resources_after" in row:
        first, last = row["resources_before"].get("cpu_seconds"), row["resources_after"].get("cpu_seconds")
        row["server_cpu_seconds"] = last - first if first is not None and last is not None else None
    row["acceptance"] = acceptance(row)
    if latencies:
        with gzip.open(directory / "latencies-ns.json.gz", "wt", encoding="utf-8") as stream:
            json.dump(latencies, stream)
    write_json(directory / "result.json", row)
    print(f"{directory.name}: valid={row['acceptance']['valid']}; rate={row['successes_per_second']}", flush=True)
    return row


def free_port():
    """系统分配临时回环端口；Ask the OS for an ephemeral loopback port."""
    with socket.socket() as stream:
        stream.bind(("127.0.0.1", 0))
        return stream.getsockname()[1]


def main():
    """固定来源和二进制后执行矩阵，不覆盖任何旧输出。
    Freeze provenance and binaries before the matrix, without overwriting old output.
    """
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--source-root", type=Path, default=ROOT)
    parser.add_argument("--server-binary", type=Path)
    parser.add_argument("--notification-mode", choices=("legacy", "off", "on"), default="legacy")
    parser.add_argument("--event-mode", choices=("both", "fast", "reliable"), default="both")
    parser.add_argument("--only", choices=("idle-0", "idle-1", "idle-100", "backlog"))
    parser.add_argument("--seconds", type=float, default=3)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--warmup", type=int, default=50)
    parser.add_argument("--backlog-events", type=int, default=20000)
    parser.add_argument("--seed", type=int, default=20261118)
    parser.add_argument("--allow-dirty", action="store_true")
    args = parser.parse_args()
    if not 0 < args.seconds <= 60 or not 1 <= args.repeats <= 10 or not 0 <= args.warmup <= 10000 or not 128 <= args.backlog_events <= 40000:
        parser.error("invalid duration, repeats, warmup or backlog budget")
    args.source_root, args.output = args.source_root.resolve(), args.output.resolve()
    status = capture(args.source_root, "git", "status", "--short")
    if status.get("returncode") != 0 or (status["stdout"] and not args.allow_dirty):
        parser.error("source must be a Git checkout and clean, or explicitly use --allow-dirty")
    args.output.mkdir(parents=True, exist_ok=False)
    shutil.copy2(Path(__file__), args.output / "v020.py")
    write_json(args.output / "plan.json", {**vars(args), "source_root": str(args.source_root),
                                           "output": str(args.output), "server_binary": str(args.server_binary) if args.server_binary else None})
    if args.server_binary is None:
        with (args.output / "build.log").open("w", encoding="utf-8") as output:
            subprocess.run(["cargo", "build", "--release", "--locked", "-p", "ling-server", "--bin", "ling"],
                           cwd=args.source_root, stdout=output, stderr=output, check=True)
        args.server_binary = args.source_root / "target/release" / ("ling.exe" if os.name == "nt" else "ling")
    (args.output / "bin").mkdir()
    binary = args.output / "bin" / args.server_binary.name
    shutil.copy2(args.server_binary.resolve(), binary)
    args.server_cpus = None
    binding = {"status": "not bound", "reason": "Windows: client and server share OS scheduling"}
    if sys.platform.startswith("linux"):
        available = sorted(os.sched_getaffinity(0))
        if len(available) >= 2:
            half = max(1, len(available) // 2)
            args.server_cpus, client_cpus = set(available[:half]), set(available[half:])
            os.sched_setaffinity(0, client_cpus)
            binding = {"server_cpus": sorted(args.server_cpus), "client_cpus": sorted(client_cpus)}
        else:
            binding = {"status": "not bound", "reason": "fewer than two available CPUs"}
    metadata = {"date_utc": datetime.now(timezone.utc).isoformat(),
                "revision": capture(args.source_root, "git", "rev-parse", "HEAD"),
                "working_tree": status, "platform": platform.platform(), "python": sys.version,
                "logical_cpus": os.cpu_count(), "binding": binding,
                "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                "binary_origin": str(args.server_binary.resolve()),
                "binary_version": capture(args.source_root, str(binary), "--version"),
                "script_sha256": hashlib.sha256((args.output / "v020.py").read_bytes()).hexdigest(),
                "source_sha256": {str(path.relative_to(args.source_root)): hashlib.sha256(path.read_bytes()).hexdigest()
                                  for folder in ("crates", "sdks/rust") for path in (args.source_root / folder).rglob("*")
                                  if path.suffix in (".rs", ".toml")},
                "cargo_lock_sha256": hashlib.sha256((args.source_root / "Cargo.lock").read_bytes()).hexdigest(),
                "topology": "same-host loopback; Python serial client; TCP_NODELAY; pipeline=1",
                "cpu_scope": "process CPU delta during measurement only; server endpoints exclude INFO queries",
                "limits": "3-second samples by default; no physical power-loss or target-environment claim"}
    if sys.platform.startswith("linux"):
        metadata["hardware"] = {name: capture(args.source_root, *command) for name, command in (
            ("cpu", ("lscpu",)), ("storage", ("lsblk", "-o", "NAME,TYPE,SIZE,ROTA,MODEL")),
            ("filesystem", ("findmnt", "-T", str(args.output))), ("memory", ("cat", "/proc/meminfo")))}
    elif sys.platform == "win32":
        metadata["hardware"] = capture(args.source_root, "powershell", "-NoProfile", "-Command",
            "[pscustomobject]@{cpu=(Get-CimInstance Win32_Processor | Select-Object Name,NumberOfCores,NumberOfLogicalProcessors); memory=(Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory; disks=(Get-CimInstance Win32_DiskDrive | Select-Object Model,Size); filesystems=(Get-CimInstance Win32_LogicalDisk | Select-Object DeviceID,FileSystem)} | ConvertTo-Json -Depth 4")
    write_json(args.output / "environment.json", metadata)
    modes = ("fast", "reliable") if args.event_mode == "both" else (args.event_mode,)
    skipped = [{"event_mode": "reliable", "reason": "Windows Reliable is explicitly unsupported"}] if os.name == "nt" and "reliable" in modes else []
    modes = tuple(mode for mode in modes if not (os.name == "nt" and mode == "reliable"))
    scenarios = (args.only,) if args.only else ("idle-0", "idle-1", "idle-100", "backlog")
    jobs = [(mode, scenario, repeat) for mode in modes for scenario in scenarios
            for repeat in range(1, args.repeats + 1)]
    random.Random(args.seed).shuffle(jobs)
    write_json(args.output / "schedule.json", {"jobs": jobs, "skipped": skipped})
    results = []
    for job in jobs:
        results.append(run_case(args, binary, *job))
        write_json(args.output / "results.json", results)
    write_json(args.output / "summary.json", {"runs": len(results), "passed": sum(row["acceptance"]["valid"] for row in results),
                                             "failed": sum(not row["acceptance"]["valid"] for row in results), "skipped": skipped})
    if any(not row["acceptance"]["valid"] for row in results):
        raise SystemExit(1)


if __name__ == "__main__":
    main()
