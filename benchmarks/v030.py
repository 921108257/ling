#!/usr/bin/env python3
"""0.3 候选及发布后验收：记录问题，保存隔离阶段和精确二进制身份。
Candidate and post-release acceptance: record findings with isolated phases and binary identity.
"""
import argparse
from collections import Counter
from dataclasses import dataclass
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
import urllib.request

from v020 import HEADER, MAX_PAYLOAD, ROOT, WireError, capture, exact, free_port, packet, process_snapshot, write_json
from v020 import Client as EventClient
from v020 import receive as event_receive

sys.dont_write_bytecode = True
TYPE_NAMES = ("null", "bool", "int", "uint", "float", "string", "bytes", "list", "map", "set")
POLICIES = ("noeviction", "lru", "lfu", "random", "ttl-first")
RESOURCE_ERRORS = ("ERR_OUT_OF_MEMORY", "ERR_JOURNAL_FULL")


@dataclass(frozen=True)
class Value:
    """不可变且保留标签；Map/Set 顺序不参与相等性。
    Immutable tagged values; Map/Set ordering does not affect equality.
    """
    tag: int
    data: object = None

    def __post_init__(self):
        """先验证再规范化，防止 Python 将 Bool、Int、UInt 合并。
        Validate before normalization so Python cannot merge Bool, Int and UInt.
        """
        tag, data = self.tag, self.data
        valid = 0 <= tag <= 9
        if tag == 0:
            valid = data is None
        elif tag == 1:
            valid = type(data) is bool
        elif tag in (2, 3):
            valid = type(data) is int and (-2**63 <= data < 2**63 if tag == 2 else 0 <= data < 2**64)
        elif tag == 4:
            valid = type(data) is float and not math.isnan(data)
            if valid and data == 0:
                object.__setattr__(self, "data", 0.0)
        elif tag in (5, 6):
            valid = type(data) is (str if tag == 5 else bytes)
        elif tag == 7:
            data = tuple(data)
            valid = all(isinstance(item, Value) for item in data)
            object.__setattr__(self, "data", data)
        elif tag == 8:
            data = tuple(data.items()) if isinstance(data, dict) else tuple(data)
            valid = all(type(key) is str and key and isinstance(item, Value) for key, item in data)
            valid = valid and len({key for key, _ in data}) == len(data)
            object.__setattr__(self, "data", tuple(sorted(data)))
        elif tag == 9:
            data = tuple(data)
            valid = all(isinstance(item, Value) and item.tag <= 6 for item in data)
            if valid:
                object.__setattr__(self, "data", frozenset(data))
        if not valid:
            raise WireError("LOCAL_VALUE: invalid tagged value")


def argument(value):
    """控制参数保持明确；存储数值必须显式传入 Value。
    Control arguments remain explicit; stored numbers must be tagged Values.
    """
    if isinstance(value, Value):
        return value
    if type(value) is str:
        return Value(5, value)
    raise WireError("LOCAL_VALUE: arguments require String or explicit Value")


def encode(value, depth=0):
    """有界编码全部十类值，不经 JSON 中转。
    Encode all ten types with bounds and without a JSON intermediate.
    """
    if depth > 12:
        raise WireError("LOCAL_LIMIT: nested value")
    value = argument(value)
    tag, data = value.tag, value.data
    result = bytes([tag])
    if tag == 1:
        result += bytes([data])
    elif tag in (2, 3, 4):
        result += struct.pack({2: ">q", 3: ">Q", 4: ">d"}[tag], data)
    elif tag in (5, 6):
        data = data.encode("utf-8") if tag == 5 else data
        if len(data) > MAX_PAYLOAD:
            raise WireError("LOCAL_LIMIT: scalar too large")
        result += struct.pack(">I", len(data)) + data
    elif tag in (7, 8, 9):
        items = data if tag != 9 else sorted(data, key=lambda item: encode(item))
        result += struct.pack(">I", len(items))
        for item in items:
            part = encode(item, depth + 1) if tag != 8 else encode(item[0], depth + 1) + encode(item[1], depth + 1)
            if len(result) + len(part) > MAX_PAYLOAD:
                raise WireError("LOCAL_LIMIT: container too large")
            result += part
    return result


def decode(data, offset=0, depth=0):
    """在分配前检查长度；拒绝 NaN、重复字段和集合内的容器。
    Check lengths before allocation; reject NaN, duplicate fields and nested Set members.
    """
    if depth > 12 or offset >= len(data):
        raise WireError("LOCAL_PROTOCOL: truncated or nested value")
    tag, offset = data[offset], offset + 1
    if tag == 0:
        return Value(0), offset
    if tag == 1:
        if offset >= len(data) or data[offset] > 1:
            raise WireError("LOCAL_PROTOCOL: invalid Bool")
        return Value(1, bool(data[offset])), offset + 1
    if tag in (2, 3, 4):
        if offset + 8 > len(data):
            raise WireError("LOCAL_PROTOCOL: truncated number")
        return Value(tag, struct.unpack_from({2: ">q", 3: ">Q", 4: ">d"}[tag], data, offset)[0]), offset + 8
    if tag not in (5, 6, 7, 8, 9) or offset + 4 > len(data):
        raise WireError("LOCAL_PROTOCOL: invalid value tag or length")
    count = struct.unpack_from(">I", data, offset)[0]
    offset += 4
    if count > len(data) - offset:
        raise WireError("LOCAL_PROTOCOL: impossible value length")
    if tag in (5, 6):
        raw = data[offset:offset + count]
        try:
            return Value(tag, raw.decode("utf-8") if tag == 5 else raw), offset + count
        except UnicodeError as error:
            raise WireError("LOCAL_PROTOCOL: invalid UTF-8") from error
    items = []
    for _ in range(count):
        first, offset = decode(data, offset, depth + 1)
        if tag == 8:
            if first.tag != 5 or not first.data:
                raise WireError("LOCAL_PROTOCOL: invalid Map key")
            second, offset = decode(data, offset, depth + 1)
            items.append((first.data, second))
        else:
            items.append(first)
    return Value(tag, items), offset


def evidence(value, complete=False):
    """证据保留完整整数与标签，极大值限制展示长度。
    Evidence preserves integer precision and tags while bounding displayed payloads.
    """
    if isinstance(value, Value):
        tag, data = value.tag, value.data
        if tag in (2, 3, 4):
            data = str(data)
        elif tag == 6:
            data = {"hex": (data if complete else data[:1024]).hex(), "bytes": len(data)}
        elif tag == 5 and len(data) > 1024 and not complete:
            data = {"prefix": data[:1024], "characters": len(data)}
        elif tag == 8:
            data = {key: evidence(item, complete) for key, item in (data if complete else data[:64])}
        elif tag in (7, 9):
            items = list(data)
            if tag == 9:
                items.sort(key=encode)
            data = [evidence(item, complete) for item in (items if complete else items[:64])]
        return {"type": TYPE_NAMES[tag], "value": data}
    if isinstance(value, dict):
        return {str(key): evidence(item, complete) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [evidence(item, complete) for item in value]
    return value


class ServerError(WireError):
    """区分明确拒绝与传输后结果未知；Distinguish explicit rejection from unknown transport outcomes."""
    def __init__(self, code, message):
        super().__init__(f"{code}: {message}")
        self.code = code


def receive(stream):
    """先验证响应封套再交给调用者匹配 ID。
    Validate response envelopes before the caller matches request IDs.
    """
    magic, version, kind, flags, request_id, size = HEADER.unpack(exact(stream, HEADER.size))
    if magic != b"LG" or version != 1 or flags or kind not in (2, 5, 6) or size > MAX_PAYLOAD:
        raise WireError("LOCAL_PROTOCOL: invalid frame header")
    payload = exact(stream, size)
    if kind == 5:
        if request_id or payload:
            raise WireError("LOCAL_PROTOCOL: invalid heartbeat")
        return kind, request_id, None
    if kind == 2 and not request_id:
        raise WireError("LOCAL_PROTOCOL: invalid response ID")
    if kind == 6 or payload[:1] == b"\x01":
        code, offset = decode(payload, int(kind == 2))
        message, offset = decode(payload, offset)
        if offset != size or code.tag != 5 or message.tag != 5:
            raise WireError("LOCAL_PROTOCOL: invalid error")
        return kind, request_id, ServerError(code.data, message.data)
    if payload[:1] != b"\x00":
        raise WireError("LOCAL_PROTOCOL: invalid response status")
    value, offset = decode(payload, 1)
    if offset != size:
        raise WireError("LOCAL_PROTOCOL: trailing response bytes")
    return kind, request_id, value


class Client:
    """每个工作线程独占连接；不重放任何请求。
    Each worker owns its connection and never replays a request.
    """
    def __init__(self, port, database=0):
        self.stream = socket.create_connection(("127.0.0.1", port), timeout=10)
        self.stream.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.request_id = 0
        try:
            self.hello = self.request("HELLO", [Value(3, 1)])
            fields = dict(self.hello.data) if self.hello.tag == 8 else {}
            if (fields.get("protocol") != Value(3, 1) or fields.get("server_version", Value(0)).tag != 5
                    or fields.get("events", Value(0)).tag != 1 or fields.get("reliable_events", Value(0)).tag != 1
                    or fields.get("stream_id", Value(0)).tag != 6 or len(fields["stream_id"].data) != 16):
                raise WireError("LOCAL_PROTOCOL: invalid HELLO capabilities")
            if self.request("SELECT", [Value(3, database)]) != Value(1, True):
                raise WireError("LOCAL_PROTOCOL: SELECT did not establish fixed database")
        except BaseException:
            self.close()
            raise

    def request(self, command, arguments=()):
        """串行请求支持心跳和旧响应，未来 ID 立即失败。
        Serial requests accept heartbeats and stale replies, rejecting future IDs.
        """
        self.request_id += 1
        self.stream.sendall(packet(1, self.request_id, encode(command) + encode(Value(7, [argument(arg) for arg in arguments]))))
        while True:
            kind, request_id, result = receive(self.stream)
            if kind == 5 or (kind == 2 and request_id < self.request_id):
                continue
            if kind == 2 and request_id != self.request_id:
                raise WireError("LOCAL_PROTOCOL: response ID mismatch")
            if kind == 6 and request_id > self.request_id:
                raise WireError("LOCAL_PROTOCOL: future ERROR ID")
            if isinstance(result, ServerError):
                raise result
            return result

    def close(self):
        """仅关闭本连接；Close only the owned connection."""
        self.stream.close()


class Records:
    """计数、对数延迟直方图与固定数量失败样例均有界。
    Counts, logarithmic latency histograms and finding examples are bounded.
    """
    def __init__(self, directory, seed, examples=200):
        self.directory, self.seed, self.limit = directory, seed, examples
        self.lock = threading.Lock()
        self.started = time.monotonic()
        self.counts, self.findings, self.errors, self.latencies = Counter(), Counter(), Counter(), Counter()
        self.examples = 0
        self.file = (directory / "findings.jsonl").open("w", encoding="utf-8")
        self.raw = gzip.open(directory / "errors.jsonl.gz", "wt", encoding="utf-8")
        self.event_file = gzip.open(directory / "consumer-events.jsonl.gz", "wt", encoding="utf-8")
        self.event_samples = 0
        self.context = threading.local()

    def count(self, name, amount=1):
        """有限调用点产生有限计数键；Finite call sites produce bounded counter keys."""
        with self.lock:
            self.counts[name] += amount

    def finding(self, category, **details):
        """保留复现输入和预期，不自动修复或重放。
        Retain reproduction input and expectation without automatic repair or replay.
        """
        with self.lock:
            self.findings[category] += 1
            row = {"seconds": time.monotonic() - self.started, "seed": self.seed,
                   "category": category, **getattr(self.context, "value", {}), **evidence(details, True)}
            self.raw.write(json.dumps(row, ensure_ascii=False, allow_nan=False) + "\n")
            self.raw.flush()
            if self.examples < self.limit:
                row = {**row, **evidence(details)}
                self.file.write(json.dumps(row, ensure_ascii=False, allow_nan=False) + "\n")
                self.file.flush()
                self.examples += 1

    def request(self, client, command, arguments=(), expected=None, allowed=()):
        """明确的容量拒绝另计；断线终止该工作线程，保留未知写入边界。
        Count resource rejections separately; connection loss ends the worker with unknown writes recorded.
        """
        started = time.perf_counter_ns()
        try:
            result = client.request(command, arguments)
            self.count("successful_requests")
            if getattr(self.context, "value", {}).get("phase") in ("shadow", "pressure"):
                self.count("producer_write_confirmations" if command in ("SET", "DEL", "PEXPIRE", "APPEND", "RPUSH", "FSET", "SADD")
                           else "producer_read_confirmations")
            if expected is not None and result != expected:
                self.finding("unexpected_result", command=command, arguments=arguments, expected=expected, actual=result)
            return True, result
        except ServerError as error:
            key = error.code if error.code.startswith("ERR_") and len(error.code) < 80 else "OTHER_SERVER_ERROR"
            with self.lock:
                self.errors[key] += 1
                context = getattr(self.context, "value", {})
                compact = context.get("phase") == "pressure" and command == "SET"
                row = {"seconds": time.monotonic() - self.started, "seed": self.seed, **context,
                       "category": "server_error", "command": command, "code": error.code,
                       "expected": error.code in allowed}
                if compact:
                    row["reproduction"] = "pressure_arguments(seed, worker, cycle, payload_bytes) in this exact script"
                    row["arguments_sha256"] = hashlib.sha256(encode(Value(7, [argument(arg) for arg in arguments]))).hexdigest()
                else:
                    row["arguments"] = evidence(arguments, True)
                self.raw.write(json.dumps(row, ensure_ascii=False, allow_nan=False) + "\n")
                self.raw.flush()
            if error.code not in allowed:
                self.finding("unexpected_server_error", command=command, arguments=arguments, error=str(error))
            else:
                self.count("expected_rejections")
            return False, error.code
        except (OSError, WireError) as error:
            self.finding("connection_or_protocol_failure", command=command, arguments=arguments,
                         error=str(error), outcome="write may have committed; request not replayed")
            raise
        finally:
            elapsed = max(1, (time.perf_counter_ns() - started) // 1000)
            with self.lock:
                self.latencies[min(63, elapsed.bit_length() - 1)] += 1
                self.counts["requests"] += 1

    def snapshot(self):
        """可并发保存阶段进度；Save phase progress while workers run."""
        with self.lock:
            return {"counts": dict(self.counts), "findings": dict(self.findings), "server_errors": dict(self.errors),
                    "finding_examples_saved": self.examples, "finding_examples_limit": self.limit,
                    "consumer_event_samples": self.event_samples,
                    "latency_histogram_us": {f"[{2**bucket},{2**(bucket+1)})": count
                                             for bucket, count in sorted(self.latencies.items())}}

    def close(self):
        """线程退出后关闭证据文件；Close evidence after worker termination."""
        self.file.close()
        self.raw.close()
        self.event_file.close()

    def event_evidence(self, category, **details):
        """所有消费异常完整保留，普通事件每百条采样且总量有界。
        Retain every consumer anomaly; sample ordinary events at a bounded rate.
        """
        with self.lock:
            row = {"seconds": time.monotonic()-self.started, "category": category, **details}
            if category != "event_sample":
                self.raw.write(json.dumps(row, ensure_ascii=False) + "\n")
                self.raw.flush()
            elif self.event_samples < 100000:
                self.event_file.write(json.dumps(row, ensure_ascii=False) + "\n")
                self.event_file.flush()
                self.event_samples += 1


def consumer_acceptance(mode, consumers):
    """消费存活、ACK 和预期 Fast 滞后独立验收。
    Validate consumer liveness, ACKs and explicitly expected Fast lag independently.
    """
    failures, expected = [], []
    if len(consumers) != 4:
        failures.append("four database/speed consumer results required")
    for name, row in consumers.items():
        if (row["termination"] == "expected_fast_lag" and mode == "fast" and name.startswith("slow-")
                and set(row["errors"]) == {"ERR_OFFSET_EXPIRED"}):
            expected.append(name)
            continue
        if row["termination"] != "requested_stop":
            failures.append(name + ": consumer stopped unexpectedly")
        if not row["received"] or row["acks"] != row["received"]:
            failures.append(name + ": absent or unconfirmed deliveries")
        if row["errors"]:
            failures.append(name + ": unexpected consumer errors")
    return {"valid": not failures, "failures": failures, "expected_fast_lag": expected,
            "scope": "ACKs confirm observed deliveries; pending backlog is reported separately by INFO"}


class Consumer:
    """每库快慢组独立连接；已确认序号用连续水位和有界空洞集合保存。
    Fast/slow groups own connections; confirmed IDs use a contiguous watermark and bounded holes.
    """
    def __init__(self, args, records, port, mode, database, slow, stream_id, stop):
        self.args, self.records, self.port, self.mode = args, records, port, mode
        self.database, self.slow, self.expected_stream, self.stop = database, slow, stream_id, stop
        self.name = f"{'slow' if slow else 'fast'}-db{database}"
        self.group = "acceptance-" + self.name
        self.started = None
        self.lock = threading.Lock()
        self.watermarks, self.holes = {shard: 0 for shard in range(4)}, {shard: set() for shard in range(4)}
        self.state = {"received": 0, "acks": 0, "duplicates": 0, "reconnects": 0,
                      "planned_disconnected_seconds": 0.0, "errors": {}, "termination": "starting"}
        self.client = self.connect()
        self.thread = threading.Thread(target=self.run, daemon=True)

    def connect(self):
        """只用于首次连接或计划重连，不恢复未知 ACK。
        Connect initially or on planned reconnects, never after an unknown ACK result.
        """
        client = EventClient(self.port)
        try:
            if client.hello.get("stream_id") != self.expected_stream:
                raise WireError("LOCAL_STREAM_MISMATCH: consumer HELLO stream changed")
            if client.request("SELECT", [self.database]) is not True:
                raise WireError("LOCAL_CONSUMER: SELECT rejected")
            client.subscribe("acceptance:*", self.group)
            return client
        except BaseException:
            client.close()
            raise

    def acknowledge(self, event):
        """校验事件身份后 ACK，重复投递与确认计数分别保存。
        Validate identity before ACK and retain delivery and confirmation counts separately.
        """
        identity = event.get("event_id", {}) if isinstance(event, dict) else {}
        stream, database, shard, sequence = (identity.get(field) for field in ("stream_id", "db", "shard", "sequence"))
        if (type(stream) is not bytes or len(stream) != 16 or stream != self.expected_stream
                or type(database) is not int or database != self.database or type(shard) is not int or shard not in self.watermarks
                or type(sequence) is not int or not 0 < sequence < 2**64
                or event.get("event_type") not in ("KEY_CREATED", "KEY_UPDATED", "KEY_DELETED", "KEY_EXPIRED", "KEY_EVICTED", "KEY_TTL_CHANGED")):
            raise WireError("LOCAL_EVENT_ID: invalid or changed event identity")
        with self.lock:
            self.state["received"] += 1
            received = self.state["received"]
            duplicate = sequence <= self.watermarks[shard] or sequence in self.holes[shard]
            self.state["duplicates"] += int(duplicate)
        details = {"consumer": self.name, "stream_id": stream.hex(), "database": database,
                   "shard": shard, "sequence": str(sequence), "event_type": event["event_type"]}
        with self.lock:
            self.state["last_received_id"] = details
        if duplicate:
            self.records.event_evidence("duplicate_delivery", **details)
        if received == 1 or received % 100 == 0:
            self.records.event_evidence("event_sample", **details)
        if self.slow:
            self.stop.wait(self.args.slow_consumer_delay)
        if self.client.request(None, identity, kind=4) is not True:
            raise WireError("LOCAL_ACK_RESULT: ACK not confirmed")
        with self.lock:
            self.state["acks"] += 1
            if sequence > self.watermarks[shard]:
                self.holes[shard].add(sequence)
                while self.watermarks[shard]+1 in self.holes[shard]:
                    self.watermarks[shard] += 1
                    self.holes[shard].remove(self.watermarks[shard])
                if sum(len(pending) for pending in self.holes.values()) > 256:
                    raise WireError("LOCAL_EVENT_GAPS: confirmed out-of-order ID budget exceeded")

    def run(self):
        """计划掉线有明确计数；任意非计划错误停止该消费者并保留原因。
        Count planned disconnects; any unplanned error stops that consumer with evidence.
        """
        started = time.monotonic()
        self.started = started
        next_reconnect = started + self.args.consumer_reconnect_seconds
        with self.lock:
            self.state["termination"] = "running"
        try:
            while not self.stop.is_set():
                if self.slow and time.monotonic() >= next_reconnect:
                    self.client.close()
                    offline = time.monotonic()
                    self.stop.wait(self.args.consumer_disconnect_seconds)
                    if self.stop.is_set():
                        with self.lock:
                            self.state["planned_disconnected_seconds"] += time.monotonic()-offline
                        break
                    self.client = self.connect()
                    with self.lock:
                        self.state["reconnects"] += 1
                        self.state["planned_disconnected_seconds"] += time.monotonic()-offline
                    self.records.event_evidence("planned_consumer_reconnect", consumer=self.name)
                    next_reconnect = time.monotonic() + self.args.consumer_reconnect_seconds
                if self.client.events:
                    self.acknowledge(self.client.events.popleft())
                elif select.select([self.client.stream], [], [], .1)[0]:
                    kind, _, event = event_receive(self.client.stream)
                    if kind == 3:
                        self.acknowledge(event)
                    elif kind != 5:
                        raise WireError("LOCAL_CONSUMER: unexpected idle response")
            with self.lock:
                self.state["termination"] = "requested_stop"
        except Exception as error:
            code = str(error).split(":", 1)[0]
            expected = self.mode == "fast" and self.slow and code == "ERR_OFFSET_EXPIRED"
            with self.lock:
                self.state["errors"][code] = self.state["errors"].get(code, 0) + 1
                self.state["termination"] = "expected_fast_lag" if expected else "error"
            if expected:
                self.records.event_evidence("expected_fast_consumer_lag", consumer=self.name, error=str(error))
            else:
                self.records.finding("consumer_failed", consumer=self.name, error=str(error),
                                     last_received_id=self.state.get("last_received_id"),
                                     outcome="any unconfirmed ACK is unknown; no automatic reconnect or replay")
        finally:
            self.client.close()
            with self.lock:
                self.state["elapsed_seconds"] = time.monotonic()-started

    def snapshot(self):
        """复制固定大小状态，避免边读边变更。
        Copy bounded state without racing dictionary updates.
        """
        with self.lock:
            elapsed = self.state.get("elapsed_seconds", time.monotonic()-self.started if self.started else 0)
            return {**self.state, "errors": dict(self.state["errors"]), "elapsed_seconds": elapsed,
                    "active_seconds": max(0, elapsed-self.state["planned_disconnected_seconds"]),
                    "confirmed_watermarks": {str(shard): str(value) for shard, value in self.watermarks.items()},
                    "confirmed_above_watermark": {str(shard): len(pending) for shard, pending in self.holes.items()}}


class CappedLog:
    """完整错误日志压缩保存，另留两个各 4 MiB 的便读日志段。
    Compress the complete error log and retain two readable 4 MiB tail segments.
    """
    def __init__(self, pipe, directory, records):
        self.pipe, self.path = pipe, directory / "server.log"
        self.records = records
        self.total = 0
        self.panic_markers = 0
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def run(self):
        """轮转便读尾部并保留完整压缩输出，外层检查磁盘余量。
        Rotate readable tails and preserve compressed output; the caller checks free disk space.
        """
        size, previous = 0, b""
        stream = self.path.open("wb")
        complete = gzip.open(self.path.with_name("server-complete.log.gz"), "wb")
        try:
            while chunk := self.pipe.read(8192):
                self.total += len(chunk)
                complete.write(chunk)
                complete.flush()
                scan = previous + chunk
                markers = scan.count(b"panicked at")
                self.panic_markers += markers
                if markers:
                    self.records.finding("server_panic_log_marker", count=markers,
                                         log_context=scan.decode("utf-8", errors="replace"))
                previous = scan[-10:]
                if size + len(chunk) > 4 * 1024 * 1024:
                    stream.close()
                    self.path.replace(self.path.with_suffix(".previous.log"))
                    stream, size = self.path.open("wb"), 0
                stream.write(chunk)
                stream.flush()
                size += len(chunk)
        finally:
            stream.close()
            complete.close()
            self.pipe.close()


def specimen(tag, rng):
    """十种顶层类型和严格数值边界；Ten outer types and strict numeric boundaries."""
    scalars = [Value(0), Value(1, True), Value(2, -2**63), Value(2, 2**63 - 1), Value(3, 2**64 - 1),
               Value(3, 1), Value(4, 1.0), Value(4, float("inf")), Value(4, float("-inf")),
               Value(5, "中文\x00🙂"), Value(6, b"\x00\xff\x80")]
    if tag <= 6:
        return {0: Value(0), 1: Value(1, bool(rng.randrange(2))), 2: Value(2, 2**63 - 1),
                3: Value(3, 2**64 - 1), 4: Value(4, -0.0),
                5: Value(5, f"值-{rng.getrandbits(32)}-🙂"), 6: Value(6, rng.randbytes(32))}[tag]
    if tag == 7:
        return Value(7, scalars + [Value(8, {"nested": Value(7, [Value(2, -1), Value(3, 1)])})])
    if tag == 8:
        return Value(8, {"类型": Value(9, scalars), "nested": Value(7, scalars)})
    return Value(9, scalars + [Value(2, 1), Value(1, True), Value(4, -0.0)])


def check_missing(records, client, key):
    """Null 成功不能代替缺失错误；Successful Null must not stand in for missing-key errors."""
    ok, result = records.request(client, "GET", [key], allowed=("ERR_KEY_NOT_FOUND",))
    if ok or result != "ERR_KEY_NOT_FOUND":
        records.finding("expected_missing_key", key=key, actual=result)


def failed_unchanged(records, client, key, current, command, args, error):
    """失败后读取同一值，保留类型与内容比较。
    Read after rejection and compare both type and contents.
    """
    ok, result = records.request(client, command, args, allowed=(error,))
    if ok or result != error:
        records.finding("expected_rejection", command=command, arguments=args, expected=error, actual=result)
    records.request(client, "GET", [key], expected=current)


def observe_expired(records, client, key, stop):
    """只以服务端 PTTL=-2 判定到期，超时仅记观察未完成。
    Only server PTTL=-2 establishes expiry; a timeout records incomplete observation.
    """
    deadline = time.monotonic() + 3
    while not stop.is_set() and time.monotonic() < deadline:
        ok, result = records.request(client, "PTTL", [key])
        if ok and result == Value(2, -2):
            return True
        if stop.wait(.005):
            break
    if not stop.is_set():
        records.finding("ttl_observation_timeout", key=key, criterion="server PTTL=-2 not observed within tester's 3 s budget")
    return False


def ttl_cycle(records, client, prefix, stop):
    """短期到期、续期、删除重建分开检查旧任务不会删新值。
    Check short expiry, renewal and delete/recreate against stale expiry work.
    """
    key, marker = prefix + ":ttl", prefix + ":marker"
    first, replacement = Value(5, "old"), Value(6, b"replacement")
    ok, _ = records.request(client, "SET", [key, first, "PX", Value(2, 5)], allowed=RESOURCE_ERRORS)
    if not ok:
        return
    if observe_expired(records, client, key, stop):
        check_missing(records, client, key)
    for recreate in (False, True):
        ok, _ = records.request(client, "SET", [key, first, "PX", Value(2, 100)], allowed=RESOURCE_ERRORS)
        if not ok:
            return
        if recreate:
            ok, _ = records.request(client, "DEL", [key], allowed=RESOURCE_ERRORS)
            if not ok:
                return
            ok, _ = records.request(client, "SET", [key, replacement], expected=Value(1, True), allowed=RESOURCE_ERRORS)
            expected = replacement
        else:
            ok, renewed = records.request(client, "PEXPIRE", [key, Value(2, 600000)], allowed=RESOURCE_ERRORS)
            if ok and renewed == Value(1, False):
                records.count("ttl_renewal_missed_window")
                continue
            if ok and renewed != Value(1, True):
                records.finding("unexpected_ttl_renewal_result", actual=renewed)
            expected = first
        if not ok:
            return
        ok, _ = records.request(client, "SET", [marker, first, "PX", Value(2, 150)], allowed=RESOURCE_ERRORS)
        if ok and observe_expired(records, client, marker, stop):
            records.request(client, "GET", [key], expected=expected)
            if recreate:
                records.request(client, "PTTL", [key], expected=Value(2, -1))
    records.count("ttl_cycles")


def shadow_cycle(records, client, prefix, cycle, rng):
    """每周期显式初始化有界 Key，再检查类型、修改和失败原子性。
    Reinitialize bounded keys each cycle, then check types, mutations and rejected-write atomicity.
    """
    tag = cycle % 10
    key, current = f"{prefix}:value:{tag}", specimen(tag, rng)
    ok, _ = records.request(client, "SET", [key, current], expected=Value(1, True), allowed=RESOURCE_ERRORS)
    if not ok:
        records.count("shadow_cycles_resource_limited")
        return
    records.request(client, "GET", [key], expected=current)
    records.request(client, "TYPE", [key], expected=Value(5, TYPE_NAMES[tag]))
    if tag in (2, 3):
        failed_unchanged(records, client, key, current, "INCR", [key], "ERR_OVERFLOW")
    elif tag == 5:
        extra = "尾\x00"
        updated = Value(5, current.data + extra)
        ok, _ = records.request(client, "APPEND", [key, extra], expected=Value(3, len(updated.data.encode("utf-8"))), allowed=RESOURCE_ERRORS)
        if ok:
            current = updated
        failed_unchanged(records, client, key, current, "INCR", [key], "ERR_TYPE_MISMATCH")
    elif tag == 7:
        addition = Value(3, rng.getrandbits(64))
        updated = Value(7, current.data + (addition,))
        ok, _ = records.request(client, "RPUSH", [key, addition], expected=Value(3, len(updated.data)), allowed=RESOURCE_ERRORS)
        if ok:
            current = updated
        failed_unchanged(records, client, key, current, "LSET", [key, Value(2, 1000000), Value(0)], "ERR_INDEX_OUT_OF_RANGE")
    elif tag == 8:
        updated = Value(8, {**dict(current.data), "mutable": Value(2, -1)})
        ok, _ = records.request(client, "FSET", [key, "mutable", Value(2, -1)], expected=Value(3, 1), allowed=RESOURCE_ERRORS)
        if ok:
            current = updated
    elif tag == 9:
        additions = [Value(2, 1), Value(3, 1), Value(4, 1.0), Value(1, True), Value(3, rng.getrandbits(64))]
        updated = Value(9, tuple(current.data) + tuple(additions))
        ok, _ = records.request(client, "SADD", [key] + additions,
                                expected=Value(3, len(updated.data) - len(current.data)), allowed=RESOURCE_ERRORS)
        if ok:
            current = updated
    else:
        failed_unchanged(records, client, key, current, "RPUSH", [key, Value(0)], "ERR_TYPE_MISMATCH")
    records.request(client, "GET", [key], expected=current)
    records.count("shadow_cycles")
    records.count("type_" + TYPE_NAMES[tag])


def pressure_arguments(seed, worker_number, cycle, payload_bytes):
    """每个压力输入独立由种子与序号生成，容量错误可无损复现。
    Generate each pressure input independently from its seed and index for exact error reproduction.
    """
    rng = random.Random(f"{seed}/{worker_number}/{cycle}")
    slot = rng.randrange(32 if cycle % 4 else 2048)
    key = f"acceptance:worker:{worker_number}:db:{worker_number % 2}:pressure:{slot}"
    current = Value(6, struct.pack(">Q", cycle) + rng.randbytes(payload_bytes - 8))
    args = [key, current]
    if cycle % 3 == 0:
        args += ["PX", Value(2, 1000)]
    return args


def pressure_cycle(records, client, cycle, seed, worker_number, payload_bytes):
    """容量阶段允许淘汰缺失，仅在存在时比较本线程写入的内容。
    Capacity pressure permits eviction misses and compares surviving worker-owned values.
    """
    args = pressure_arguments(seed, worker_number, cycle, payload_bytes)
    key, current = args[:2]
    ok, _ = records.request(client, "SET", args, expected=Value(1, True), allowed=RESOURCE_ERRORS)
    if ok:
        found, result = records.request(client, "GET", [key], allowed=("ERR_KEY_NOT_FOUND",))
        if found and result != current:
            records.finding("pressure_surviving_value_mismatch", key=key, expected=current, actual=result)
        if not found:
            records.count("pressure_eviction_or_expiry_misses")
    records.count("pressure_cycles")


def worker(args, records, port, phase, number, stop):
    """固定数据库连接周期重建；传输失败后不悄悄恢复或重放。
    Recreate fixed-database connections periodically; never silently recover or replay after transport failure.
    """
    client = None
    rng = random.Random(args.seed + number * 1000003)
    database = number % 2
    prefix = f"acceptance:worker:{number}:db:{database}"
    cycle, next_cycle = 0, time.monotonic()
    try:
        client = Client(port, database)
        records.count("connections_opened")
        isolation_ready = False
        if phase == "shadow" and number < 2:
            isolation_ready, _ = records.request(client, "SET", ["acceptance:db-isolation", Value(3, database)],
                                                  expected=Value(1, True), allowed=RESOURCE_ERRORS)
        while not stop.is_set():
            records.context.value = {"phase": phase, "worker": number, "database": database,
                                     "cycle": cycle, "payload_bytes": args.payload_bytes}
            if phase == "shadow":
                shadow_cycle(records, client, prefix, cycle, rng)
                if isolation_ready and cycle % 10 == 0:
                    records.request(client, "GET", ["acceptance:db-isolation"], expected=Value(3, database))
                    records.count("database_isolation_checks")
                if cycle % 100 == 0:
                    ttl_cycle(records, client, prefix, stop)
            else:
                pressure_cycle(records, client, cycle, args.seed, number, args.payload_bytes)
            cycle += 1
            if cycle % args.churn_cycles == 0:
                client.close()
                client = Client(port, database)
                records.count("connections_opened")
                records.count("planned_reconnections")
            next_cycle += 1 / args.cycles_per_second
            if next_cycle < time.monotonic() - 1:
                next_cycle = time.monotonic()
            stop.wait(max(0, next_cycle - time.monotonic()))
        records.count("workers_requested_stop")
    except (OSError, WireError) as error:
        records.finding("worker_stopped_early", worker=number, cycle=cycle, error=str(error))
        records.count("workers_failed")
    except Exception as error:
        records.finding("tester_worker_exception", worker=number, cycle=cycle, error=repr(error))
        records.count("workers_failed")
    finally:
        if client is not None:
            client.close()


def sha256(path):
    """流式哈希避免加载整个二进制；Hash files without loading complete binaries."""
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def manifest(args, binary):
    """工具工作区身份与被测二进制来源分开记录。
    Record tool-workspace identity separately from tested-binary provenance.
    """
    tracked = capture(ROOT, "git", "ls-files")
    sources = {}
    for name in tracked.get("stdout", "").splitlines():
        path = ROOT / name
        if path.is_file() and (path.suffix in (".rs", ".toml", ".py") or name == "Cargo.lock"):
            sources[name] = sha256(path)
    result = {"date_utc": datetime.now(timezone.utc).isoformat(), "stage": args.stage,
              "mode": args.stage.replace("-", "_") + "_record_only",
              "tool_workspace_revision": capture(ROOT, "git", "rev-parse", "HEAD"),
              "tool_workspace_status": capture(ROOT, "git", "status", "--short"),
              "tool_sources_sha256": sources, "script_sha256": sha256(Path(__file__)),
              "binary": {"input_path": str(args.server_binary), "copied_path": str(binary),
                         "revision_asserted_by_operator": args.binary_revision,
                         "sha256": sha256(binary), "version": capture(ROOT, str(binary), "--version")},
              "platform": platform.platform(), "python": sys.version, "cpu_count": os.cpu_count(),
              "settings": {key: str(value) if isinstance(value, Path) else value for key, value in vars(args).items()},
              "scope": {"shadow": "separate noeviction service; strict values; no automatic repair",
                        "pressure": "fresh service per policy; eviction misses permitted",
                        "consumer": "fast and slow named groups in each database; slow planned reconnects; ACK/ID/liveness checked separately",
                        "crash_recovery": "not covered; run dedicated recovery acceptance separately",
                        "open_arrival_load": "not covered; workers use bounded closed-loop pacing",
                        "physical_power_failure": "not covered",
                        "cpu_placement": {"client": args.client_cpus, "server": args.server_cpus,
                                          "same_host": True}},
              "evidence_budget": {"resource_interval_seconds": args.sample_seconds,
                                  "resource_samples_limit": args.resource_samples,
                                  "finding_examples_limit": args.finding_examples,
                                  "all_errors": "gzip JSONL; capacity SET errors use exact deterministic input reference and SHA-256",
                                  "cycles_upper_bound_per_phase": math.ceil(args.seconds * args.workers * args.cycles_per_second)}}
    if sys.platform.startswith("linux"):
        result["environment"] = {name: capture(ROOT, *command) for name, command in {
            "cpu": ("lscpu",), "devices": ("lsblk", "-o", "NAME,MODEL,SIZE,TYPE,FSTYPE,MOUNTPOINTS"),
            "mounts": ("findmnt", "-J"), "memory": ("cat", "/proc/meminfo"),
            "virtualization": ("systemd-detect-virt",)}.items()}
        result["allowed_cpus"] = sorted(os.sched_getaffinity(0))
    elif sys.platform == "win32":
        result["environment"] = capture(ROOT, "powershell", "-NoProfile", "-Command",
            "[ordered]@{cpu=@(Get-CimInstance Win32_Processor | Select-Object Name,NumberOfCores,NumberOfLogicalProcessors);"
            "memory=@(Get-CimInstance Win32_ComputerSystem | Select-Object TotalPhysicalMemory);"
            "disks=@(Get-CimInstance Win32_DiskDrive | Select-Object Model,Size);"
            "volumes=@(Get-Volume | Select-Object DriveLetter,FileSystem,Size)} | ConvertTo-Json -Depth 4")
    result["device_write_cache"] = "not established by portable probes; no physical durability conclusion"
    return result


def configuration(args, port, metrics_port, mode, phase, policy):
    """影子配额容纳固定集合，压力配额刻意触及上限。
    Shadow quotas fit its bounded key set; pressure quotas intentionally reach capacity.
    """
    memory = 16 * 1024 * 1024 if phase == "shadow" else args.memory_bytes
    keys = 4096 if phase == "shadow" else args.max_keys
    config = f'''[server]
port={port}
max_connections={2 * args.workers + 8}
idle_timeout_ms=600000
request_timeout_ms=10000
max_request_bytes=1048576
max_response_bytes=1048576
[database]
databases=2
[engine]
shard_count=4
max_memory_bytes={memory}
max_keys={keys}
[eviction]
policy="{policy}"
[events]
mode="{mode}"
retention_days=1
buffer_bytes=4194304
max_pending_events=128
max_groups=32
consumer_timeout_ms=60000
retry_interval_ms=5000
[metrics]
enabled=true
port={metrics_port}
[log]
level="error"
'''
    if mode == "reliable":
        config += f'[event_journal]\ndir="journal"\nmax_bytes={args.journal_bytes}\n'
    return config


class Trends:
    """常量内存统计预热后的资源斜率，不保存每次采样到数组。
    Constant-memory post-warmup resource slopes avoid per-sample arrays.
    """
    def __init__(self, warmup):
        self.warmup, self.values = warmup, {}

    def add(self, elapsed, sample):
        """每字段独立记录可用性；Track availability independently for each field."""
        if elapsed < self.warmup:
            return
        x = elapsed - self.warmup
        for field in ("rss_bytes", "handles", "threads"):
            y = sample.get(field)
            if isinstance(y, (int, float)):
                row = self.values.setdefault(field, [0, 0.0, 0.0, 0.0, 0.0, x, x, y])
                row[0] += 1
                row[1] += x
                row[2] += y
                row[3] += x*x
                row[4] += x*y
                row[6], row[7] = x, max(row[7], y)

    def summary(self):
        """少于一分钟观测不推断稳态趋势。
        Do not infer steady-state trends from observations shorter than one minute.
        """
        output = {}
        for field, (n, sx, sy, sxx, sxy, start, end, peak) in self.values.items():
            denominator = n*sxx - sx*sx
            slope = (n*sxy - sx*sy) / denominator * 3600 if n >= 3 and end-start >= 60 and denominator > 0 else None
            output[field] = {"samples": n, "span_seconds": end-start, "peak": peak, "slope_per_hour": slope}
        return output


def resource_sample(records, observer, process, elapsed, metrics_port, directory):
    """原生资源、INFO、Prometheus 与 Journal 大小一起采样。
    Sample native resources, INFO, Prometheus and Journal size together.
    """
    row = {"elapsed_seconds": elapsed, "native": process_snapshot(process.pid)}
    ok, info = records.request(observer, "INFO", ["all"])
    row["info"] = evidence(info) if ok else {"error": info}
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{metrics_port}/metrics", timeout=2) as response:
            raw = response.read(1024 * 1024 + 1)
        row["prometheus"] = raw[:1024 * 1024].decode("utf-8", errors="replace")
        row["prometheus_truncated"] = len(raw) > 1024 * 1024
    except (OSError, ValueError) as error:
        row["prometheus_unavailable"] = str(error)
        records.finding("metrics_sample_failed", error=str(error))
    count, size = 0, 0
    for path in (directory / "journal").glob("**/*"):
        if path.is_file():
            count += 1
            if count > 4096:
                break
            try:
                size += path.stat().st_size
            except FileNotFoundError:
                row["journal_changed_during_sample"] = True
    row["journal"] = {"bytes": size, "files": min(count, 4096), "truncated": count > 4096}
    return row


def disk_space(directory):
    """WSL 同时观察虚拟文件系统和常见宿主卷的真实余量。
    In WSL, observe virtual filesystem space and the usual host volume's actual free space.
    """
    spaces = {"output_filesystem_free_bytes": shutil.disk_usage(directory).free}
    if "microsoft" in platform.release().lower() and Path("/mnt/c").is_dir():
        spaces["wsl_host_c_free_bytes"] = shutil.disk_usage("/mnt/c").free
    return spaces


def run_phase(args, binary, mode, phase, policy):
    """新服务隔离每个阶段，错误时仍保存部分进度和退出原因。
    Isolate each phase in a fresh server and retain partial progress on errors.
    """
    directory = args.output / f"{mode}-{phase}-{policy}"
    directory.mkdir()
    records = Records(directory, args.seed, args.finding_examples)
    stop, threads, observer, server, logger = threading.Event(), [], None, None, None
    consumers = []
    port, metrics_port = free_port(), free_port()
    while metrics_port == port:
        metrics_port = free_port()
    config = directory / "ling.toml"
    config.write_text(configuration(args, port, metrics_port, mode, phase, policy), encoding="utf-8")
    row = {"stage": args.stage, "mode": args.stage.replace("-", "_") + "_record_only",
           "event_mode": mode, "phase": phase, "policy": policy, "requested_seconds": args.seconds,
           "seed": args.seed, "status": "starting", "no_fix_policy": True, "config_sha256": sha256(config)}
    trends, sample_count, started = Trends(args.warmup_seconds), 0, None
    native_valid, native_missing, native_attempts = Counter(), Counter(), 0
    ending = None
    try:
        command = [str(binary), "--config", str(config)]
        if args.server_cpus:
            command = ["taskset", "-c", args.server_cpus] + command
        server = subprocess.Popen(command, cwd=directory,
                                  stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        logger = CappedLog(server.stdout, directory, records)
        deadline = time.monotonic() + 15
        while observer is None:
            try:
                observer = Client(port)
            except OSError:
                if server.poll() is not None or time.monotonic() >= deadline:
                    raise WireError("LOCAL_STARTUP: service unavailable")
                time.sleep(.02)
        row["hello"] = evidence(observer.hello)
        stream_id = dict(observer.hello.data)["stream_id"].data
        for database in range(2):
            for slow in (False, True):
                consumers.append(Consumer(args, records, port, mode, database, slow, stream_id, stop))
        for consumer in consumers:
            consumer.thread.start()
        started = time.monotonic()
        for number in range(args.workers):
            thread = threading.Thread(target=worker, args=(args, records, port, phase, number, stop), daemon=True)
            threads.append(thread)
            thread.start()
        next_sample, next_control = started, started + 60
        with gzip.open(directory / "resources.jsonl.gz", "wt", encoding="utf-8") as raw:
            while time.monotonic() - started < args.seconds:
                if server.poll() is not None:
                    records.finding("server_exited_early", exit_code=server.returncode)
                    break
                if all(not thread.is_alive() for thread in threads):
                    records.finding("all_workers_exited_early")
                    break
                now = time.monotonic()
                if now >= next_sample:
                    spaces = disk_space(directory)
                    if min(spaces.values()) < args.min_free_bytes:
                        records.finding("evidence_budget_exhausted", minimum_free_bytes=args.min_free_bytes, **spaces)
                        ending = "evidence_budget_exhausted"
                        break
                    sample = resource_sample(records, observer, server, now-started, metrics_port, directory)
                    sample["disk"] = spaces
                    native_attempts += 1
                    for field in ("cpu_seconds", "rss_bytes", "handles", "threads"):
                        (native_valid if type(sample["native"].get(field)) in (int, float) else native_missing)[field] += 1
                    sample["consumers"] = {consumer.name: consumer.snapshot() for consumer in consumers}
                    trends.add(now-started, sample["native"])
                    if sample_count < args.resource_samples:
                        raw.write(json.dumps(sample, ensure_ascii=False, allow_nan=False) + "\n")
                        raw.flush()
                        sample_count += 1
                    else:
                        records.count("resource_samples_omitted")
                    next_sample = now + args.sample_seconds
                    write_json(directory / "progress.json", {**row, **records.snapshot(), "elapsed_seconds": now-started,
                                                             "consumers": sample["consumers"]})
                if now >= next_control:
                    records.request(observer, "CONFIG", ["RELOAD"], expected=Value(1, True))
                    records.count("config_reload_attempts")
                    if phase == "pressure":
                        records.request(observer, "SELECT", [Value(3, int((now-started)//60) % 2)], expected=Value(1, True))
                        records.request(observer, "FLUSHDB", expected=Value(1, True), allowed=RESOURCE_ERRORS)
                        records.count("flushdb_attempts")
                    next_control = now + 60
                stop.wait(min(.1, max(0, args.seconds - (time.monotonic()-started))))
        row["status"] = ending or ("duration_reached" if time.monotonic()-started >= args.seconds else "ended_early")
    except KeyboardInterrupt:
        row["status"] = "interrupted"
    except (OSError, WireError) as error:
        records.finding("phase_failure", error=str(error))
        row["status"] = "failed"
    finally:
        stop.set()
        for thread in threads:
            thread.join(timeout=12)
            if thread.is_alive():
                records.finding("worker_shutdown_timeout", thread=thread.name)
        for consumer in consumers:
            if consumer.thread.ident is not None:
                consumer.thread.join(timeout=12)
                if consumer.thread.is_alive():
                    records.finding("consumer_shutdown_timeout", consumer=consumer.name)
            else:
                consumer.client.close()
        if started is not None:
            row["elapsed_seconds"] = time.monotonic() - started
        if observer is not None and server is not None and server.poll() is None:
            try:
                ok, final_info = records.request(observer, "INFO", ["all"])
                row["final_info"] = evidence(final_info) if ok else {"error": final_info}
            except (OSError, WireError):
                pass
        if server is not None:
            if observer is not None and server.poll() is None:
                try:
                    records.request(observer, "SHUTDOWN", expected=Value(1, True))
                    row["shutdown_method"] = "SHUTDOWN request"
                    server.wait(timeout=10)
                except (OSError, WireError, subprocess.TimeoutExpired) as error:
                    row["shutdown_request_error"] = str(error)
            if server.poll() is None:
                server.terminate()
                row["shutdown_method"] = "terminate fallback"
                try:
                    server.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    server.kill()
                    server.wait(timeout=10)
                    records.finding("server_shutdown_timeout")
            row["server_exit_code"] = server.returncode
        if observer is not None:
            observer.close()
        if logger is not None:
            logger.thread.join(timeout=3)
            row["server_log_bytes_observed"] = logger.total
            row["server_log_readable_tail_limit_bytes"] = 8 * 1024 * 1024
            row["server_log_complete_gzip_bytes"] = (directory / "server-complete.log.gz").stat().st_size
            row["server_log_capture_complete"] = not logger.thread.is_alive()
            if logger.thread.is_alive():
                records.finding("server_log_shutdown_timeout")
            row["server_panic_markers"] = logger.panic_markers
        row.update(records.snapshot())
        row["resource_trends"] = trends.summary()
        row["resource_samples_saved"] = sample_count
        row["resource_samples_limit"] = args.resource_samples
        row["native_resources"] = {"attempts": native_attempts, "valid": dict(native_valid), "missing": dict(native_missing)}
        row["consumers"] = {consumer.name: consumer.snapshot() for consumer in consumers}
        row["consumer_acceptance"] = consumer_acceptance(mode, row["consumers"])
        row["workload_degraded_by_resource_limit"] = any(code in row["server_errors"] for code in RESOURCE_ERRORS)
        row["criteria"] = {"no_findings": not row["findings"], "full_duration": row["status"] == "duration_reached",
                           "all_workers_completed": row["counts"].get("workers_requested_stop", 0) == args.workers,
                           "all_ten_types_checked": all(row["counts"].get("type_" + name, 0) for name in TYPE_NAMES) if phase == "shadow" else None,
                           "shadow_writes_not_capacity_limited": not row["counts"].get("shadow_cycles_resource_limited", 0) if phase == "shadow" else None,
                           "consumers_valid": row["consumer_acceptance"]["valid"],
                           "native_resources_available": native_attempts > 0 and all(
                               native_valid[field] == native_attempts for field in
                               (("cpu_seconds", "rss_bytes", "handles", "threads") if sys.platform.startswith("linux")
                                else ("cpu_seconds", "rss_bytes", "handles"))),
                           "resource_trends": "recorded; compare against the separately committed acceptance plan"}
        write_json(directory / "result.json", row)
        records.close()
    return row


def parser():
    """所有长测预算在执行前显式保存；Save all endurance budgets before execution."""
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("output", type=Path, help="fresh output directory (must not exist)")
    result.add_argument("--server-binary", type=Path, required=True)
    result.add_argument("--binary-revision", required=True, help="exact tested commit/tag asserted by operator; never inferred from tool HEAD")
    result.add_argument("--stage", choices=("pre-release", "post-release"), default="post-release",
                        help="both stages record findings; pre-release blockers require review, post-release findings are recorded only")
    result.add_argument("--event-mode", choices=("fast", "reliable", "both"), default="both")
    result.add_argument("--phase", choices=("all", "shadow", "pressure"), default="all")
    result.add_argument("--policy", choices=("all",) + POLICIES, default="all")
    result.add_argument("--seconds", type=float, default=600, help="duration of EACH fresh phase; supports multi-day runs")
    result.add_argument("--seed", type=int, default=3002026)
    result.add_argument("--workers", type=int, default=4)
    result.add_argument("--cycles-per-second", type=float, default=20, help="bounded closed-loop cycles per worker per second")
    result.add_argument("--churn-cycles", type=int, default=100)
    result.add_argument("--payload-bytes", type=int, default=4096)
    result.add_argument("--memory-bytes", type=int, default=2 * 1024 * 1024)
    result.add_argument("--max-keys", type=int, default=256)
    result.add_argument("--journal-bytes", type=int, default=1024 * 1024 * 1024)
    result.add_argument("--sample-seconds", type=float, default=5)
    result.add_argument("--warmup-seconds", type=float, default=30)
    result.add_argument("--finding-examples", type=int, default=200)
    result.add_argument("--resource-samples", type=int, default=100000)
    result.add_argument("--slow-consumer-delay", type=float, default=.05, help="seconds before each slow-consumer ACK")
    result.add_argument("--consumer-reconnect-seconds", type=float, default=120)
    result.add_argument("--consumer-disconnect-seconds", type=float, default=.2)
    result.add_argument("--min-free-bytes", type=int, default=10 * 1024 * 1024 * 1024,
                        help="stop without discarding evidence below this free-space reserve; includes /mnt/c on WSL")
    result.add_argument("--client-cpus", help="Linux CPU list, disjoint from --server-cpus")
    result.add_argument("--server-cpus", help="Linux CPU list applied before server starts")
    return result


def cpu_set(specification, available):
    """执行前检查 CPU 范围，拒绝不可用或空集合。
    Validate CPU ranges before execution and reject unavailable or empty sets.
    """
    cpus = set()
    for token in specification.split(","):
        bounds = token.split("-")
        if not 1 <= len(bounds) <= 2 or any(not part.isdecimal() for part in bounds):
            raise ValueError("invalid CPU list")
        low, high = int(bounds[0]), int(bounds[-1])
        if high < low or high > max(available):
            raise ValueError("invalid CPU range")
        cpus.update(range(low, high+1))
    if not cpus or not cpus <= available:
        raise ValueError("CPU list outside allowed affinity")
    return cpus


def main():
    """保存环境后顺序运行；遇到发现继续独立阶段，最终以非零退出提示检查。
    Save provenance, run independent phases, and return nonzero when findings require review.
    """
    cli = parser()
    args = cli.parse_args()
    args.output, args.server_binary = args.output.resolve(), args.server_binary.resolve()
    if (not args.server_binary.is_file() or args.seconds <= 0 or not math.isfinite(args.seconds)
            or not 2 <= args.workers <= 16 or not .01 <= args.cycles_per_second <= 10000
            or args.churn_cycles < 1 or not 8 <= args.payload_bytes <= 262144
            or args.memory_bytes < 1024 * 1024 or args.max_keys < 1 or args.journal_bytes < 1024 * 1024
            or not .1 <= args.sample_seconds <= 3600 or args.warmup_seconds < 0 or not math.isfinite(args.warmup_seconds)
            or not 1 <= args.finding_examples <= 10000 or not 1 <= args.resource_samples <= 1000000
            or not 0 <= args.slow_consumer_delay <= 1 or not 1 <= args.consumer_reconnect_seconds <= 86400
            or not 0 <= args.consumer_disconnect_seconds <= 60 or args.min_free_bytes < 0):
        cli.error("invalid input file, duration, worker count or resource budget")
    client_cpus = None
    if args.client_cpus or args.server_cpus:
        if not sys.platform.startswith("linux") or not args.client_cpus or not args.server_cpus or not shutil.which("taskset"):
            cli.error("CPU placement requires Linux taskset and both disjoint CPU lists")
        try:
            available = set(os.sched_getaffinity(0))
            client_cpus = cpu_set(args.client_cpus, available)
            server_cpus = cpu_set(args.server_cpus, available)
            if client_cpus & server_cpus:
                raise ValueError("client and server CPU lists overlap")
        except ValueError as error:
            cli.error(str(error))
    args.output.mkdir(parents=True, exist_ok=False)
    binary = args.output / ("ling.exe" if sys.platform == "win32" else "ling")
    shutil.copy2(args.server_binary, binary)
    source = args.output / "tool_source"
    source.mkdir()
    for name in ("v030.py", "v020.py"):
        shutil.copy2(Path(__file__).with_name(name), source / name)
    write_json(args.output / "environment.json", manifest(args, binary))
    if client_cpus:
        os.sched_setaffinity(0, client_cpus)
    modes = ("fast", "reliable") if args.event_mode == "both" else (args.event_mode,)
    policies = POLICIES if args.policy == "all" else (args.policy,)
    rows = []
    for mode in modes:
        if mode == "reliable" and sys.platform == "win32":
            rows.append({"stage": args.stage, "mode": args.stage.replace("-", "_") + "_record_only",
                         "event_mode": mode, "status": "skipped", "reason": "Windows Reliable is unsupported; not accepted as a pass"})
            continue
        phases = [("shadow", "noeviction")] if args.phase in ("all", "shadow") else []
        if args.phase in ("all", "pressure"):
            phases.extend(("pressure", policy) for policy in policies)
        for phase, policy in phases:
            row = run_phase(args, binary, mode, phase, policy)
            rows.append(row)
            write_json(args.output / "summary.json", {"stage": args.stage,
                       "mode": args.stage.replace("-", "_") + "_record_only", "runs": rows})
            print(json.dumps({"event_mode": mode, "phase": phase, "policy": policy, "status": row["status"],
                              "findings": row["findings"]}, ensure_ascii=False), flush=True)
            if row["status"] == "interrupted":
                return 130
            if row["status"] == "evidence_budget_exhausted":
                return 1
    write_json(args.output / "summary.json", {"stage": args.stage,
               "mode": args.stage.replace("-", "_") + "_record_only", "runs": rows})
    return int(any(row.get("findings") or row["status"] not in ("duration_reached", "skipped")
                   or any(value is False for value in row.get("criteria", {}).values()) for row in rows))


if __name__ == "__main__":
    raise SystemExit(main())
