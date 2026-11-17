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
import unittest
from types import SimpleNamespace
from unittest.mock import patch

from v030 import (CappedLog, Client, Consumer, Records, ServerError, Trends, Value, WireError, check_missing,
                  configuration, consumer_acceptance, cpu_set, decode, disk_space, encode, evidence, failed_unchanged,
                  manifest, observe_expired, packet, parser, pressure_arguments, pressure_cycle,
                  receive, specimen)


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
