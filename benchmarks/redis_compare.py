#!/usr/bin/env python3
"""Redis 8 与 ling 的有界、交错、闭环对照；Bounded interleaved Redis 8 / ling comparison.

Linux only; starts private child processes, never attaches to an existing service.
GET / SET / SET PX use equal keys, payload bytes, connections and pipeline depth 1.
Fast retains ling events while Redis disables persistence. Reliable journals events;
Redis AOF always persists values and may group parallel commands into one fsync.
These durability objects are different: report each pair separately, never one ranking.
The shared Python process, protocol codecs and same-host CPU budget limit this workload.
Redis core retains its bundled vectorset module; no external modules are accepted.
"""
import argparse
from array import array
from collections import Counter
from datetime import datetime, timezone
import gzip
import hashlib
import json
import math
import os
from pathlib import Path
import random
import re
import shutil
import socket
import statistics
import subprocess
import sys
import threading
import time

from v020 import HEADER, WireError, capture, decode, encode, packet, process_snapshot

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
MAX_REPLY = 8 * 1024 * 1024
MAX_SAMPLES = 2_000_000
# 来自 Redis 官方 release、tag API 与 redis-hashes；Official release/tag/hash sources.
REDIS_PIN = {
    "version": "8.10.3", "checked_date": "2026-11-18",
    "released_utc": "2026-10-26T11:56:24Z",
    "commit": "1a2574ce1551d3de6e0b4a56756c0fdbc6659e4f",
    "sha256": "edde778f6bdee7581e42fc139ca9e07e2822212092f7501f99989656e3629d02",
    "archive": "https://download.redis.io/releases/redis-8.10.3.tar.gz",
    "release": "https://github.com/redis/redis/releases/tag/8.10.3",
    "hash_source": "https://raw.githubusercontent.com/redis/redis-hashes/master/README",
    "tag_source": "https://api.github.com/repos/redis/redis/git/ref/tags/8.10.3",
    "durability_source": "https://redis.io/docs/latest/operate/oss_and_stack/management/persistence/",
    "protocol_source": "https://redis.io/docs/latest/develop/reference/protocol-spec/",
}
# 固定源码默认静态加载此内置模块；The pinned core registers this built-in in moduleLoadInternalModules.
REDIS_CORE_MODULES = [[b"name", b"vectorset", b"ver", 1, b"path", b"", b"args", []]]


def json_value(value):
    """身份字节不经文本丢失；Preserve bytes in JSON without lossy text conversion."""
    if isinstance(value, bytes):
        return {"bytes_hex": value.hex()}
    if isinstance(value, dict):
        return {str(key): json_value(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [json_value(item) for item in value]
    return value


def write_json(path, value):
    """只写本次新建目录；Write only inside this run's new directory."""
    path.write_text(json.dumps(json_value(value), ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def digest(path):
    """流式摘要避免读入整个二进制；Hash binaries without loading them into memory."""
    with Path(path).open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


class Wire:
    """两协议共享绝对请求期限和接收预算；Shared absolute deadlines and bounded buffering."""
    def __init__(self, port, timeout):
        self.timeout, self.buffer, self.budget = timeout, bytearray(), MAX_REPLY
        self.stream = socket.create_connection(("127.0.0.1", port), timeout=timeout)
        self.stream.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.deadline = time.monotonic() + timeout

    def begin(self, payload):
        """发送和每次读取共用一个期限；One deadline covers send and every fragmented read."""
        self.deadline, self.budget = time.monotonic() + self.timeout, MAX_REPLY
        self.remaining()
        self.stream.sendall(payload)

    def remaining(self):
        """滴流不能续期；A trickling peer cannot extend a request deadline."""
        left = self.deadline - time.monotonic()
        if left <= 0:
            raise TimeoutError("whole request deadline exceeded")
        self.stream.settimeout(left)

    def fill(self):
        """一次最多接收64KiB；Read at most 64 KiB per system call."""
        self.remaining()
        data = self.stream.recv(min(65536, self.budget + 1))
        if not data:
            raise WireError("LOCAL_EOF: incomplete response")
        self.budget -= len(data)
        if self.budget < 0:
            raise WireError("LOCAL_LIMIT: response byte budget")
        self.buffer.extend(data)

    def take(self, size):
        """先检查长度再扩充；Validate lengths before buffering payloads."""
        if not 0 <= size <= MAX_REPLY:
            raise WireError("LOCAL_LIMIT: response length")
        while len(self.buffer) < size:
            self.fill()
        data = bytes(self.buffer[:size])
        del self.buffer[:size]
        return data

    def line(self):
        """RESP控制行有64KiB上界；Bound each RESP control line to 64 KiB."""
        while True:
            end = self.buffer.find(b"\r\n")
            if end >= 0:
                if end > 65536:
                    raise WireError("LOCAL_LIMIT: RESP line")
                return self.take(end + 2)[:-2]
            if len(self.buffer) > 65536:
                raise WireError("LOCAL_LIMIT: RESP line")
            self.fill()

    def close(self):
        """只关闭本客户端持有的连接；Close only the socket owned by this client."""
        self.stream.close()


class RedisClient:
    """仅实现本实验所需RESP2；Implement bounded RESP2, without third-party clients."""
    def __init__(self, port, timeout):
        self.wire = Wire(port, timeout)

    def response(self, depth=0):
        """限制数组深度、数量、负载并验证CRLF；Bound arrays and validate framing."""
        if depth > 8:
            raise WireError("LOCAL_LIMIT: RESP nesting")
        line = self.wire.line()
        if not line:
            raise WireError("LOCAL_PROTOCOL: empty RESP line")
        kind, body = line[:1], line[1:]
        if kind == b"+":
            return body
        if kind == b"-":
            raise WireError("REDIS_ERROR: " + body.decode("utf-8", errors="replace"))
        if kind not in (b":", b"$", b"*") or not re.fullmatch(rb"-?[0-9]+", body):
            raise WireError("LOCAL_PROTOCOL: invalid RESP type or length")
        size = int(body)
        if kind == b":":
            return size
        if size == -1:
            return None
        if size < 0 or size > (4096 if kind == b"*" else MAX_REPLY - 2):
            raise WireError("LOCAL_LIMIT: RESP container")
        if kind == b"*":
            return [self.response(depth + 1) for _ in range(size)]
        data = self.wire.take(size + 2)
        if data[-2:] != b"\r\n":
            raise WireError("LOCAL_PROTOCOL: RESP bulk terminator")
        return data[:-2]

    def request(self, command, arguments=()):
        """严格串行，每个错误都交给调用者；Serial requests, with no implicit retries."""
        values = [command, *arguments]
        encoded = [item if isinstance(item, bytes) else str(item).encode("utf-8") for item in values]
        body = b"*" + str(len(encoded)).encode() + b"\r\n" + b"".join(
            b"$" + str(len(item)).encode() + b"\r\n" + item + b"\r\n" for item in encoded)
        self.wire.begin(body)
        result = self.response()
        self.wire.remaining()
        return result

    def close(self):
        """释放连接；Release this connection."""
        self.wire.close()


class LingClient:
    """复用冻结Value编码，同样受绝对期限约束；Reuse Value codecs with the same deadline."""
    def __init__(self, port, timeout):
        self.wire, self.request_id = Wire(port, timeout), 0
        try:
            self.hello = self.request("HELLO", [1])
        except BaseException:
            self.close()
            raise

    def request(self, command, arguments=()):
        """验证版本、ID、状态及完整负载；Validate version, ID, status and full payload."""
        arguments = list(arguments)
        if command == "SET" and len(arguments) == 4 and arguments[2] == "PX":
            # 时间参数必须是 Int；共享实验编码器的普通整数仍是 HELLO 所需的 UInt。
            # Time arguments require Int; ordinary helper integers retain HELLO's UInt contract.
            ttl = arguments[3]
            if type(ttl) is not int or not 0 < ttl < 2**63:
                raise WireError("LOCAL_ENCODING: SET PX requires a positive signed Int TTL")
            encoded_arguments = (b"\x07" + len(arguments).to_bytes(4, "big")
                                 + b"".join(encode(item) for item in arguments[:-1])
                                 + b"\x02" + ttl.to_bytes(8, "big", signed=True))
        else:
            encoded_arguments = encode(arguments)
        self.request_id += 1
        self.wire.begin(packet(1, self.request_id, encode(command) + encoded_arguments))
        while True:
            magic, version, kind, flags, identity, size = HEADER.unpack(self.wire.take(HEADER.size))
            if magic != b"LG" or version != 1 or flags or size > MAX_REPLY - HEADER.size:
                raise WireError("LOCAL_PROTOCOL: invalid ling header")
            payload = self.wire.take(size)
            if kind == 5 and identity == 0 and not payload:
                continue
            if kind not in (2, 6) or (kind == 2 and (identity == 0 or identity > self.request_id)) or (
                    kind == 6 and identity > self.request_id):
                raise WireError("LOCAL_PROTOCOL: unexpected ling frame or ID")
            if kind == 2 and identity < self.request_id:
                continue
            status = payload[:1] if kind == 2 else b"\x01"
            if status not in (b"\x00", b"\x01"):
                raise WireError("LOCAL_PROTOCOL: ling status")
            value, offset = decode(payload, int(kind == 2))
            if status == b"\x01":
                detail, offset = decode(payload, offset)
                if offset != len(payload) or not isinstance(value, str) or not isinstance(detail, str):
                    raise WireError("LOCAL_PROTOCOL: ling error fields")
                raise WireError(value + ": " + detail)
            if offset != len(payload):
                raise WireError("LOCAL_PROTOCOL: ling trailing bytes")
            self.wire.remaining()
            return value

    def close(self):
        """释放连接；Release this connection."""
        self.wire.close()


def key_at(index):
    """两协议使用相同16字节Key；Use identical 16-byte ASCII keys for both protocols."""
    return f"cmp:{index:012d}"


def value_at(kind, size, sequence):
    """至少16字节，写入序号保证变化；At least 16 bytes, with changing write identities."""
    prefix = f"{sequence:016x}"
    if kind == "string":
        return prefix + "s" * (size - 16)
    return sequence.to_bytes(16, "big") + b"\x00\xff" * ((size - 16) // 2) + b"\x80" * ((size - 16) % 2)


def expected_wire(system, value):
    """Redis没有ling类型标签，比较相同UTF-8/原始字节；Redis returns the same bytes without type tags."""
    return value.encode("utf-8") if system == "redis" and isinstance(value, str) else value


def perform(client, system, operation, key, value, ttl_ms):
    """准备参数后计时，包含编码/网络/解码；Time codecs, network and result validation."""
    arguments = [key] if operation == "get" else [key, value]
    if operation == "set-px":
        arguments.extend(["PX", ttl_ms])
    started = time.perf_counter_ns()
    reply = client.request("GET" if operation == "get" else "SET", arguments)
    expected = expected_wire(system, value) if operation == "get" else (b"OK" if system == "redis" else True)
    if type(reply) is not type(expected) or reply != expected:
        raise WireError("LOCAL_RESULT: value, type or success mismatch")
    return time.perf_counter_ns() - started


def quantiles(values):
    """每次独立最近秩，不合并重复；Independent nearest-rank quantiles, never pooled repeats."""
    ordered = sorted(values)
    return {name: ordered[max(0, math.ceil(len(ordered) * fraction) - 1)] / 1000 if ordered else None
            for name, fraction in (("p50_us", .5), ("p95_us", .95), ("p99_us", .99), ("p999_us", .999))}


def schedule(seed, modes, kinds, operations, repeats):
    """每组相邻交错两系统，组序有种子；Seed group order and alternate each system pair."""
    rng = random.Random(seed)
    groups = [(mode, kind, operation, repeat) for repeat in range(1, repeats + 1)
              for mode in modes for kind in kinds for operation in operations]
    rng.shuffle(groups)
    result = []
    for mode, kind, operation, repeat in groups:
        systems = ["ling", "redis"]
        rng.shuffle(systems)
        result.extend(dict(system=system, mode=mode, kind=kind, operation=operation, repeat=repeat)
                      for system in systems)
    return result


def configuration(system, mode, port, connections, memory, journal):
    """显式固定影响比较的配置；Pin persistence, memory, eviction and connection settings."""
    if system == "redis":
        return (f'bind 127.0.0.1\nport {port}\nprotected-mode yes\ndaemonize no\nsave ""\n'
                f'appendonly {"yes" if mode == "reliable" else "no"}\nappendfsync always\n'
                'no-appendfsync-on-rewrite no\nauto-aof-rewrite-percentage 100\n'
                'auto-aof-rewrite-min-size 64mb\naof-use-rdb-preamble yes\n'
                f'maxmemory {memory}\nmaxmemory-policy noeviction\nmaxclients {connections + 8}\n'
                'io-threads 1\nhz 10\ndir .\nloglevel warning\n')
    return (f'[server]\nhost="127.0.0.1"\nport={port}\nmax_connections={connections + 8}\n'
            'idle_timeout_ms=600000\nrequest_timeout_ms=30000\nmax_request_bytes=2097152\n'
            'max_response_bytes=2097152\n[engine]\nshard_count=4\n'
            f'max_memory_bytes={memory}\n[eviction]\npolicy="noeviction"\n[events]\nmode="{mode}"\n'
            'buffer_bytes=67108864\nnotification_enabled=true\n[log]\nlevel="error"\n'
            + (f'[event_journal]\ndir="journal"\nmax_bytes={journal}\n' if mode == "reliable" else ''))


def free_port():
    """系统分配临时端口；Ask the OS for an ephemeral loopback port."""
    with socket.socket() as stream:
        stream.bind(("127.0.0.1", 0))
        return stream.getsockname()[1]


def owns_listener(pid, port, proc=Path("/proc")):
    """发起协议请求前核实监听socket属于自有子进程；Verify the child's Linux listener before connecting."""
    try:
        directory = proc / str(pid)
        inodes = set()
        for path in (directory / "fd").iterdir():
            try:
                target = os.readlink(path)
            except OSError:
                continue
            if target.startswith("socket:[") and target.endswith("]"):
                inodes.add(target[8:-1])
        endpoint = f"0100007F:{port:04X}"
        for line in (directory / "net" / "tcp").read_text().splitlines()[1:]:
            fields = line.split()
            if len(fields) >= 10 and fields[1] == endpoint and fields[3] == "0A" and fields[9] in inodes:
                return True
    except OSError:
        # 启动中fd可能消失，下一次有界轮询重新核验；A closing fd is retried within startup's deadline.
        pass
    return False


def server_info(client, system):
    """保存服务端报告及Redis有效配置；Keep runtime reports and all effective Redis settings."""
    if system == "ling":
        return client.request("INFO", ["all"])
    raw = client.request("INFO", ["all"])
    if not isinstance(raw, bytes):
        raise WireError("LOCAL_RESULT: Redis INFO is not bytes")
    return {"info": raw.decode("utf-8"), "config": client.request("CONFIG", ["GET", "*"]),
            "modules": client.request("MODULE", ["LIST"])}


def memory_report(info, system):
    """保留不同的计费口径，不把Redis使用量叫作ling受管预算；Keep each memory accounting scope explicit."""
    if system == "ling":
        return {"scope": "ling managed cache budget; excludes event/journal/network memory",
                "bytes": info["memory"]["managed_bytes"]}
    fields = dict(line.split(":", 1) for line in info["info"].splitlines() if line and not line.startswith("#") and ":" in line)
    return {"scope": "Redis used_memory including allocator-accounted server structures",
            "bytes": int(fields["used_memory"]), "dataset_bytes": int(fields.get("used_memory_dataset", "0"))}


def resource_snapshot(pid):
    """原生CPU/RSS加Linux峰值；Native process CPU/RSS plus Linux resident high-water mark."""
    result = process_snapshot(pid)
    try:
        fields = dict(line.split(":", 1) for line in Path(f"/proc/{pid}/status").read_text().splitlines() if ":" in line)
        result["peak_rss_bytes"] = int(fields["VmHWM"].split()[0]) * 1024
    except (OSError, KeyError, ValueError) as error:
        result["peak_unavailable"] = str(error)
    return result


def save_latencies(path, rows):
    """即使本轮失败也保存已完成样本；Preserve completed latency samples even for failed runs."""
    with gzip.open(path, "wt", encoding="utf-8") as output:
        output.write("[")
        count = 0
        for row in sorted(rows, key=lambda row: row["worker"]):
            for sample in row["samples"]:
                output.write(("," if count else "") + str(sample))
                count += 1
        output.write("]\n")


def run_case(args, case, binary, root, server_cpus):
    """每次新服务和目录；Each repeat uses a fresh process and persistence directory."""
    name = "-".join(str(case[key]) for key in ("mode", "kind", "operation", "repeat", "system"))
    directory = root / name
    directory.mkdir()
    port, system = free_port(), case["system"]
    config = directory / ("redis.conf" if system == "redis" else "ling.toml")
    config.write_text(configuration(system, case["mode"], port, args.connections,
                                    args.memory_bytes, args.journal_bytes), encoding="utf-8")
    factory = RedisClient if system == "redis" else LingClient
    clients, threads, errors, rows = [], [], [], []
    abort, start = threading.Event(), threading.Event()
    ready = threading.Barrier(args.connections + 1)
    state, observer, server = {}, None, None
    result = {**case, "config": config.name, "errors": errors, "valid": False,
              "pipeline": 1, "connections": args.connections}
    with (directory / "server.log").open("wb") as log:
        try:
            command = [str(binary), str(config)] if system == "redis" else [str(binary), "--config", str(config)]
            if server_cpus is not None:
                command = ["taskset", "-c", ",".join(map(str, sorted(server_cpus))), *command]
            result["server_command"] = command
            server = subprocess.Popen(command, cwd=directory, stdout=log, stderr=log)
            until = time.monotonic() + args.timeout
            while True:
                if server.poll() is not None:
                    raise RuntimeError("server startup failed; see server.log")
                if not owns_listener(server.pid, port):
                    if time.monotonic() >= until:
                        raise RuntimeError("private child did not own the configured listener before startup deadline")
                    time.sleep(.02)
                    continue
                try:
                    observer = factory(port, args.timeout)
                    if observer.request("PING") not in ("PONG", b"PONG"):
                        raise WireError("LOCAL_RESULT: startup PING")
                    if server.poll() is not None or not owns_listener(server.pid, port):
                        raise RuntimeError("private child listener disappeared during startup")
                    break
                except OSError:
                    if observer is not None:
                        observer.close()
                        observer = None
                    if server.poll() is not None or time.monotonic() >= until:
                        raise RuntimeError("server startup failed; see server.log")
                    time.sleep(.02)
            if system == "redis":
                metadata = server_info(observer, system)
                result["effective_configuration"] = metadata
                settings = dict(zip(metadata["config"][::2], metadata["config"][1::2]))
                wanted = {b"appendonly": b"yes" if case["mode"] == "reliable" else b"no",
                          b"appendfsync": b"always", b"save": b"", b"io-threads": b"1",
                          b"maxmemory-policy": b"noeviction", b"maxmemory": str(args.memory_bytes).encode()}
                result["configuration_mismatches"] = {key.decode(): {"expected": value, "actual": settings.get(key)}
                                                      for key, value in wanted.items() if settings.get(key) != value}
                if result["configuration_mismatches"] or metadata["modules"] != REDIS_CORE_MODULES:
                    raise RuntimeError("Redis effective configuration or module mismatch")
            setup_deadline = time.monotonic() + 60
            for index in range(args.keys):
                if time.monotonic() >= setup_deadline:
                    raise TimeoutError("preload exceeded its 60-second phase budget")
                value = value_at(case["kind"], args.value_bytes, index)
                try:
                    perform(observer, system, "set", key_at(index), value, args.ttl_ms)
                except BaseException as error:
                    known_rejection = isinstance(error, WireError) and str(error).startswith("ERR_") and not str(error).startswith("ERR_COMMIT_UNKNOWN")
                    errors.append({"phase": "preload", "key_index": index, "key": key_at(index),
                                   "error": repr(error), "monotonic_ns": time.monotonic_ns(),
                                   "write_result_may_be_unknown": not known_rejection})
                    raise

            def work(index):
                """每连接私有样本，首错停止；Private samples per connection, stop on first error."""
                samples, client = array("Q"), None
                row = {"worker": index, "samples": samples}
                rows.append(row)
                stage, iteration, key_index, in_request = "connecting", None, None, False
                try:
                    if server.poll() is not None or not owns_listener(server.pid, port):
                        raise RuntimeError("private child does not own the worker listener")
                    client = factory(port, args.timeout)
                    clients.append(client)
                    if server.poll() is not None or not owns_listener(server.pid, port):
                        raise RuntimeError("private child listener disappeared while connecting a worker")
                    stage = "warmup"
                    for iteration in range(args.warmup):
                        if abort.is_set():
                            raise RuntimeError("warmup cancelled after another worker failed")
                        key_index = (iteration * args.connections + index) % args.keys
                        value = value_at(case["kind"], args.value_bytes, key_index if case["operation"] == "get"
                                         else args.keys + iteration * args.connections + index)
                        in_request = True
                        perform(client, system, case["operation"], key_at(key_index), value, args.ttl_ms)
                        in_request = False
                    ready.wait(timeout=60)
                    if not start.wait(timeout=4 * args.timeout + 1):
                        raise TimeoutError("measurement start gate deadline exceeded")
                    stage = "measured"
                    iteration = args.warmup
                    while not abort.is_set() and time.perf_counter_ns() < state["end_ns"]:
                        key_index = (iteration * args.connections + index) % args.keys
                        value = value_at(case["kind"], args.value_bytes, key_index if case["operation"] == "get"
                                         else args.keys + iteration * args.connections + index)
                        in_request = True
                        samples.append(perform(client, system, case["operation"], key_at(key_index), value, args.ttl_ms))
                        in_request = False
                        iteration += 1
                        if len(samples) >= MAX_SAMPLES // args.connections:
                            raise WireError("LOCAL_LIMIT: latency sample budget exhausted")
                    row["end_ns"] = time.perf_counter_ns()
                except BaseException as error:
                    known_rejection = isinstance(error, WireError) and str(error).startswith("ERR_") and not str(error).startswith("ERR_COMMIT_UNKNOWN")
                    errors.append({"worker": index, "error": repr(error), "phase": stage,
                                   "iteration": iteration, "key_index": key_index,
                                   "monotonic_ns": time.monotonic_ns(),
                                   "write_result_may_be_unknown": in_request and case["operation"] != "get" and not known_rejection})
                    abort.set()
                    ready.abort()
                finally:
                    if client is not None:
                        client.close()

            threads = [threading.Thread(target=work, args=(index,), daemon=True) for index in range(args.connections)]
            warmup_started = time.monotonic()
            for thread in threads:
                thread.start()
            ready.wait(timeout=60)
            result["info_before"] = server_info(observer, system)
            result["memory_before"] = memory_report(result["info_before"], system)
            result["resources_before"] = resource_snapshot(server.pid)
            if case["operation"] == "set-px" and time.monotonic() - warmup_started + args.seconds + args.timeout >= args.ttl_ms / 1000:
                raise RuntimeError("TTL may expire during this measurement; increase --ttl-ms")
            state["start_ns"] = time.perf_counter_ns()
            state["end_ns"] = state["start_ns"] + round(args.seconds * 1e9)
            cpu_before = time.process_time()
            start.set()
            deadline = time.monotonic() + args.seconds + args.timeout + 2
            for thread in threads:
                thread.join(max(0, deadline - time.monotonic()))
            result["client_cpu_seconds"] = time.process_time() - cpu_before
            if any(thread.is_alive() for thread in threads):
                raise RuntimeError("load worker did not stop within absolute request deadline")
            result["resources_after"] = resource_snapshot(server.pid)
            result["info_after"] = server_info(observer, system)
            result["memory_after"] = memory_report(result["info_after"], system)
            samples = array("Q", (sample for row in rows for sample in row["samples"]))
            measured_end = max([state["start_ns"], *(row.get("end_ns", state["start_ns"]) for row in rows)])
            result.update(successes=len(samples), start_ns=state["start_ns"], planned_end_ns=state["end_ns"],
                          measured_end_ns=measured_end, elapsed_seconds=(measured_end - state["start_ns"]) / 1e9,
                          worker_counts=[len(row["samples"]) for row in sorted(rows, key=lambda row: row["worker"])],
                          **quantiles(samples))
            result["successes_per_second"] = len(samples) / result["elapsed_seconds"] if result["elapsed_seconds"] else None
            before, after = result["resources_before"].get("cpu_seconds"), result["resources_after"].get("cpu_seconds")
            result["server_cpu_seconds"] = after - before if before is not None and after is not None else None
            result["valid"] = not errors and bool(samples) and all(row["samples"] for row in rows)
        except BaseException as error:
            errors.append({"phase": "controller", "error": repr(error)})
        finally:
            abort.set()
            ready.abort()
            start.set()
            for client in clients:
                try:
                    client.wire.stream.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
            cleanup_deadline = time.monotonic() + args.timeout + 1
            for thread in threads:
                thread.join(max(0, cleanup_deadline - time.monotonic()))
            if observer is not None:
                observer.close()
            if server is not None:
                if server.poll() is None:
                    server.terminate()
                    try:
                        server.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        server.kill()
                        server.wait(timeout=5)
                else:
                    errors.append({"phase": "server", "error": f"unexpected exit {server.returncode}"})
                result["server_exit_after_cleanup"] = server.returncode
            result["threads_alive"] = sum(thread.is_alive() for thread in threads)
            result["valid"] = result["valid"] and not errors and not result["threads_alive"]
            if not result["threads_alive"]:
                save_latencies(directory / "latencies-ns.json.gz", rows)
    write_json(directory / "result.json", result)
    print(f"{name}: valid={result['valid']}; rate={result.get('successes_per_second')}", flush=True)
    return result


def summarize(results):
    """每系统每场景三次独立范围，不合并延迟；Keep separate repeat ranges and durability pairs."""
    groups = {}
    for row in results:
        key = tuple(row[field] for field in ("mode", "kind", "operation", "system"))
        groups.setdefault(key, []).append(row)
    output = []
    for key, rows in sorted(groups.items()):
        ranges = {}
        for field in ("successes_per_second", "p50_us", "p95_us", "p99_us", "p999_us",
                      "server_cpu_seconds", "client_cpu_seconds"):
            values = [row[field] for row in rows if row.get(field) is not None]
            ranges[field] = {"min": min(values), "median": statistics.median(values), "max": max(values)} if values else None
        output.append(dict(zip(("mode", "kind", "operation", "system"), key)) |
                      {"repeats": len(rows), "all_valid": all(row["valid"] for row in rows), "ranges": ranges})
    return output


def main(argv=None):
    """先冻结来源/计划再执行，禁止覆盖旧结果；Freeze inputs and plan before execution."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path, nargs="?")
    parser.add_argument("--print-redis-pin", action="store_true")
    parser.add_argument("--ling-binary", type=Path)
    parser.add_argument("--ling-revision", help="operator-asserted build revision; recorded separately from tool checkout")
    parser.add_argument("--redis-binary", type=Path)
    parser.add_argument("--redis-source-archive", type=Path)
    parser.add_argument("--seconds", type=float, default=3)
    parser.add_argument("--timeout", type=float, default=10)
    parser.add_argument("--connections", type=int, default=4)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--keys", type=int, default=1000)
    parser.add_argument("--value-bytes", type=int, default=64)
    parser.add_argument("--warmup", type=int, default=100)
    parser.add_argument("--ttl-ms", type=int, default=600000)
    parser.add_argument("--memory-bytes", type=int, default=268435456)
    parser.add_argument("--journal-bytes", type=int, default=1073741824)
    parser.add_argument("--mode", choices=("fast", "reliable", "both"), default="both")
    parser.add_argument("--kind", choices=("string", "bytes", "both"), default="both")
    parser.add_argument("--operation", choices=("get", "set", "set-px", "all"), default="all")
    parser.add_argument("--seed", type=int, default=20261118)
    args = parser.parse_args(argv)
    if args.print_redis_pin:
        print(json.dumps(REDIS_PIN, indent=2))
        return 0
    if sys.platform != "linux":
        parser.error("measurement requires Linux; unit tests remain portable")
    if args.output is None or args.ling_binary is None or args.redis_binary is None or not args.ling_revision:
        parser.error("output, --ling-binary, --ling-revision and --redis-binary are required")
    if not (math.isfinite(args.seconds) and 0 < args.seconds <= 60 and math.isfinite(args.timeout)
            and 0 < args.timeout <= 60 and 1 <= args.connections <= 64 and 3 <= args.repeats <= 10
            and 1 <= args.keys <= 100000 and 16 <= args.value_bytes <= 1044480 and 0 <= args.warmup <= 1000
            and args.ttl_ms > (args.seconds + args.timeout) * 1000 and args.memory_bytes >= 67108864
            and args.journal_bytes >= 67108864):
        parser.error("invalid duration, resource, payload, repeat or TTL budget")
    binaries = {"ling": args.ling_binary.resolve(strict=True), "redis": args.redis_binary.resolve(strict=True)}
    version = capture(ROOT, str(binaries["redis"]), "--version")
    if version.get("returncode") != 0 or not re.search(r"\bv=" + re.escape(REDIS_PIN["version"]) + r"(?:\s|$)", version["stdout"]):
        parser.error("Redis executable must report pinned stable version " + REDIS_PIN["version"])
    if args.redis_source_archive and digest(args.redis_source_archive) != REDIS_PIN["sha256"]:
        parser.error("Redis source archive differs from official pinned SHA-256")
    if len(os.sched_getaffinity(0)) > 1 and shutil.which("taskset") is None:
        parser.error("taskset is required to bind the server before any worker threads start")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    inputs = output / "inputs"
    inputs.mkdir()
    sources = {"controller": Path(__file__), "protocol_helper": Path(sys.modules[decode.__module__].__file__),
               "tests": Path(__file__).with_name("test_redis_compare.py"), **binaries}
    if args.redis_source_archive:
        sources["redis_source_archive"] = args.redis_source_archive
    manifest = {}
    for label, source in sources.items():
        target = inputs / (label + "-" + source.name)
        original_digest = digest(source)
        shutil.copyfile(source, target)
        copied_digest = digest(target)
        if copied_digest != original_digest or digest(source) != original_digest:
            raise RuntimeError("input changed while freezing: " + str(source))
        if label in binaries:
            target.chmod(target.stat().st_mode | 0o100)
            binaries[label] = target
        manifest[label] = {"original": str(source), "copy": str(target.relative_to(output)), "sha256": copied_digest}
    original_cpus = sorted(os.sched_getaffinity(0))
    split = max(1, len(original_cpus) // 2)
    server_cpus = set(original_cpus[:split]) if len(original_cpus) > 1 else None
    client_cpus = set(original_cpus[split:]) if len(original_cpus) > 1 else None
    modes = ("fast", "reliable") if args.mode == "both" else (args.mode,)
    kinds = ("string", "bytes") if args.kind == "both" else (args.kind,)
    operations = ("get", "set", "set-px") if args.operation == "all" else (args.operation,)
    plan = schedule(args.seed, modes, kinds, operations, args.repeats)
    environment = {"created_utc": datetime.now(timezone.utc).isoformat(), "redis_pin": REDIS_PIN,
                   "redis_source_archive_verified": bool(args.redis_source_archive), "inputs": manifest,
                   "source_binary_relation": "binary versions/hashes retained; an optional archive hash does not attest that this binary was built from it",
                   "ling_revision_operator_asserted": args.ling_revision,
                   "binary_versions": {**{name: capture(ROOT, str(path), "--version") for name, path in binaries.items()}},
                   "tool_checkout_head": capture(ROOT, "git", "rev-parse", "HEAD"), "tool_checkout_status": capture(ROOT, "git", "status", "--short"),
                   "python": sys.version, "platform": sys.platform, "original_cpus": original_cpus,
                   "server_cpus": sorted(server_cpus) if server_cpus else None,
                   "client_cpus": sorted(client_cpus) if client_cpus else None,
                   "arguments": {key: str(value) if isinstance(value, Path) else value for key, value in vars(args).items()},
                   "semantics": __doc__, "consumers": 0, "pipeline": 1,
                   "client_limits": "closed loop, same Python process, per-request codecs differ; not open arrivals or server saturation",
                   "wire_sizes": "application key/payload lengths match; protocol framing and wire request sizes inherently differ, no artificial padding",
                   "affinity_scope": "logical CPU sets; SMT siblings may share physical cores; server bound by taskset before exec",
                   "latency": "prepare key/value, then time encoding/send/receive/decode/validation; per-run nearest rank, no pooling",
                   "resource_scope": "CPU deltas bracket only measurement; RSS endpoints plus VmHWM lifetime peak include preload/warmup allocations",
                   "hardware": {name: capture(ROOT, *command) for name, command in (
                       ("cpu", ("lscpu",)), ("kernel", ("uname", "-a")),
                       ("filesystem", ("findmnt", "-T", str(output))), ("memory", ("cat", "/proc/meminfo")),
                       ("storage", ("lsblk", "-o", "NAME,TYPE,SIZE,ROTA,MODEL")))}}
    write_json(output / "environment.json", environment)
    write_json(output / "schedule.json", plan)
    results = []
    try:
        if client_cpus:
            os.sched_setaffinity(0, client_cpus)
        for case in plan:
            result = run_case(args, case, binaries[case["system"]], output, server_cpus)
            results.append(result)
            write_json(output / "results.json", results)
            if not result["valid"]:
                break
    finally:
        os.sched_setaffinity(0, original_cpus)
    passed = len(results) == len(plan) and all(result["valid"] for result in results)
    write_json(output / "summary.json", {"valid": passed, "planned_cases": len(plan), "executed_cases": len(results),
                                        "comparability": __doc__, "groups": summarize(results), "results": results})
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
