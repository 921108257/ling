"""开放到达工具的固定时钟及本机假服务回归；Anchored-clock and localhost fake-server load tests."""
from collections import Counter
import contextlib
import io
import json
from pathlib import Path
import queue
import socket
import socketserver
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

from open_load import (BoundedClient, Evidence, Histogram, Job, MAX_ARRIVALS, Phase, Pool, classify, main,
                       recovery_probe, schedule)
from v020 import HEADER, WireError, decode, encode, exact, packet


class FakeServer:
    """只拥有测试新建的回环监听与连接；Own only this test's new loopback listener and sockets."""
    def __init__(self, rule=None):
        self.rule = rule or (lambda command, arguments, index: {})
        self.requests, self.connections, self.sockets = [], 0, []
        self.lock = threading.Lock()
        parent = self

        class Handler(socketserver.BaseRequestHandler):
            """通过真实自定义协议提供可控慢响应；Serve controllable delays over the real wire format."""
            def handle(self):
                self.request.settimeout(2)
                self.request.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
                with parent.lock:
                    parent.connections += 1
                    parent.sockets.append(self.request)
                try:
                    while True:
                        _, _, kind, _, request_id, size = HEADER.unpack(exact(self.request, HEADER.size))
                        if kind != 1:
                            return
                        payload = exact(self.request, size)
                        command, offset = decode(payload)
                        arguments, offset = decode(payload, offset)
                        if offset != len(payload):
                            return
                        with parent.lock:
                            parent.requests.append((command, arguments, time.monotonic_ns()))
                            index = len(parent.requests)
                        action = parent.rule(command, arguments, index)
                        time.sleep(action.get("delay", 0))
                        if action.get("close"):
                            return
                        if action.get("error"):
                            body = b"\1" + encode(action["error"]) + encode("fake server error")
                        else:
                            value = {"protocol": 1, "events": True, "reliable_events": False,
                                     "stream_id": bytes(16), "server_version": "fake"} if command == "HELLO" else (
                                         "PONG" if command == "PING" else True)
                            body = b"\0" + encode(value)
                        response = packet(2, request_id, body)
                        if action.get("trickle"):
                            for byte in response:
                                self.request.sendall(bytes([byte]))
                                time.sleep(action["trickle"])
                        else:
                            self.request.sendall(response)
                except (OSError, WireError):
                    pass

        class Server(socketserver.ThreadingTCPServer):
            """关闭时不等待恶意滴流线程；Daemon handlers remain bounded by socket closure."""
            daemon_threads = True
            allow_reuse_address = False

        self.server = Server(("127.0.0.1", 0), Handler)
        self.port = self.server.server_address[1]
        self.worker = threading.Thread(target=self.server.serve_forever, kwargs={"poll_interval": .01}, daemon=True)

    def __enter__(self):
        self.worker.start()
        return self

    def __exit__(self, *_):
        self.server.shutdown()
        with self.lock:
            for stream in self.sockets:
                try:
                    stream.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
                stream.close()
        self.server.server_close()
        self.worker.join(2)


class FakeClock:
    """每次读取可前进，覆盖跨越等待边界；Clock reads may advance across sleep boundaries."""
    def __init__(self, step=0):
        self.now, self.step, self.sleeps = 10_000_000_000, step, []

    def read(self):
        current = self.now
        self.now += self.step
        return current

    def sleep(self, seconds):
        if seconds < 0:
            raise ValueError("negative sleep")
        self.sleeps.append(seconds)
        self.now += round(seconds * 1e9)


class Scheduling(unittest.TestCase):
    """没有服务响应参与的调度和容量契约；Scheduling and capacity contracts independent of replies."""
    def test_fixed_schedule_does_not_wait_for_responses_or_hide_drops(self):
        phases = [Phase("baseline", .1, 100), Phase("overload", .1, 200), Phase("recovery", .1, 100)]
        clock, jobs = FakeClock(), queue.Queue(2)
        with tempfile.TemporaryDirectory() as directory:
            evidence = Evidence(Path(directory) / "raw.jsonl", phases)
            timing = schedule(phases, jobs, evidence, clock.read, clock.sleep)
            rows = evidence.finish()
            raw = [json.loads(line) for line in (Path(directory) / "raw.jsonl").read_text(encoding="utf-8").splitlines()]
        arrivals = [row for row in raw if row["event"] == "arrival"]
        self.assertEqual(len(arrivals), 40)
        self.assertEqual([rows[phase.name]["counts"]["scheduled"] for phase in phases], [10, 20, 10])
        self.assertEqual(sum(row["counts"].get("tester_dropped", 0) for row in rows.values()), 38)
        self.assertFalse(any(row["errors"] for row in rows.values()))
        self.assertEqual([row["deadline_ns"] for row in arrivals[:4]],
                         [10_000_000_000, 10_010_000_000, 10_020_000_000, 10_030_000_000])
        self.assertEqual(timing["scheduled_end_ns"], 10_300_000_000)
        self.assertEqual([arrivals[index]["command"] for index in range(4)], ["PING", "SET", "PING", "SET"])
        self.assertLessEqual(jobs.qsize(), 2)

    def test_clock_crossing_deadline_never_requests_negative_sleep(self):
        clock, jobs = FakeClock(step=7_000_000), queue.Queue(20)
        phases = [Phase("baseline", .05, 100)]
        with tempfile.TemporaryDirectory() as directory:
            evidence = Evidence(Path(directory) / "raw.jsonl", phases)
            schedule(phases, jobs, evidence, clock.read, clock.sleep)
            rows = evidence.finish()
        self.assertEqual(rows["baseline"]["counts"]["scheduled"], 5)
        self.assertTrue(all(value >= 0 for value in clock.sleeps))

    def test_histogram_is_bounded_and_reports_empty_and_upper_bounds(self):
        histogram = Histogram()
        self.assertIsNone(histogram.result()["p99_us"])
        for value in range(1, 100_000):
            histogram.add(value * 1000)
        result = histogram.result()
        self.assertGreaterEqual(result["p50_us"], 50_000)
        self.assertLess(result["p50_us"], 50_800)
        self.assertLess(len(histogram.buckets), 1000)

    def test_error_categories_preserve_unknown_writes(self):
        for error, category, unknown in (
                (WireError("ERR_BUSY: queue full"), "server_error", False),
                (WireError("ERR_COMMIT_UNKNOWN: interrupted"), "server_error", True),
                (TimeoutError("late"), "client_timeout", True),
                (WireError("LOCAL_EOF: closed"), "client_transport_error", True),
                (WireError("LOCAL_PROTOCOL: wrong frame"), "client_protocol_error", True)):
            with self.subTest(error=error):
                result = classify(error, "SET")
                self.assertEqual((result["outcome"], result["result_unknown"]), (category, unknown))
        self.assertFalse(classify(OSError("connect"), "SET", "connect")["result_unknown"])
        self.assertEqual(classify(WireError("ERR_BUSY: connection cap"), "HELLO", "connect")["outcome"],
                         "server_connect_error")


class LiveWire(unittest.TestCase):
    """仅新建本机假服务，验证饱和、恢复、错误与退出；Own-server tests of overload, recovery and deadlines."""
    def run_tool(self, server, output, *extra):
        """极短三阶段，输出保持可复核；Use short phases while retaining complete evidence."""
        arguments = ["--port", str(server.port), "--output", str(output), "--connections", "2",
                     "--queue-capacity", "4", "--baseline-seconds", ".08", "--baseline-rate", "25",
                     "--overload-seconds", ".2", "--overload-rate", "500", "--recovery-seconds", ".15",
                     "--recovery-rate", "20", "--request-timeout", ".3", "--drain-seconds", ".5",
                     "--probe-seconds", ".6", "--probe-interval", ".01", "--recovery-consecutive", "2",
                     "--recovery-threshold-ms", "150", *extra]
        with contextlib.redirect_stdout(io.StringIO()):
            code = main(arguments)
        return code, json.loads((output / "summary.json").read_text(encoding="utf-8")), [
            json.loads(line) for line in (output / "requests.jsonl").read_text(encoding="utf-8").splitlines()]

    def test_saturation_drops_are_distinct_and_recovery_passes(self):
        with FakeServer(lambda command, args, index: {"delay": .012 if command != "HELLO" else 0}) as server:
            with tempfile.TemporaryDirectory() as directory:
                output = Path(directory) / "run"
                code, summary, raw = self.run_tool(server, output)
                plan = json.loads((output / "plan.json").read_text(encoding="utf-8"))
        self.assertEqual(code, 0)
        self.assertTrue(summary["passed"])
        self.assertTrue(summary["recovery"]["recovered"])
        self.assertGreater(summary["phases"]["overload"]["counts"].get("tester_dropped", 0), 0)
        self.assertEqual(summary["phases"]["overload"]["errors"], {})
        self.assertLessEqual(summary["maximum_queued"], 4)
        self.assertTrue(all(all(row.values()) for row in summary["accounting"].values()))
        self.assertEqual(plan["arguments"]["connections"], 2)
        self.assertIn("protocol_helper", plan["inputs"])
        arrivals = {row["request"]: row for row in raw if row["event"] == "arrival" and row["accepted"]}
        completions = [row for row in raw if row["event"] == "completion"]
        self.assertEqual(len(arrivals), len(completions))
        self.assertTrue(all(row["completion_ns"] >= row["dispatch_ns"] >= row["deadline_ns"] for row in completions))
        set_values = [arguments[1] for command, arguments, _ in server.requests if command == "SET"]
        self.assertGreater(len(set(set_values)), 1)

    def test_server_busy_has_a_separate_error_code_and_no_retry(self):
        with FakeServer(lambda command, args, index: {"error": "ERR_BUSY"} if command == "SET" else {}) as server:
            with tempfile.TemporaryDirectory() as directory:
                code, summary, raw = self.run_tool(server, Path(directory) / "run", "--overload-rate", "50")
        self.assertEqual(code, 0)
        errors = Counter()
        for phase in summary["phases"].values():
            errors.update(phase["errors"])
        self.assertGreater(errors["server_error:ERR_BUSY"], 0)
        failed_sets = [row for row in raw if row["event"] == "completion" and row["command"] == "SET"]
        self.assertTrue(all(row["outcome"] == "server_error" and not row["result_unknown"] for row in failed_sets))
        actual_sets = [row for row in server.requests if row[0] == "SET"]
        self.assertEqual(len(actual_sets), len(failed_sets))

    def test_drain_cancellation_is_local_and_queued_work_has_no_dispatch(self):
        with FakeServer(lambda command, args, index: {"delay": .4} if command != "HELLO" else {}) as server:
            with tempfile.TemporaryDirectory() as directory:
                _, summary, raw = self.run_tool(server, Path(directory) / "run", "--drain-seconds", ".01",
                                                 "--request-timeout", "1", "--probe-seconds", ".1")
        cancelled = [row for row in raw if row.get("outcome") == "tester_cancelled"]
        self.assertTrue(cancelled)
        self.assertTrue(any(row["dispatch_ns"] is None for row in cancelled))
        self.assertTrue(any(row["dispatch_ns"] is not None for row in cancelled))
        self.assertTrue(all(row["error_code"] == "LOCAL_DRAIN_DEADLINE" for row in cancelled))
        self.assertFalse(summary["threads_alive"])
        self.assertTrue(summary["valid"])
        self.assertFalse(summary["passed"])

    def test_trickling_response_obeys_whole_request_deadline(self):
        with FakeServer(lambda command, args, index: {"trickle": .02} if command == "SET" else {}) as server:
            with tempfile.TemporaryDirectory() as directory:
                started = time.monotonic()
                _, summary, raw = self.run_tool(server, Path(directory) / "run", "--request-timeout", ".09",
                                                 "--overload-rate", "30")
                elapsed = time.monotonic() - started
        timeouts = [row for row in raw if row.get("outcome") == "client_timeout"]
        self.assertTrue(timeouts)
        self.assertTrue(all(row["result_unknown"] for row in timeouts if row["command"] == "SET"))
        self.assertLess(elapsed, 3)
        self.assertFalse(summary["threads_alive"])

    def test_failed_final_ping_never_reports_pass(self):
        with FakeServer(lambda command, args, index: {"error": "ERR_BUSY"} if command == "PING" else {}) as server:
            with tempfile.TemporaryDirectory() as directory:
                code, summary, _ = self.run_tool(server, Path(directory) / "run", "--probe-seconds", ".08")
        self.assertEqual(code, 1)
        self.assertTrue(summary["valid"])
        self.assertFalse(summary["passed"])
        self.assertFalse(summary["recovery"]["recovered"])

    def test_setup_deadline_cancels_a_socket_registered_after_expiry(self):
        with FakeServer() as server, tempfile.TemporaryDirectory() as directory:
            phase = Phase("baseline", .01, 100)
            evidence = Evidence(Path(directory) / "raw.jsonl", [phase])

            def late_factory(port, timeout, register):
                """期限先于 socket 建立，后登记仍须取消；Registering after expiry must not escape cancellation."""
                time.sleep(.08)
                return BoundedClient(port, timeout, register)

            pool = Pool(server.port, 1, 2, .03, evidence, 10, 1, late_factory)
            try:
                with self.assertRaisesRegex(RuntimeError, "connection setup failed"):
                    pool.start()
            finally:
                alive = pool.finish(.01)
                evidence.finish()
            self.assertFalse(alive)
            self.assertEqual(pool.states[0]["setup_error"]["outcome"], "client_timeout")
            self.assertFalse(any(command != "HELLO" for command, _, _ in server.requests))

    def test_late_complete_response_is_timeout_even_before_watchdog_runs(self):
        class LateClient:
            """故意超过总期限后返回完整成功；Deliberately return a complete success after the deadline."""
            def __init__(self, port, timeout, register):
                self.hello = {}
                self.stream = self
                register(self)

            def request(self, *_):
                time.sleep(.03)
                return True

            def shutdown(self, *_):
                """假 socket 无网络资源；The fake socket owns no network resource."""

            def close(self):
                """不需要关闭真实 socket；No real socket needs closure."""

        phase = Phase("baseline", .01, 100)
        with tempfile.TemporaryDirectory() as directory:
            raw_path = Path(directory) / "raw.jsonl"
            evidence = Evidence(raw_path, [phase])
            pool = Pool(1, 1, 2, .005, evidence, 10, 1, LateClient)
            # 排除此测试中的轮询帮助，只验证响应完成时的直接期限核验。
            # Exclude polling assistance and exercise the direct completion-time deadline check.
            pool.monitor = threading.Thread(target=lambda: pool.monitor_stop.wait(1), daemon=True)
            pool.start()
            now = time.monotonic_ns()
            job = Job(1, "baseline", "SET", now, now, 1)
            pool.jobs.put_nowait(job)
            evidence.arrival(job, True)
            alive = pool.finish(.2)
            rows = evidence.finish()
            raw = [json.loads(line) for line in raw_path.read_text(encoding="utf-8").splitlines()]
        self.assertFalse(alive)
        self.assertEqual(rows["baseline"]["counts"]["client_timeout"], 1)
        terminal = [row for row in raw if row["event"] == "completion"][0]
        self.assertTrue(terminal["result_unknown"])
        self.assertEqual(terminal["error_code"], "LOCAL_TIMEOUT")

    def test_output_must_be_new_and_huge_run_is_rejected_before_connection(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(FileExistsError), patch("open_load.Pool.start") as start:
                main(["--port", "1", "--output", directory])
            start.assert_not_called()
            with self.assertRaises(SystemExit), patch("open_load.Pool.start") as start:
                with contextlib.redirect_stderr(io.StringIO()):
                    main(["--port", "1", "--output", str(Path(directory) / "huge"),
                          "--overload-rate", str(MAX_ARRIVALS)])
            start.assert_not_called()


if __name__ == "__main__":
    unittest.main()
