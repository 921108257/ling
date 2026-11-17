#!/usr/bin/env python3
"""固定到达率的有界开放负载；Bounded open-arrival load against an isolated running server.

Example: python3 benchmarks/open_load.py --port 16480 --output /tmp/open-load-new
  --baseline-rate 100 --overload-rate 5000 --recovery-rate 100

主调度不等待响应；连接内串行，无写入重试。队列满是 tester_dropped，不是服务端拒绝。
The scheduler never waits for responses; connections are serial and writes are never retried.
One same-machine Python client, its CPU/GIL, logging, fixed connections and finite queue limit
the generated load. This is not unlimited lossless arrival, remote-client or production evidence.
本机 Python/GIL、记录开销、连接和队列限制均会影响结果；不能称为无限无损到达。
Only connects to --port on 127.0.0.1; the caller owns server isolation and configuration.
只连接已启动的专用服务；调用者负责服务隔离。所有单调时间都是本次负载进程的时钟。
"""
import argparse
from collections import Counter, deque
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import queue
import re
import shutil
import socket
import sys
import threading
import time

from v020 import Client, WireError, capture

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
MAX_ARRIVALS = 2_000_000
LIMITATIONS = "主调度" + __doc__.split("主调度", 1)[1]


@dataclass(frozen=True)
class Phase:
    """到达率绑定计划时间，不随响应调整；Arrival rates bind to planned time, not responses."""
    name: str
    seconds: float
    rate: float


@dataclass(frozen=True)
class Job:
    """每个计划到达的身份与原始时钟；Identity and raw clocks for one scheduled arrival."""
    request: int
    phase: str
    command: str
    deadline_ns: int
    arrival_ns: int
    queued: int


def json_default(value):
    """HELLO 字节身份保持精度与可读性；Keep HELLO byte identities lossless and readable."""
    if isinstance(value, bytes):
        return {"hex": value.hex()}
    raise TypeError(f"unsupported evidence value: {type(value).__name__}")


class Histogram:
    """有界内存的微秒上界桶；Bounded microsecond upper-bound buckets, at most 1/64 relative width."""
    def __init__(self):
        self.buckets = Counter()
        self.count = 0
        self.minimum = None
        self.maximum = None

    def add(self, nanoseconds):
        """不保存无界样本；Retain a bounded histogram instead of an unbounded sample array."""
        value = max(0, math.ceil(nanoseconds / 1000))
        shift = max(0, value.bit_length() - 7)
        upper = ((value >> shift) + 1) * (1 << shift) - 1
        self.buckets[upper] += 1
        self.count += 1
        self.minimum = value if self.minimum is None else min(self.minimum, value)
        self.maximum = value if self.maximum is None else max(self.maximum, value)

    def result(self):
        """百分位取最近秩所在桶上界；Nearest-rank percentiles use their bucket's upper bound."""
        result = {"count": self.count, "min_us": self.minimum, "max_us": self.maximum}
        items = sorted(self.buckets.items())
        for label, percentile in (("p50_us", .5), ("p95_us", .95), ("p99_us", .99), ("p999_us", .999)):
            rank, seen, answer = math.ceil(self.count * percentile), 0, None
            for upper, count in items:
                seen += count
                if seen >= rank:
                    answer = upper
                    break
            result[label] = answer
        return result


def classify(error, command, stage="request", timed_out=False):
    """错误来源不混用；Separate server replies, local transport, protocol and unsent failures."""
    message = str(error)
    code = message.split(":", 1)[0]
    if timed_out or isinstance(error, (TimeoutError, socket.timeout)):
        category, code = "client_timeout", "LOCAL_TIMEOUT"
    elif isinstance(error, WireError) and re.fullmatch(r"ERR_[A-Z0-9_]+", code):
        category = "server_error" if stage == "request" else "server_connect_error"
    elif isinstance(error, WireError) and code in ("LOCAL_PROTOCOL", "LOCAL_ENCODING", "LOCAL_RESPONSE"):
        category = "client_protocol_error"
    elif isinstance(error, (OSError, WireError)):
        category = "client_transport_error"
        if not code.startswith("LOCAL_"):
            code = "LOCAL_TRANSPORT"
    else:
        category, code = "tester_error", "LOCAL_TESTER"
    unknown = stage == "request" and command == "SET" and (
        category not in ("server_error",) or code == "ERR_COMMIT_UNKNOWN")
    return {"outcome": category, "error_code": code, "error": message[:512],
            "stage": stage, "result_unknown": unknown}


class Evidence:
    """串行化原始证据与有界汇总；Serialize raw evidence and bounded summaries under one lock."""
    def __init__(self, path, phases):
        self.path = path
        self.stream = path.open("x", encoding="utf-8", buffering=1024 * 1024)
        self.lock = threading.Lock()
        self.rows = {phase.name: {"counts": Counter(), "errors": Counter(), "latencies": {}}
                     for phase in phases}
        self.maximum_queued = 0

    def write(self, event, **fields):
        """记录缓冲到磁盘，阶段和结束时刷新；Buffer raw events, flush at phase boundaries and shutdown."""
        with self.lock:
            self.stream.write(json.dumps({"event": event, **fields}, ensure_ascii=False, default=json_default) + "\n")

    def arrival(self, job, accepted):
        """测试端丢弃永不记为服务器错误；Tester drops never count as server rejections."""
        with self.lock:
            row = self.rows[job.phase]
            row["counts"]["scheduled"] += 1
            row["counts"][f"scheduled_{job.command}"] += 1
            row["counts"]["enqueued" if accepted else "tester_dropped"] += 1
            self.maximum_queued = max(self.maximum_queued, job.queued)
            row["latencies"].setdefault("scheduler_lag", Histogram()).add(job.arrival_ns - job.deadline_ns)
            self.stream.write(json.dumps({"event": "arrival", **asdict(job), "accepted": accepted,
                                          "outcome": "enqueued" if accepted else "tester_dropped"}) + "\n")

    def complete(self, job, dispatch_ns, completion_ns, outcome, **fields):
        """包含计划到达至完成，避免隐藏排队；Include planned-arrival-to-completion latency."""
        with self.lock:
            row = self.rows[job.phase]
            row["counts"][outcome] += 1
            if fields.get("error_code"):
                row["errors"][f"{outcome}:{fields['error_code']}"] += 1
            durations = {"scheduled_to_completion": completion_ns - job.deadline_ns}
            if dispatch_ns is not None:
                row["counts"]["dispatched"] += 1
                durations.update(queue_wait=dispatch_ns - job.arrival_ns,
                                 dispatch_to_completion=completion_ns - dispatch_ns)
            else:
                durations["cancelled_queue_wait"] = completion_ns - job.arrival_ns
            if fields.get("request_start_ns") is not None:
                row["counts"]["request_started"] += 1
                durations["request_to_completion"] = completion_ns - fields["request_start_ns"]
            if outcome == "success":
                durations["success_scheduled_to_completion"] = completion_ns - job.deadline_ns
            for name, value in durations.items():
                row["latencies"].setdefault(name, Histogram()).add(value)
            self.stream.write(json.dumps({"event": "completion", **asdict(job), "dispatch_ns": dispatch_ns,
                                          "completion_ns": completion_ns, "outcome": outcome, **fields}) + "\n")

    def flush(self):
        """显式刷新阶段证据；Explicitly flush stage evidence without per-request fsync skew."""
        with self.lock:
            self.stream.flush()

    def finish(self):
        """无线程再写后关闭并保存分布；Close after writers stop and return histogram summaries."""
        with self.lock:
            self.stream.flush()
            os.fsync(self.stream.fileno())
            self.stream.close()
            return {name: {"counts": dict(row["counts"]), "errors": dict(row["errors"]),
                           "latencies": {key: value.result() for key, value in row["latencies"].items()}}
                    for name, row in self.rows.items()}


class BoundedClient(Client):
    """复用 v020 协议，注册套接字供总期限取消；Reuse v020 with socket registration for whole-call deadlines."""
    def __init__(self, port, timeout, register):
        self.stream = socket.create_connection(("127.0.0.1", port), timeout=timeout)
        self.stream.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.stream.settimeout(timeout)
        self.request_id, self.events, self.subscribed = 0, deque(), False
        register(self)
        try:
            self.hello = self.request("HELLO", [1])
        except BaseException:
            self.close()
            raise


def close_client(client):
    """shutdown 唤醒阻塞读写，只关闭本工具的连接；Wake blocked I/O on this tool's own socket."""
    if client is not None:
        try:
            client.stream.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        client.close()


class Pool:
    """固定工作线程，每连接串行；Fixed workers each own at most one serial connection."""
    def __init__(self, port, connections, capacity, request_timeout, evidence, key_count, value_bytes,
                 client_factory=BoundedClient):
        self.port, self.request_timeout, self.evidence = port, request_timeout, evidence
        self.key_count, self.value = key_count, bytes(value_bytes)
        self.factory = client_factory
        self.jobs = queue.Queue(capacity)
        self.stop = threading.Event()
        self.monitor_stop = threading.Event()
        self.lock = threading.Lock()
        self.states = [{"client": None, "deadline": None, "timed_out": False, "ready": threading.Event(),
                        "setup_error": None} for _ in range(connections)]
        self.threads = [threading.Thread(target=self.worker, args=(index,), name=f"open-load-{index}", daemon=True)
                        for index in range(connections)]
        self.monitor = threading.Thread(target=self.watchdog, name="open-load-watchdog", daemon=True)

    def register(self, state, client):
        """连接建立后立即暴露给期限监视；Expose connected sockets to deadline cancellation immediately."""
        with self.lock:
            state["client"] = client
            if state["timed_out"] or self.stop.is_set():
                close_client(client)

    def begin(self, state):
        """期限覆盖建连、HELLO 与一条请求；A whole-call deadline includes connect, HELLO and one request."""
        with self.lock:
            state["deadline"] = time.monotonic_ns() + int(self.request_timeout * 1e9)
            state["timed_out"] = False

    def connect(self, state):
        """重建连接仅用于下一请求，不重放原请求；Reconnect for a later request, never replay the failed one."""
        return self.factory(self.port, self.request_timeout, lambda client: self.register(state, client))

    def watchdog(self):
        """滴流响应也不能无限延长调用；Even a trickling response cannot extend the whole-call deadline."""
        while not self.monitor_stop.wait(.01):
            with self.lock:
                now = time.monotonic_ns()
                for state in self.states:
                    if state["deadline"] is not None and now >= state["deadline"]:
                        state["timed_out"] = True
                        state["deadline"] = None
                        # 与 begin/register 同锁，旧期限不能关闭下一次调用复用的 socket。
                        # Serialize expiry with begin/register so an old deadline cannot close a new call.
                        close_client(state["client"])

    def worker(self, index):
        """只从有界队列取任务，失败请求不重试；Consume a bounded queue without retrying failed requests."""
        state = self.states[index]
        client = None
        try:
            self.begin(state)
            try:
                client = self.connect(state)
                self.evidence.write("connection_ready", worker=index, monotonic_ns=time.monotonic_ns(),
                                    hello=client.hello)
            except Exception as error:
                state["setup_error"] = classify(error, "HELLO", "connect", state["timed_out"])
            finally:
                with self.lock:
                    state["deadline"] = None
                state["ready"].set()
            if state["setup_error"] is not None:
                return
            while not self.stop.is_set():
                try:
                    job = self.jobs.get(timeout=.05)
                except queue.Empty:
                    continue
                if self.stop.is_set():
                    self.evidence.complete(job, None, time.monotonic_ns(), "tester_cancelled", worker=index,
                                           error_code="LOCAL_DRAIN_DEADLINE", result_unknown=False)
                    self.jobs.task_done()
                    continue
                dispatch = time.monotonic_ns()
                queued_at_dispatch = self.jobs.qsize()
                stage, sent_ns = "connect", None
                self.begin(state)
                try:
                    if client is None:
                        client = self.connect(state)
                        self.evidence.write("connection_replaced", worker=index, monotonic_ns=time.monotonic_ns())
                    stage, sent_ns = "request", time.monotonic_ns()
                    identity = job.request.to_bytes(8, "little")
                    value = (identity + self.value)[:len(self.value)]
                    arguments = () if job.command == "PING" else (
                        f"open-load:{(job.request // 2) % self.key_count:08d}", value)
                    response = client.request(job.command, arguments)
                    with self.lock:
                        expired = state["timed_out"] or (state["deadline"] is not None
                                                        and time.monotonic_ns() >= state["deadline"])
                        state["timed_out"] = expired
                    if expired:
                        raise TimeoutError("whole-call request deadline exceeded")
                    if (job.command == "PING" and response != "PONG") or (
                            job.command == "SET" and response is not True):
                        raise WireError("LOCAL_RESPONSE: unexpected successful response value")
                    self.evidence.complete(job, dispatch, time.monotonic_ns(), "success", worker=index,
                                           request_start_ns=sent_ns, queued_at_dispatch=queued_at_dispatch,
                                           error_code=None, result_unknown=False)
                except Exception as error:
                    failure = classify(error, job.command, stage, state["timed_out"])
                    if self.stop.is_set():
                        failure.update(outcome="tester_cancelled", error_code="LOCAL_DRAIN_DEADLINE",
                                       result_unknown=sent_ns is not None and job.command == "SET")
                    self.evidence.complete(job, dispatch, time.monotonic_ns(), worker=index,
                                           request_start_ns=sent_ns, queued_at_dispatch=queued_at_dispatch, **failure)
                    # 共享解码器不保留 ERROR/RESPONSE 差异，错误后统一换连接，绝不重放原请求。
                    # The shared decoder omits fatal-envelope identity; replace on any error, never replay.
                    close_client(client)
                    client = None
                    self.register(state, None)
                finally:
                    with self.lock:
                        state["deadline"] = None
                    self.jobs.task_done()
        except Exception as error:
            self.evidence.write("worker_failure", worker=index, error=repr(error), monotonic_ns=time.monotonic_ns())
            state["setup_error"] = {"outcome": "tester_error", "error": repr(error)}
        finally:
            close_client(client)
            with self.lock:
                state["deadline"] = None
                state["client"] = None
            state["ready"].set()

    def start(self):
        """全部连接预先建立，失败则不计负载；Establish every connection before counting any load."""
        self.monitor.start()
        for worker in self.threads:
            worker.start()
        deadline = time.monotonic() + self.request_timeout + 1
        for state in self.states:
            state["ready"].wait(max(0, deadline - time.monotonic()))
        failures = [state["setup_error"] or {"outcome": "client_timeout"} for state in self.states
                    if state["setup_error"] is not None or not state["ready"].is_set()]
        if failures:
            raise RuntimeError(f"connection setup failed: {failures}")

    def finish(self, drain_seconds):
        """有界排空，剩余任务明确取消，所有线程共享退出期限；Bound drain and thread shutdown, recording cancellations."""
        deadline = time.monotonic() + drain_seconds
        while self.jobs.unfinished_tasks and time.monotonic() < deadline:
            self.stop.wait(.01)
        self.stop.set()
        with self.lock:
            clients = [state["client"] for state in self.states]
        for client in clients:
            close_client(client)
        deadline = time.monotonic() + self.request_timeout + 1
        for worker in self.threads:
            if worker.ident is not None:
                worker.join(max(0, deadline - time.monotonic()))
        self.monitor_stop.set()
        if self.monitor.ident is not None:
            self.monitor.join(1)
        while True:
            try:
                job = self.jobs.get_nowait()
            except queue.Empty:
                break
            self.evidence.complete(job, None, time.monotonic_ns(), "tester_cancelled",
                                   error_code="LOCAL_DRAIN_DEADLINE", result_unknown=False)
            self.jobs.task_done()
        return [worker.name for worker in [*self.threads, self.monitor] if worker.is_alive()]


def schedule(phases, jobs, evidence, clock=time.monotonic_ns, sleep=time.sleep, start_ns=None):
    """固定单调期限；迟到仍按原期限追赶，不因响应减速而降率。
    Keep anchored deadlines and account for late catch-up arrivals without response-driven rate reduction.
    """
    start_ns = clock() if start_ns is None else start_ns
    cursor, request, boundaries = start_ns, 0, []
    for phase in phases:
        end = cursor + round(phase.seconds * 1e9)
        boundaries.append({**asdict(phase), "start_ns": cursor, "end_ns": end})
        evidence.write("phase_start", **boundaries[-1], actual_ns=clock())
        index = 0
        while (deadline := cursor + round(index * 1e9 / phase.rate)) < end:
            while clock() < deadline:
                sleep(max(0, (deadline - clock()) / 1e9))
            arrival = clock()
            job = Job(request, phase.name, "PING" if request % 2 == 0 else "SET", deadline, arrival,
                      min(jobs.maxsize, jobs.qsize() + 1))
            accepted = True
            try:
                jobs.put_nowait(job)
            except queue.Full:
                accepted = False
            evidence.arrival(job, accepted)
            request, index = request + 1, index + 1
        while clock() < end:
            sleep(max(0, (end - clock()) / 1e9))
        evidence.write("phase_end", phase=phase.name, planned_ns=end, actual_ns=clock())
        evidence.flush()
        cursor = end
    return {"start_ns": start_ns, "scheduled_end_ns": cursor, "actual_end_ns": clock(), "phases": boundaries}


def recovery_probe(port, timeout, interval, threshold_ms, consecutive, evidence, arrivals_end_ns,
                   overload_end_ns, actual_arrivals_end_ns=None, client_factory=BoundedClient):
    """负载连接退出后，逐次新连接 PING，稳定成功才算恢复；Probe fresh serial connections after load workers exit."""
    until, streak, attempts = time.monotonic() + timeout, 0, 0
    stable_start = None
    latencies = Histogram()
    while time.monotonic() < until:
        start = time.monotonic_ns()
        client = None
        # 每次探测总期限独立；timer 只用于最多几十次的结束探测。
        # A whole-probe watchdog is used only for the small number of final probes.
        active, active_lock, expired = [], threading.Lock(), threading.Event()
        def register(candidate):
            """计时器触发后的新套接字立即关闭；Close a socket registered after watchdog expiry immediately."""
            with active_lock:
                active.append(candidate)
                if expired.is_set():
                    close_client(candidate)
        def cancel():
            """关闭本次探测套接字；Cancel only this probe's socket."""
            with active_lock:
                expired.set()
                for candidate in active:
                    close_client(candidate)
        remaining = max(.001, until - time.monotonic())
        timer = threading.Timer(min(1, remaining), cancel)
        timer.daemon = True
        timer.start()
        failure, stage = {}, "connect"
        try:
            client = client_factory(port, min(1, remaining), register)
            stage = "request"
            if client.request("PING") != "PONG":
                raise WireError("LOCAL_RESPONSE: recovery PING was not PONG")
            end = time.monotonic_ns()
            healthy = not expired.is_set() and end - start <= threshold_ms * 1e6
        except Exception as error:
            end, healthy = time.monotonic_ns(), False
            failure = classify(error, "PING", stage, expired.is_set())
        finally:
            timer.cancel()
            timer.join(1)
            close_client(client)
        attempts += 1
        latencies.add(end - start)
        if healthy:
            stable_start = end if streak == 0 else stable_start
            streak += 1
        else:
            streak, stable_start = 0, None
        evidence.write("recovery_ping", start_ns=start, completion_ns=end, healthy=healthy,
                       consecutive=streak, **(failure or {"outcome": "success" if healthy else "slow_success",
                                                         "error_code": None, "stage": "request"}))
        if streak >= consecutive:
            return {"recovered": True, "attempts": attempts, "stable_first_completion_ns": stable_start,
                    "confirmed_ns": end, "from_scheduled_arrivals_end_seconds": (end - arrivals_end_ns) / 1e9,
                    "from_actual_arrivals_end_seconds": (end - (actual_arrivals_end_ns or arrivals_end_ns)) / 1e9,
                    "from_overload_end_seconds": (end - overload_end_ns) / 1e9,
                    "includes_drain": True, "threshold_ms_including_connect_hello": threshold_ms,
                    "consecutive_required": consecutive, "latency": latencies.result()}
        time.sleep(min(interval, max(0, until - time.monotonic())))
    return {"recovered": False, "attempts": attempts, "timeout_seconds": timeout,
            "from_scheduled_arrivals_end_seconds": (time.monotonic_ns() - arrivals_end_ns) / 1e9,
            "from_actual_arrivals_end_seconds": (time.monotonic_ns() - (actual_arrivals_end_ns or arrivals_end_ns)) / 1e9,
            "from_overload_end_seconds": (time.monotonic_ns() - overload_end_ns) / 1e9,
            "includes_drain": True, "consecutive_required": consecutive, "latency": latencies.result()}


def write_json(path, value):
    """只写新运行目录中的文件；Write readable JSON inside the new run directory."""
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def snapshot(output, binary=None, config=None):
    """保留工具与可选受测文件副本；Retain tool sources and optional caller-supplied server inputs."""
    directory = output / "inputs"
    directory.mkdir()
    result = {}
    for label, source in (("controller", Path(__file__)), ("protocol_helper", Path(sys.modules[Client.__module__].__file__)),
                          ("tests", Path(__file__).with_name("test_open_load.py")),
                          ("binary", binary), ("config", config)):
        if source is None:
            continue
        source = Path(source).resolve(strict=True)
        target = directory / f"{label}-{source.name}"
        shutil.copyfile(source, target)
        with target.open("rb") as copied:
            digest = hashlib.file_digest(copied, "sha256").hexdigest()
        result[label] = {"original_path": str(source), "copy": str(target.relative_to(output)), "sha256": digest}
    return result


def main(argv=None):
    """三段计划与新目录先固定；Freeze the three-phase plan and fresh output before opening connections."""
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--connections", type=int, default=32)
    parser.add_argument("--queue-capacity", type=int, default=1024)
    parser.add_argument("--request-timeout", type=float, default=5)
    parser.add_argument("--drain-seconds", type=float, default=10)
    parser.add_argument("--probe-seconds", type=float, default=30)
    parser.add_argument("--probe-interval", type=float, default=.1)
    parser.add_argument("--recovery-threshold-ms", type=float, default=100)
    parser.add_argument("--recovery-consecutive", type=int, default=3)
    parser.add_argument("--keys", type=int, default=1024)
    parser.add_argument("--value-bytes", type=int, default=64)
    parser.add_argument("--binary", type=Path, help="optional evidence copy; does not start a server")
    parser.add_argument("--config", type=Path, help="optional evidence copy; does not configure a running server")
    for phase, rate in (("baseline", 100), ("overload", 5000), ("recovery", 100)):
        parser.add_argument(f"--{phase}-seconds", type=float, default=60)
        parser.add_argument(f"--{phase}-rate", type=float, default=rate)
    args = parser.parse_args(argv)
    phases = [Phase(name, getattr(args, f"{name}_seconds"), getattr(args, f"{name}_rate"))
              for name in ("baseline", "overload", "recovery")]
    numbers = [args.request_timeout, args.drain_seconds, args.probe_seconds, args.probe_interval,
               args.recovery_threshold_ms, *(value for phase in phases for value in (phase.seconds, phase.rate))]
    if any(not math.isfinite(value) or value <= 0 for value in numbers):
        parser.error("durations, rates and thresholds must be finite and positive")
    if not (.001 <= args.request_timeout <= 300 and args.drain_seconds <= 600 and args.probe_seconds <= 600
            and args.probe_interval <= 60 and all(.001 <= phase.seconds <= 86400 for phase in phases)):
        parser.error("request timeout must be .001..300s; drain/probe <=600s, interval <=60s, phases .001..86400s")
    if not (0 < args.port < 65536 and 0 < args.connections <= 256 and 0 < args.queue_capacity <= 1_000_000
            and 0 < args.keys <= 1_000_000 and 0 <= args.value_bytes <= 256 * 1024
            and 0 < args.recovery_consecutive <= 100):
        parser.error("port, connections, queue, keys, value size or recovery streak out of bounds")
    if (any(phase.seconds * phase.rate > MAX_ARRIVALS for phase in phases)
            or sum(math.ceil(phase.seconds * phase.rate) for phase in phases) > MAX_ARRIVALS):
        parser.error(f"one run is limited to {MAX_ARRIVALS} scheduled arrivals; split larger runs")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    inputs = snapshot(output, args.binary, args.config)
    plan = {"created_utc": datetime.now(timezone.utc).isoformat(), "arguments": vars(args) | {"output": str(output),
            "binary": str(args.binary) if args.binary else None, "config": str(args.config) if args.config else None},
            "phases": [asdict(phase) for phase in phases], "inputs": inputs, "limitations": LIMITATIONS,
            "queue_depth_is_approximate": True, "raw_flush": "phase boundaries and shutdown; final fsync",
            "histogram": "ceil microseconds, 64 buckets per octave; percentiles are bucket upper bounds",
            "binary_config_relation": "caller-supplied copies, not attestation of running server",
            "on_error": "close the connection on any request error; reconnect only for a later job, never replay",
            "protocol_helper_limit": "v020 errors preserve the server code but do not validate the error frame request ID; this load tool is not a protocol-association acceptance test",
            "final_recovery_limit": "confirmation starts after the recovery phase and queue drain; it is not the first instant of recovery after overload",
            "set_value": "request ID in little-endian prefix plus zero padding; 0 bytes is fixed empty data, short values wrap at their byte width",
            "valid_definition": "complete tool evidence and accounting; passed additionally requires final PING recovery",
            "environment": {"platform": platform.platform(), "python": sys.version, "cpu_count": os.cpu_count(),
                            "pid": os.getpid(), "affinity": sorted(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else None,
                            "git_head": capture(ROOT, "git", "rev-parse", "HEAD"),
                            "git_status": capture(ROOT, "git", "status", "--short")}}
    write_json(output / "plan.json", plan)
    evidence = Evidence(output / "requests.jsonl", phases)
    pool = Pool(args.port, args.connections, args.queue_capacity, args.request_timeout, evidence,
                args.keys, args.value_bytes)
    result, failure, timing = {"valid": False}, None, None
    try:
        pool.start()
        timing = schedule(phases, pool.jobs, evidence)
    except BaseException as error:
        failure = {"type": type(error).__name__, "error": str(error)}
    finally:
        alive = pool.finish(args.drain_seconds if timing is not None else .01)
    result.update(timing=timing, failure=failure, threads_alive=alive)
    if timing is not None and not alive:
        result["recovery"] = recovery_probe(args.port, args.probe_seconds, args.probe_interval,
            args.recovery_threshold_ms, args.recovery_consecutive, evidence, timing["scheduled_end_ns"],
            timing["phases"][1]["end_ns"], timing["actual_end_ns"])
    result["maximum_queued"] = evidence.maximum_queued
    result["phases"] = evidence.finish()
    accounting = {}
    for name, row in result["phases"].items():
        counts = row["counts"]
        terminals = sum(value for key, value in counts.items() if key in (
            "success", "server_error", "server_connect_error", "client_timeout", "client_protocol_error",
            "client_transport_error", "tester_error", "tester_cancelled"))
        accounting[name] = {"scheduled_equals_enqueued_plus_dropped": counts.get("scheduled", 0)
                            == counts.get("enqueued", 0) + counts.get("tester_dropped", 0),
                            "every_enqueued_job_has_terminal_outcome": counts.get("enqueued", 0) == terminals}
    result["accounting"] = accounting
    result["valid"] = (failure is None and not alive and not any(state["setup_error"] for state in pool.states)
                       and all(all(checks.values()) for checks in accounting.values()))
    result["passed"] = result["valid"] and result.get("recovery", {}).get("recovered", False)
    write_json(output / "summary.json", result)
    print(json.dumps({"output": str(output), "valid": result["valid"], "passed": result["passed"],
                      "recovery": result.get("recovery")}, ensure_ascii=False))
    return 0 if result["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
