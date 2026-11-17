"""0.3 验收工具的类型、证据与阶段隔离契约。
Contracts for typed values, retained evidence and isolated 0.3 acceptance phases.
"""
import gzip
import hashlib
import io
import json
from pathlib import Path
import random
import socket
import struct
import tempfile
import threading
import time
import unittest
from types import SimpleNamespace
from unittest.mock import patch

from v030 import (CappedLog, Client, Consumer, Records, ServerError, Trends, Value, WireError, check_missing,
                  configuration, consumer_acceptance, cpu_set, decode, disk_space, encode, evidence, failed_unchanged,
                  manifest, native_snapshot, observe_expired, packet, parser, pressure_arguments, pressure_cycle,
                  progress_sample, receive, resource_sample, run_phase, specimen, worker)


class Protocol(unittest.TestCase):
    """字节向量独立于被测编码器；Golden vectors are independent of the tested encoder."""
    def test_all_types_have_independent_golden_bytes(self):
        vectors = [(Value(0), "00"), (Value(1, True), "0101"),
                   (Value(2, -2**63), "028000000000000000"),
                   (Value(3, 2**64-1), "03ffffffffffffffff"),
                   (Value(4, 1.0), "043ff0000000000000"),
                   (Value(5, "中"), "0500000003e4b8ad"),
                   (Value(6, b"\x00\xff"), "060000000200ff"),
                   (Value(7, [Value(2, 1)]), "0700000001020000000000000001"),
                   (Value(8, {"a": Value(0)}), "080000000105000000016100"),
                   (Value(9, [Value(3, 1)]), "0900000001030000000000000001")]
        for expected, hexadecimal in vectors:
            with self.subTest(tag=expected.tag):
                wire = bytes.fromhex(hexadecimal)
                self.assertEqual(encode(expected), wire)
                self.assertEqual(decode(wire), (expected, len(wire)))

    def test_type_sensitive_nested_equality_and_ordering(self):
        numbers = [Value(1, True), Value(2, 1), Value(3, 1), Value(4, 1.0)]
        self.assertEqual(len(set(numbers)), 4)
        self.assertNotEqual(Value(7, numbers), Value(9, numbers))
        self.assertEqual(Value(9, numbers), Value(9, reversed(numbers)))
        self.assertEqual(Value(8, [("a", numbers[0]), ("b", numbers[1])]),
                         Value(8, [("b", numbers[1]), ("a", numbers[0])]))
        self.assertNotEqual(Value(7, [Value(2, 1)]), Value(7, [Value(3, 1)]))
        self.assertNotEqual(Value(6, b"x"), Value(5, "x"))

    def test_set_deduplication_preserves_tags_and_float_zero(self):
        data = Value(9, [Value(2, 1), Value(3, 1), Value(2, 1), Value(4, -0.0), Value(4, 0.0)])
        self.assertEqual(len(data.data), 3)
        self.assertEqual(encode(Value(4, -0.0)), bytes.fromhex("040000000000000000"))
        value, _ = decode(bytes.fromhex("048000000000000000"))
        self.assertEqual(encode(value), bytes.fromhex("040000000000000000"))

    def test_bad_values_and_allocation_lengths_are_rejected(self):
        invalid = [b"\x05\xff\xff\xff\xff", b"\x07\xff\xff\xff\xff", b"\x01\x02",
                   b"\x03\x00", b"\x05\x00\x00\x00\x01\xff", b"\x04" + struct.pack(">d", float("nan")),
                   bytes.fromhex("09000000010700000000"),
                   bytes.fromhex("08000000020500000001610005000000016100")]
        for raw in invalid:
            with self.subTest(raw=raw), self.assertRaises(WireError):
                decode(raw)
        for tag, data in ((2, True), (3, -1), (2, 2**63), (4, float("nan")), (8, {"": Value(0)})):
            with self.subTest(tag=tag), self.assertRaises(WireError):
                Value(tag, data)
        nested = Value(0)
        for _ in range(14):
            nested = Value(7, [nested])
        with self.assertRaises(WireError):
            encode(nested)

    def test_fragmented_response_and_full_width_integer(self):
        left, right = socket.socketpair()
        try:
            wire = bytes.fromhex("4c470102000000000000000000020000000a0003ffffffffffffffff")
            def send():
                """逐字节发送独立响应；Send an independently encoded response one byte at a time."""
                for byte in wire:
                    right.sendall(bytes([byte]))
            thread = threading.Thread(target=send)
            thread.start()
            self.assertEqual(receive(left), (2, 2, Value(3, 2**64-1)))
            thread.join()
        finally:
            left.close()
            right.close()

    def test_error_envelope_and_trailing_bytes(self):
        for frame, error in [(packet(2, 7, b"\x01" + encode("ERR_BUSY") + encode("busy")), False),
                             (packet(2, 0, b"\x00\x00"), True),
                             (packet(2, 1, b"\x00\x00\x00"), True)]:
            left, right = socket.socketpair()
            try:
                right.sendall(frame)
                if error:
                    with self.assertRaises(WireError):
                        receive(left)
                else:
                    kind, request_id, result = receive(left)
                    self.assertEqual((kind, request_id, result.code), (2, 7, "ERR_BUSY"))
            finally:
                left.close()
                right.close()

    def test_client_matches_error_ids_before_exposing_errors(self):
        client = Client.__new__(Client)
        client.request_id = 1
        left, right = socket.socketpair()
        client.stream = left
        try:
            right.sendall(packet(2, 1, b"\x01" + encode("ERR_OVERFLOW") + encode("stale")) +
                          packet(5, 0, b"") + packet(2, 2, b"\x00" + encode(Value(0))))
            self.assertEqual(client.request("GET", ["key"]), Value(0))
            right.sendall(packet(2, 4, b"\x01" + encode("ERR_BUSY") + encode("future")))
            with self.assertRaisesRegex(WireError, "ID mismatch"):
                client.request("PING")
            right.sendall(packet(6, 2, encode("ERR_BUSY") + encode("fatal prior request")))
            with self.assertRaises(ServerError):
                client.request("PING")
            right.sendall(packet(6, 7, encode("ERR_BUSY") + encode("future fatal")))
            with self.assertRaisesRegex(WireError, "future ERROR"):
                client.request("PING")
        finally:
            left.close()
            right.close()


class FakeClient:
    """顺序参考响应不依赖产品实现；Ordered reference responses independent of product code."""
    def __init__(self, responses):
        self.responses, self.calls = iter(responses), []

    def request(self, command, arguments):
        """保留调用以检查写入未重放；Retain calls to detect replayed writes."""
        self.calls.append((command, arguments))
        result = next(self.responses)
        if isinstance(result, Exception):
            raise result
        return result


class NativeResources(unittest.TestCase):
    """不访问真实 proc 的 RSS 来源与失败契约；Check RSS sources and failures without reading real proc files."""
    def test_linux_rss_uses_rollup_and_preserves_original_snapshot(self):
        """精确汇总与近似读数分列，不修改共享 helper 的结果。
        Separate mapping totals from approximate status without mutating the shared helper result.
        """
        original = {"rss_bytes": 999, "cpu_seconds": 1.25, "handles": 7, "threads": 2}
        for kilobytes in (0, 12345):
            with self.subTest(kilobytes=kilobytes), patch("v030.sys.platform", "linux"), \
                 patch("v030.process_snapshot", return_value=original), \
                 patch("v030.Path.read_text", autospec=True, return_value=f"0000-ffff ---p 00000000 00:00 0 [rollup]\nRss: {kilobytes} kB\nPss: 12 kB\n") as read:
                result = native_snapshot(123)
            self.assertEqual(result, {"rss_bytes": kilobytes * 1024, "vmrss_approximate_bytes": 999,
                                      "rss_source": "proc_smaps_rollup:Rss", "cpu_seconds": 1.25,
                                      "handles": 7, "threads": 2})
            self.assertEqual(read.call_args.args, (Path("/proc/123/smaps_rollup"),))
            self.assertEqual(read.call_args.kwargs, {"encoding": "ascii"})
            self.assertEqual(original, {"rss_bytes": 999, "cpu_seconds": 1.25, "handles": 7, "threads": 2})

    def test_linux_rejects_missing_duplicate_or_malformed_rollup_rss(self):
        """单位、符号或重复字段错误不能静默采用 VmRSS。
        Invalid units, signs and duplicate fields must never fall back to VmRSS.
        """
        invalid = ("Pss: 1 kB\n", "Rss: 1 kB\nRss: 2 kB\n", "Rss: 1 kB\nRss: invalid\n",
                   "Rss: -1 kB\n", "Rss: +1 kB\n", "Rss: 1.5 kB\n", "Rss: 1_000 kB\n",
                   "Rss: １２ kB\n", "Rss: 1 KB\n", "Rss: 1024 B\n", "Rss: 1\n",
                   "Rss: 1 kB extra\n", "Rss : 1 kB\n")
        for content in invalid:
            with self.subTest(content=content), patch("v030.sys.platform", "linux"), \
                 patch("v030.process_snapshot", return_value={"rss_bytes": 999, "threads": 2}), \
                 patch("v030.Path.read_text", autospec=True, return_value=content):
                result = native_snapshot(123)
            self.assertNotIn("rss_bytes", result)
            self.assertEqual(result["vmrss_approximate_bytes"], 999)
            self.assertEqual(result["threads"], 2)
            self.assertIn("ValueError", result["rss_unavailable"])

    def test_linux_read_failure_keeps_main_rss_unavailable(self):
        """缺文件、权限和编码失败保留原因，其他原生指标仍可观察。
        Missing files, permissions and decoding failures retain causes without dropping other native metrics.
        """
        for error in (FileNotFoundError("missing"), PermissionError("denied"),
                      UnicodeDecodeError("ascii", b"\xff", 0, 1, "invalid")):
            with self.subTest(error=error), patch("v030.sys.platform", "linux"), \
                 patch("v030.process_snapshot", return_value={"cpu_seconds": 2, "handles": 3}), \
                 patch("v030.Path.read_text", autospec=True, side_effect=error):
                result = native_snapshot(123)
            self.assertNotIn("rss_bytes", result)
            self.assertNotIn("vmrss_approximate_bytes", result)
            self.assertEqual(result["cpu_seconds"], 2)
            self.assertEqual(result["handles"], 3)
            self.assertIn(type(error).__name__, result["rss_unavailable"])

    def test_windows_keeps_native_metrics_without_proc_access(self):
        """Windows 继续使用已有原生 API；Keep existing Windows native APIs without proc access."""
        original = {"rss_bytes": 123, "cpu_seconds": 2, "handles": 3, "threads": None}
        with patch("v030.sys.platform", "win32"), patch("v030.process_snapshot", return_value=original), \
             patch("v030.Path.read_text", autospec=True) as read:
            self.assertEqual(native_snapshot(123), original)
        read.assert_not_called()


class Acceptance(unittest.TestCase):
    """误报边界与证据可复现性；False-positive boundaries and reproducible evidence."""
    def setUp(self):
        """每次独立临时证据目录；Use a fresh temporary evidence directory per test."""
        self.temporary = tempfile.TemporaryDirectory()
        self.directory = Path(self.temporary.name)
        self.records = Records(self.directory, 123, examples=1)

    def tearDown(self):
        """先关闭证据再删除临时目录；Close evidence before removing the temporary directory."""
        self.records.close()
        self.temporary.cleanup()

    def test_rejected_write_checks_unchanged_value(self):
        client = FakeClient([ServerError("ERR_OVERFLOW", "overflow"), Value(3, 9)])
        failed_unchanged(self.records, client, "key", Value(2, 9), "INCR", ["key"], "ERR_OVERFLOW")
        self.assertEqual(self.records.snapshot()["findings"], {"unexpected_result": 1})
        self.assertEqual(len(client.calls), 2)

    def test_null_does_not_hide_missing_error(self):
        check_missing(self.records, FakeClient([Value(0)]), "key")
        self.assertEqual(self.records.snapshot()["findings"], {"expected_missing_key": 1})

    def test_expiry_requires_server_pttl_missing(self):
        client = FakeClient([Value(2, 0), Value(2, -2)])
        self.assertTrue(observe_expired(self.records, client, "key", threading.Event()))
        self.assertEqual([command for command, _ in client.calls], ["PTTL", "PTTL"])

    def test_pressure_accepts_eviction_miss_without_false_data_loss(self):
        client = FakeClient([Value(1, True), ServerError("ERR_KEY_NOT_FOUND", "evicted")])
        pressure_cycle(self.records, client, 7, 123, 0, 64)
        self.assertEqual(self.records.snapshot()["findings"], {})
        self.assertEqual(self.records.snapshot()["counts"]["pressure_eviction_or_expiry_misses"], 1)

    def test_pressure_does_not_hide_changed_surviving_value(self):
        client = FakeClient([Value(1, True), Value(6, b"wrong")])
        pressure_cycle(self.records, client, 7, 123, 0, 64)
        self.assertEqual(self.records.snapshot()["findings"], {"pressure_surviving_value_mismatch": 1})

    def test_pressure_input_can_be_rebuilt_without_prior_cycles(self):
        before = pressure_arguments(123, 2, 500, 64)
        pressure_arguments(123, 2, 501, 64)
        self.assertEqual(before, pressure_arguments(123, 2, 500, 64))
        self.assertNotEqual(before, pressure_arguments(123, 2, 501, 64))
        self.assertEqual(len(before[1].data), 64)

    def test_bounded_examples_and_complete_error_log(self):
        for number in range(3):
            self.records.finding("difference", expected=Value(3, 2**64-1), actual=Value(6, b"x" * 2048), number=number)
        self.assertEqual(self.records.snapshot()["finding_examples_saved"], 1)
        self.assertEqual(self.records.snapshot()["findings"], {"difference": 3})
        self.records.raw.flush()
        with (self.directory / "errors.jsonl.gz").open("rb") as source:
            # gzip 的写入尚未封尾，解码首个完整刷新块。
            # The writer is still open; decode complete flushed blocks without a footer.
            import zlib
            raw = zlib.decompressobj(16 + zlib.MAX_WBITS).decompress(source.read()).decode("utf-8")
        rows = [json.loads(line) for line in raw.splitlines()]
        self.assertEqual(len(rows), 3)
        self.assertEqual(rows[0]["expected"]["value"], str(2**64-1))
        self.assertEqual(len(rows[0]["actual"]["value"]["hex"]), 4096)

    def test_server_log_retains_complete_output_and_bounded_tails(self):
        """完整日志可恢复且跨块 panic 只计一次；Recover complete output and count split panic markers once."""
        marker = b"panicked at"
        raw = b"x" * (8192 - len(marker)) + marker + b"x" * (8192 - 3) + marker + b"x" * (8 * 1024 * 1024)
        logger = CappedLog(io.BytesIO(raw), self.directory, self.records)
        logger.thread.join(10)
        self.assertFalse(logger.thread.is_alive())
        self.assertEqual(logger.total, len(raw))
        self.assertEqual(logger.panic_markers, 2)
        self.assertEqual(self.records.snapshot()["findings"], {"server_panic_log_marker": 2})
        with gzip.open(self.directory / "server-complete.log.gz", "rb") as stream:
            self.assertEqual(stream.read(), raw)
        previous = (self.directory / "server.previous.log").read_bytes()
        current = (self.directory / "server.log").read_bytes()
        self.assertLessEqual(len(previous), 4 * 1024 * 1024)
        self.assertLessEqual(len(current), 4 * 1024 * 1024)
        self.assertEqual(previous + current, raw[-len(previous + current):])

    def test_expected_capacity_error_keeps_exact_reproduction_hash(self):
        self.records.context.value = {"phase": "pressure", "worker": 2, "database": 0, "cycle": 500, "payload_bytes": 64}
        args = pressure_arguments(123, 2, 500, 64)
        client = FakeClient([ServerError("ERR_OUT_OF_MEMORY", "capacity")])
        self.records.request(client, "SET", args, allowed=("ERR_OUT_OF_MEMORY",))
        self.assertEqual(self.records.snapshot()["findings"], {})
        self.assertEqual(self.records.snapshot()["server_errors"], {"ERR_OUT_OF_MEMORY": 1})
        import zlib
        raw = zlib.decompressobj(16 + zlib.MAX_WBITS).decompress((self.directory / "errors.jsonl.gz").read_bytes())
        row = json.loads(raw)
        rebuilt = Value(7, [Value(5, arg) if isinstance(arg, str) else arg for arg in args])
        self.assertEqual(row["arguments_sha256"], hashlib.sha256(encode(rebuilt)).hexdigest())
        self.assertEqual(row["worker"], 2)

    def test_transport_error_never_replays(self):
        client = FakeClient([OSError("disconnected")])
        with self.assertRaises(OSError):
            self.records.request(client, "INCR", ["key"])
        self.assertEqual(len(client.calls), 1)
        self.assertIn("connection_or_protocol_failure", self.records.snapshot()["findings"])

    def test_shadow_and_pressure_use_fresh_distinct_configuration(self):
        args = parser().parse_args(["unused", "--server-binary", "ling", "--binary-revision", "test", "--seconds", "51840"])
        self.assertEqual(args.seconds, 51840)
        shadow = configuration(args, 10001, 10002, "fast", "shadow", "noeviction")
        pressure = configuration(args, 10001, 10002, "reliable", "pressure", "random")
        self.assertIn("max_memory_bytes=16777216", shadow)
        self.assertIn('policy="noeviction"', shadow)
        self.assertIn("max_memory_bytes=2097152", pressure)
        self.assertIn('policy="random"', pressure)
        self.assertIn('[event_journal]', pressure)

    def test_acceptance_stage_is_explicit_and_defaults_to_post_release(self):
        """预发布证据不会标成发布后；Pre-release evidence must not claim to be post-release."""
        arguments = ["unused", "--server-binary", "ling", "--binary-revision", "test"]
        self.assertEqual(parser().parse_args(arguments).stage, "post-release")
        binary = self.directory / "ling"
        binary.write_bytes(b"candidate")
        for stage in ("pre-release", "post-release"):
            with self.subTest(stage=stage), patch("v030.capture", return_value={"stdout": ""}):
                args = parser().parse_args(arguments + ["--stage", stage])
                row = manifest(args, binary)
                self.assertEqual(row["stage"], stage)
                self.assertEqual(row["mode"], stage.replace("-", "_") + "_record_only")

    def test_environment_helpers_do_not_merge_scalar_tags(self):
        values = [specimen(tag, random.Random(123)) for tag in range(10)]
        for expected in values:
            self.assertEqual(decode(encode(expected))[0], expected)
        self.assertEqual(evidence(Value(3, 2**64-1))["value"], "18446744073709551615")
        self.assertEqual(cpu_set("0-2,4", {0, 1, 2, 3, 4}), {0, 1, 2, 4})
        with self.assertRaises(ValueError):
            cpu_set("0-5", {0, 1, 2})

    def test_resource_slope_requires_long_enough_observation(self):
        trends = Trends(10)
        for elapsed in (0, 10, 20, 30):
            trends.add(elapsed, {"rss_bytes": 100 + elapsed})
        self.assertIsNone(trends.summary()["rss_bytes"]["slope_per_hour"])
        for elapsed in (40, 50, 60, 70):
            trends.add(elapsed, {"rss_bytes": 100 + elapsed})
        self.assertAlmostEqual(trends.summary()["rss_bytes"]["slope_per_hour"], 3600)

    def test_resource_history_retains_each_workers_progress(self):
        """旧采样不随后续计数改变，可核对逐 worker 的停滞窗口。
        Historical samples stay independent, exposing per-worker stalls between observations.
        """
        args = parser().parse_args(["unused", "--server-binary", "ling", "--binary-revision", "test"])
        stop = threading.Event()
        client = FakeClient([])
        client.close = lambda: None
        def cycle(*unused):
            """只完成两个周期后请求停机；Request stop after exactly two completed cycles."""
            self.records.count("pressure_cycles")
            if self.records.snapshot()["counts"]["pressure_cycles"] == 2:
                stop.set()
        with patch("v030.Client", return_value=client), patch("v030.pressure_cycle", side_effect=cycle):
            worker(args, self.records, 1234, "pressure", 0, stop)
        self.assertEqual(self.records.snapshot()["counts"]["worker_0_cycles"], 2)
        def sample(elapsed):
            """独立模拟可用的资源和指标；Supply independent resource and metric observations."""
            response = io.BytesIO(b"ling_test 1\n")
            with patch("v030.native_snapshot", return_value={"rss_bytes": 100}), \
                 patch("v030.urllib.request.urlopen", return_value=response):
                return resource_sample(self.records, FakeClient([Value(8, {})]),
                                       SimpleNamespace(pid=123), elapsed, 1234, self.directory)
        first = sample(5)
        self.records.count("worker_1_cycles", 10)
        second = sample(10)
        self.assertNotIn("worker_1_cycles", first["workload"]["counts"])
        self.assertEqual(second["workload"]["counts"]["worker_1_cycles"], 10)
        self.assertEqual(first["workload"]["counts"]["worker_0_cycles"], 2)

    def test_consumer_failure_is_not_hidden_by_successful_production(self):
        rows = {f"{speed}-db{database}": {"received": 10, "acks": 10, "termination": "requested_stop", "errors": {}}
                for database in range(2) for speed in ("fast", "slow")}
        self.assertTrue(consumer_acceptance("reliable", rows)["valid"])
        rows["slow-db0"]["termination"] = "error"
        self.assertFalse(consumer_acceptance("reliable", rows)["valid"])
        rows["slow-db0"].update(termination="expected_fast_lag", errors={"ERR_OFFSET_EXPIRED": 1}, acks=9)
        self.assertFalse(consumer_acceptance("reliable", rows)["valid"])
        self.assertTrue(consumer_acceptance("fast", rows)["valid"])
        rows["fast-db0"].update(termination="expected_fast_lag", errors={"ERR_OFFSET_EXPIRED": 1})
        self.assertFalse(consumer_acceptance("fast", rows)["valid"])

    def test_resource_times_bracket_slow_info_and_consumer_snapshots(self):
        """延迟INFO与消费者读数必须全部位于采样区间，不能用采样开始冒充计数时刻。
        Delayed INFO and consumer reads must lie inside the sample interval, not at its start timestamp.
        """
        clock = [100.0]
        def native(_):
            """模拟原生采样耗时；Simulate native observation latency."""
            clock[0] += 1
            return {"rss_bytes": 100}
        def rollup(*unused, **options):
            """逐映射汇总的读取成本也在边界内；Include mapping-total read cost inside the sample boundary."""
            clock[0] += 1
            return "Rss: 1 kB\n"
        def consumer_state():
            """在慢读取后完成消费者快照；Complete a consumer snapshot after a slow read."""
            clock[0] += 3
            return {"acks": 1}
        response = io.BytesIO(b"ling_test 1\n")
        client = FakeClient([Value(8, {})])
        original_request = client.request
        def info(command, arguments):
            """模拟INFO等待；Simulate INFO latency."""
            clock[0] += 2
            return original_request(command, arguments)
        client.request = info
        consumer = SimpleNamespace(name="slow-db0", snapshot=consumer_state)
        with patch("v030.time.monotonic", side_effect=lambda: clock[0]), \
             patch("v030.sys.platform", "linux"), \
             patch("v030.process_snapshot", side_effect=native), \
             patch("v030.Path.read_text", autospec=True, side_effect=rollup), \
             patch("v030.urllib.request.urlopen", return_value=response):
            sample = resource_sample(self.records, client, SimpleNamespace(pid=1), 0, 1, self.directory,
                                     consumers=[consumer], phase_started=90)
        self.assertEqual(sample["sample_started_elapsed_seconds"], 10)
        self.assertEqual(sample["sample_completed_elapsed_seconds"], 17)
        self.assertEqual(sample["elapsed_seconds"], 10)
        self.assertEqual(sample["native"]["rss_bytes"], 1024)
        self.assertEqual(sample["consumers"]["slow-db0"]["acks"], 1)

    def test_terminal_sample_preserves_stop_boundary_and_budget(self):
        """排空耗时不充作负载时长，末次样本仍保存且不突破既有样本预算。
        Draining is separate from load duration; the terminal sample is retained within the original sample cap.
        """
        args = parser().parse_args([str(self.directory), "--server-binary", "ling", "--binary-revision", "test",
                                    "--seconds", ".02", "--resource-samples", "1", "--workers", "2",
                                    "--warmup-seconds", "0"])
        args.output = self.directory
        process = SimpleNamespace(pid=123, stdout=io.BytesIO(), returncode=None)
        process.poll = lambda: process.returncode
        def wait(timeout):
            """只模拟自有进程正常退出；Simulate only the owned process's clean exit."""
            process.returncode = 0
            return 0
        process.wait = wait
        class Thread:
            """排空时产生后续计数，验证停止前快照不被覆盖。
            Add late counters while draining to verify the pre-stop snapshot stays independent.
            """
            def __init__(self, target=None, args=(), **unused):
                self.args, self.ident, self.name, self.alive = args, None, "fake", False
            def start(self):
                """模拟已开始；Mark the simulated thread as started."""
                self.ident, self.alive = 1, True
            def is_alive(self):
                """显式报告存活；Report simulated liveness."""
                return self.alive
            def join(self, timeout):
                """排空不应计入有效负载边界；Draining must not move the effective load boundary."""
                if self.args:
                    self.args[1].count("worker_0_cycles", 100)
                    self.args[1].count("workers_requested_stop")
                time.sleep(.003)
                self.alive = False
        class FakeConsumer:
            """固定四组状态，不建立连接；Provide four bounded group states without connections."""
            def __init__(self, args, records, port, mode, database, slow, stream_id, stop):
                self.name = f"{'slow' if slow else 'fast'}-db{database}"
                self.thread = Thread()
            def snapshot(self):
                """保存停止状态；Retain the simulated stop state."""
                return {"received": 1, "acks": 1, "termination": "running" if self.thread.alive else "requested_stop", "errors": {}}
        class Logger:
            """保留正常空gzip日志供阶段收尾检查；Retain a valid empty gzip log for phase finalization."""
            def __init__(self, pipe, directory, records):
                self.thread, self.total, self.panic_markers = Thread(), 0, 0
                with gzip.open(directory / "server-complete.log.gz", "wb"):
                    pass
        observer = SimpleNamespace(hello=Value(8, {"stream_id": Value(6, b"x" * 16)}), close=lambda: None,
                                   request=lambda command, arguments: Value(8, {}) if command == "INFO" else Value(1, True))
        native = {"rss_bytes": 100, "cpu_seconds": 0, "handles": 4, "threads": 2}
        with patch("v030.subprocess.Popen", return_value=process), patch("v030.CappedLog", Logger), \
             patch("v030.Client", return_value=observer), patch("v030.Consumer", FakeConsumer), \
             patch("v030.threading.Thread", Thread), patch("v030.native_snapshot", return_value=native), \
             patch("v030.urllib.request.urlopen", side_effect=lambda *a, **k: io.BytesIO(b"ling_test 1\n")), \
             patch("v030.disk_space", return_value={"output_filesystem_free_bytes": 10**12}):
            result = run_phase(args, self.directory / "ling", "fast", "pressure", "noeviction")
        with gzip.open(self.directory / "fast-pressure-noeviction/resources.jsonl.gz", "rt", encoding="utf-8") as stream:
            samples = [json.loads(line) for line in stream]
        self.assertEqual(len(samples), 1)
        self.assertEqual(samples[0]["sample_kind"], "terminal")
        self.assertEqual(result["resource_samples_saved"], 1)
        self.assertGreater(result["counts"]["resource_samples_omitted"], 0)
        self.assertNotIn("worker_0_cycles", result["load_stop"]["workload"]["counts"])
        self.assertEqual(samples[0]["workload"]["counts"]["worker_0_cycles"], 200)
        self.assertLessEqual(result["load_stop"]["sample_completed_elapsed_seconds"], result["load_stop_requested_elapsed_seconds"])
        self.assertGreater(samples[0]["sample_started_elapsed_seconds"], result["load_stop_requested_elapsed_seconds"])
        self.assertEqual(result["native_resources"]["attempts"], 2)
        for field in ("rss_bytes", "handles", "threads"):
            self.assertEqual(result["resource_trends"][field]["samples"], 2)

    def test_planned_offline_is_accounted_while_still_disconnected(self):
        """正在离线的时间立即扣除，完成和重复收尾不会重复累计。
        Ongoing offline time is deducted immediately; completion and repeated cleanup do not double count it.
        """
        args = parser().parse_args(["unused", "--server-binary", "ling", "--binary-revision", "test"])
        stream_id = b"x" * 16
        client = FakeClient([True])
        client.hello, client.subscribe, client.close = {"stream_id": stream_id}, lambda *a: None, lambda: None
        with patch("v030.EventClient", return_value=client):
            consumer = Consumer(args, self.records, 1234, "reliable", 0, True, stream_id, threading.Event())
        consumer.started, consumer.offline_started = 100, 120
        consumer.state["planned_disconnected_seconds"] = 2
        with patch("v030.time.monotonic", return_value=130):
            during = consumer.snapshot()
        self.assertTrue(during["planned_offline"])
        self.assertEqual((during["elapsed_seconds"], during["planned_disconnected_seconds"], during["active_seconds"]), (30, 12, 18))
        consumer.end_planned_disconnect(140, reconnected=True)
        consumer.end_planned_disconnect(150, reconnected=False)
        with patch("v030.time.monotonic", return_value=145):
            after = consumer.snapshot()
        self.assertFalse(after["planned_offline"])
        self.assertEqual((after["planned_disconnected_seconds"], after["active_seconds"], after["reconnects"]), (22, 23, 1))

    def test_progress_boundary_keeps_a_conservative_count_snapshot(self):
        """边界后的完成量不会追写进此前的停止快照。
        Later completions cannot be written back into an earlier stop-boundary snapshot.
        """
        with patch("v030.time.monotonic", side_effect=[100, 102]):
            boundary = progress_sample(self.records, [], 90)
        self.records.count("worker_0_cycles", 100)
        self.assertEqual((boundary["sample_started_elapsed_seconds"], boundary["sample_completed_elapsed_seconds"]), (10, 12))
        self.assertNotIn("worker_0_cycles", boundary["workload"]["counts"])

    def test_stop_during_planned_disconnect_closes_the_offline_interval(self):
        """计划离线中请求停止，也记录区间结束且不伪装为已重连。
        A stop during planned offline time closes its evidence interval without claiming reconnection.
        """
        args = parser().parse_args(["unused", "--server-binary", "ling", "--binary-revision", "test",
                                    "--consumer-reconnect-seconds", "1"])
        stream_id, stop = b"x" * 16, threading.Event()
        client = FakeClient([True])
        client.hello, client.subscribe, client.close = {"stream_id": stream_id}, lambda *a: None, lambda: None
        with patch("v030.EventClient", return_value=client):
            consumer = Consumer(args, self.records, 1234, "reliable", 0, True, stream_id, stop)
        ticks = iter(range(100, 200))
        with patch("v030.time.monotonic", side_effect=lambda: next(ticks)), \
             patch.object(stop, "wait", side_effect=lambda seconds: stop.set()):
            consumer.run()
            final = consumer.snapshot()
        self.assertEqual(final["termination"], "requested_stop")
        self.assertFalse(final["planned_offline"])
        self.assertGreater(final["planned_disconnected_seconds"], 0)
        self.assertEqual(final["reconnects"], 0)
        self.assertEqual(final["active_seconds"] + final["planned_disconnected_seconds"], final["elapsed_seconds"])
        import zlib
        raw = zlib.decompressobj(16 + zlib.MAX_WBITS).decompress((self.directory / "errors.jsonl.gz").read_bytes())
        rows = [json.loads(line) for line in raw.splitlines()]
        self.assertEqual([row["category"] for row in rows],
                         ["planned_consumer_disconnect_started", "planned_consumer_disconnect_completed"])
        self.assertFalse(rows[-1]["reconnected"])

    def test_consumer_tracks_wire_names_ack_holes_and_duplicates(self):
        args = parser().parse_args(["unused", "--server-binary", "ling", "--binary-revision", "test"])
        stream_id = bytes(range(16))
        client = FakeClient([True, True, True, True])
        client.hello = {"stream_id": stream_id}
        client.subscribe = lambda pattern, group: None
        client.close = lambda: None
        original_request = client.request
        client.request = lambda command, arguments, **kwargs: original_request(command, arguments)
        with patch("v030.EventClient", return_value=client):
            consumer = Consumer(args, self.records, 1234, "reliable", 0, False, stream_id, threading.Event())
        for sequence in (2, 1, 2):
            consumer.acknowledge({"event_id": {"stream_id": stream_id, "db": 0, "shard": 0, "sequence": sequence},
                                  "event_type": "KEY_CREATED"})
        state = consumer.snapshot()
        self.assertEqual((state["received"], state["acks"], state["duplicates"]), (3, 3, 1))
        self.assertEqual(state["confirmed_watermarks"]["0"], "2")
        self.assertEqual(state["confirmed_above_watermark"]["0"], 0)
        with self.assertRaisesRegex(WireError, "EVENT_ID"):
            consumer.acknowledge({"event_id": {"stream_id": stream_id, "db": 1, "shard": 0, "sequence": 3},
                                  "event_type": "KEY_CREATED"})

    def test_idle_heartbeat_returns_to_stop_check(self):
        args = parser().parse_args(["unused", "--server-binary", "ling", "--binary-revision", "test"])
        stream_id, stop = bytes(range(16)), threading.Event()
        client = FakeClient([True])
        client.hello, client.events, client.stream = {"stream_id": stream_id}, [], object()
        client.subscribe = lambda pattern, group: None
        client.close = lambda: None
        with patch("v030.EventClient", return_value=client):
            consumer = Consumer(args, self.records, 1234, "reliable", 0, False, stream_id, stop)
        def heartbeat(_):
            """读到心跳后请求正常结束；Request normal stop after a heartbeat is read."""
            stop.set()
            return 5, 0, None
        with patch("v030.select.select", return_value=([client.stream], [], [])), patch("v030.event_receive", side_effect=heartbeat):
            consumer.run()
        self.assertEqual(consumer.snapshot()["termination"], "requested_stop")
        self.assertEqual(self.records.snapshot()["findings"], {})

    def test_wsl_disk_guard_includes_host_volume(self):
        with patch("v030.platform.release", return_value="6.6-microsoft-standard-WSL2"), \
             patch("v030.Path.is_dir", return_value=True), \
             patch("v030.shutil.disk_usage", side_effect=[SimpleNamespace(free=10**12), SimpleNamespace(free=5*10**9)]):
            spaces = disk_space(self.directory)
        self.assertEqual(min(spaces.values()), 5*10**9)


if __name__ == "__main__":
    unittest.main()
