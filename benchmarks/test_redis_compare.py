"""比较工具协议与计划契约，不启动产品服务；Protocol/plan tests without running product servers."""
from collections import Counter
import socket
import threading
from pathlib import Path
import tempfile
import time
import tomllib
import unittest
from types import SimpleNamespace
from unittest.mock import MagicMock, patch

from redis_compare import (HEADER, MAX_REPLY, LingClient, RedisClient, Wire, WireError, configuration,
                           expected_wire, memory_report, owns_listener, packet, perform, quantiles,
                           run_case, save_latencies, schedule, summarize, value_at)
from v020 import encode, exact


class Peer:
    """只拥有一对本地测试socket；Own a local test socket pair with no external service."""
    def __init__(self, timeout=.2):
        self.client, self.server = socket.socketpair()
        self.server.settimeout(2)
        self.wire = Wire.__new__(Wire)
        self.wire.stream, self.wire.timeout = self.client, timeout
        self.wire.buffer, self.wire.budget = bytearray(), MAX_REPLY
        self.wire.deadline = time.monotonic() + timeout

    def close(self):
        """释放测试两端；Close both test endpoints."""
        self.client.close()
        self.server.close()


class Child:
    """只模拟工具自建进程的生命周期；Model only the controller-owned child's lifecycle."""
    pid = 1234
    returncode = None

    def poll(self):
        """启动时保持存活；Remain alive until owned cleanup."""
        return self.returncode

    def terminate(self):
        """只终止此实例；Terminate this fake child only."""
        self.returncode = 0

    def wait(self, timeout):
        """无需真实进程等待；No real process is involved."""
        return self.returncode


def case_arguments(**overrides):
    """最小有界生命周期参数；Minimal bounded driver lifecycle settings."""
    return SimpleNamespace(**(dict(connections=1, keys=1, warmup=0, seconds=.001, timeout=.01,
                                  value_bytes=16, ttl_ms=10000, memory_bytes=67108864,
                                  journal_bytes=67108864) | overrides))


class RedisComparisonTests(unittest.TestCase):
    """独立字节向量、有限预算及重复计划；Independent vectors, finite budgets and repeat planning."""
    def test_resp_coalesced_array_preserves_binary_and_nil(self):
        peer = Peer()
        client = RedisClient.__new__(RedisClient)
        client.wire = peer.wire
        try:
            peer.server.sendall(b"*4\r\n+OK\r\n$3\r\na\x00b\r\n$-1\r\n:123\r\n")
            self.assertEqual(client.response(), [b"OK", b"a\0b", None, 123])
        finally:
            peer.close()

    def test_resp_refuses_lengths_types_and_bad_bulk_terminator(self):
        vectors = (b"$-2\r\n", b"$8388609\r\n", b"*4097\r\n", b"*no\r\n",
                   b"?bad\r\n", b"$3\r\nabcXX", b"-ERR rejected\r\n")
        for vector in vectors:
            with self.subTest(vector=vector):
                peer = Peer()
                client = RedisClient.__new__(RedisClient)
                client.wire = peer.wire
                try:
                    peer.server.sendall(vector)
                    with self.assertRaises(WireError):
                        client.response()
                finally:
                    peer.close()

    def test_resp_fragmented_bulk_uses_one_absolute_deadline(self):
        peer = Peer(.06)
        client = RedisClient.__new__(RedisClient)
        client.wire = peer.wire
        stopped = threading.Event()

        def trickle():
            """间隔短于socket超时，但整帧超出总期限；Short intervals cannot renew the overall deadline."""
            try:
                peer.server.sendall(b"$20\r\n")
                for _ in range(20):
                    if stopped.wait(.02):
                        return
                    peer.server.sendall(b"a")
            except OSError:
                pass
        worker = threading.Thread(target=trickle, daemon=True)
        worker.start()
        started = time.monotonic()
        try:
            with self.assertRaises(TimeoutError):
                client.response()
            self.assertLess(time.monotonic() - started, .3)
        finally:
            stopped.set()
            worker.join(1)
            peer.close()

    def test_resp_request_has_exact_binary_vector_and_no_replay(self):
        peer = Peer()
        client = RedisClient.__new__(RedisClient)
        client.wire = peer.wire
        peer.server.sendall(b"+OK\r\n")
        try:
            self.assertEqual(client.request("SET", ["a", b"\x00\xff", "PX", 50]), b"OK")
            expected = b"*5\r\n$3\r\nSET\r\n$1\r\na\r\n$2\r\n\x00\xff\r\n$2\r\nPX\r\n$2\r\n50\r\n"
            self.assertEqual(exact(peer.server, len(expected)), expected)
            peer.server.settimeout(.02)
            with self.assertRaises(TimeoutError):
                peer.server.recv(1)
        finally:
            peer.close()

    def test_ling_set_and_set_px_have_strict_independent_wire_vectors(self):
        """普通SET保持类型，PX必须带Int标签；Plain SET retains types and PX uses the signed Int tag."""
        for value, encoded_value in (("v", b"\x05\0\0\0\x01v"), (b"\0\xff", b"\x06\0\0\0\x02\0\xff")):
            for operation in ("set", "set-px"):
                with self.subTest(value=value, operation=operation):
                    peer = Peer()
                    client = LingClient.__new__(LingClient)
                    client.wire, client.request_id = peer.wire, 0
                    peer.server.sendall(packet(2, 1, b"\x00\x01\x01"))
                    try:
                        self.assertGreaterEqual(perform(client, "ling", operation, "k", value, 600000), 0)
                        expected = (b"\x05\0\0\0\x03SET\x07" + (4 if operation == "set-px" else 2).to_bytes(4, "big")
                                    + b"\x05\0\0\0\x01k" + encoded_value)
                        if operation == "set-px":
                            expected += b"\x05\0\0\0\x02PX\x02\0\0\0\0\0\x09\x27\xc0"
                        header = HEADER.unpack(exact(peer.server, HEADER.size))
                        self.assertEqual(header, (b"LG", 1, 1, 0, 1, len(expected)))
                        self.assertEqual(exact(peer.server, header[-1]), expected)
                    finally:
                        peer.close()

    def test_ling_hello_still_uses_uint_and_invalid_px_never_sends(self):
        """HELLO不能随TTL修复改变标签；TTL fix must not change HELLO or send unrepresentable inputs."""
        peer = Peer()
        client = LingClient.__new__(LingClient)
        client.wire, client.request_id = peer.wire, 0
        peer.server.sendall(packet(2, 1, b"\x00\x01\x01"))
        try:
            self.assertIs(client.request("HELLO", [1]), True)
            header = HEADER.unpack(exact(peer.server, HEADER.size))
            self.assertEqual(exact(peer.server, header[-1]),
                b"\x05\0\0\0\x05HELLO\x07\0\0\0\x01\x03\0\0\0\0\0\0\0\x01")
            for ttl in (0, -1, 2**63, True):
                with self.subTest(ttl=ttl), self.assertRaisesRegex(WireError, "LOCAL_ENCODING"):
                    client.request("SET", ["k", "v", "PX", ttl])
            peer.server.settimeout(.02)
            with self.assertRaises(TimeoutError):
                peer.server.recv(1)
        finally:
            peer.close()

    def test_ling_heartbeat_and_late_response_do_not_complete_request(self):
        peer = Peer()
        client = LingClient.__new__(LingClient)
        client.wire, client.request_id = peer.wire, 1
        peer.server.sendall(packet(5, 0, b"") + packet(2, 1, b"\0" + encode("old"))
                            + packet(2, 2, b"\0" + encode("PONG")))
        try:
            self.assertEqual(client.request("PING"), "PONG")
            header = HEADER.unpack(exact(peer.server, HEADER.size))
            self.assertEqual(header[4], 2)
        finally:
            peer.close()

    def test_ling_rejects_future_response_and_preserves_server_error(self):
        for identity, payload, pattern in ((2, b"\0" + encode(True), "LOCAL_PROTOCOL"),
                                           (1, b"\1" + encode("ERR_BUSY") + encode("full"), "ERR_BUSY")):
            peer = Peer()
            client = LingClient.__new__(LingClient)
            client.wire, client.request_id = peer.wire, 0
            try:
                peer.server.sendall(packet(2, identity, payload))
                with self.assertRaisesRegex(WireError, pattern):
                    client.request("SET", ["k", b"v"])
            finally:
                peer.close()

    def test_ling_connection_error_accepts_zero_or_sent_id_but_not_future(self):
        for identity in (0, 1, 2):
            peer = Peer()
            client = LingClient.__new__(LingClient)
            client.wire, client.request_id = peer.wire, 0
            try:
                peer.server.sendall(packet(6, identity, encode("ERR_BUSY") + encode("full")))
                with self.assertRaisesRegex(WireError, "LOCAL_PROTOCOL" if identity == 2 else "ERR_BUSY"):
                    client.request("SET", ["key", b"value"])
            finally:
                peer.close()

    def test_identical_payload_bytes_and_changing_values(self):
        for size in (16, 17, 64, 4096):
            for kind in ("string", "bytes"):
                first, second = value_at(kind, size, 1), value_at(kind, size, 2)
                self.assertNotEqual(first, second)
                self.assertEqual(len(expected_wire("redis", first)), size)
                self.assertEqual(len(first.encode() if isinstance(first, str) else first), size)

    def test_schedule_is_seeded_three_independent_interleaved_repeats(self):
        planned = schedule(42, ("fast", "reliable"), ("string", "bytes"), ("get", "set", "set-px"), 3)
        self.assertEqual(planned, schedule(42, ("fast", "reliable"), ("string", "bytes"), ("get", "set", "set-px"), 3))
        self.assertEqual(len(planned), 72)
        for left, right in zip(planned[::2], planned[1::2]):
            self.assertEqual({left["system"], right["system"]}, {"ling", "redis"})
            self.assertEqual({key: value for key, value in left.items() if key != "system"},
                             {key: value for key, value in right.items() if key != "system"})
        counts = Counter((item["system"], item["mode"], item["kind"], item["operation"]) for item in planned)
        self.assertEqual(set(counts.values()), {3})

    def test_generated_configs_preserve_separate_durability_objects(self):
        fast = configuration("redis", "fast", 12345, 4, 268435456, 1073741824)
        reliable = configuration("redis", "reliable", 12345, 4, 268435456, 1073741824)
        self.assertIn('save ""\n', fast)
        self.assertIn("appendonly no\n", fast)
        self.assertIn("appendonly yes\nappendfsync always\n", reliable)
        self.assertIn("io-threads 1\n", reliable)
        for mode in ("fast", "reliable"):
            ling = tomllib.loads(configuration("ling", mode, 12345, 4, 268435456, 1073741824))
            self.assertEqual(ling["events"]["mode"], mode)
            self.assertEqual(ling["eviction"]["policy"], "noeviction")
            self.assertEqual("event_journal" in ling, mode == "reliable")

    def test_memory_scopes_and_nearest_rank_are_explicit(self):
        self.assertEqual(memory_report({"memory": {"managed_bytes": 123}}, "ling")["bytes"], 123)
        redis = memory_report({"info": "# Memory\r\nused_memory:100\r\nused_memory_dataset:80\r\n"}, "redis")
        self.assertEqual((redis["bytes"], redis["dataset_bytes"]), (100, 80))
        self.assertEqual(quantiles([1000, 2000, 3000, 4000])["p50_us"], 2)
        self.assertEqual(quantiles([1000, 2000, 3000, 4000])["p999_us"], 4)

    def test_measurement_rejects_wrong_reply_type(self):
        class Fake:
            """Bool与Bytes回复不得静默等价；Do not coerce reply types."""
            def request(self, command, arguments):
                return 1
        with self.assertRaises(WireError):
            perform(Fake(), "ling", "set", "key", b"value", 50)

    def test_failed_run_samples_and_independent_repeat_ranges_survive(self):
        """失败不删除样本，分位数不混池；Failures retain samples and repeat percentiles stay independent."""
        import gzip
        import json
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "latency.json.gz"
            save_latencies(path, [{"worker": 1, "samples": [3000]}, {"worker": 0, "samples": [1000, 2000]}])
            with gzip.open(path, "rt", encoding="utf-8") as source:
                self.assertEqual(json.load(source), [1000, 2000, 3000])
        rows = [{"mode": "fast", "kind": "bytes", "operation": "get", "system": "ling",
                 "p99_us": value, "valid": True} for value in (10, 20, 100)]
        summary = summarize(rows)
        self.assertEqual(summary[0]["ranges"]["p99_us"], {"min": 10, "median": 20, "max": 100})

    def test_listener_requires_matching_child_inode_loopback_port_and_listen_state(self):
        """共享net表中的其他socket不能冒充子进程；A namespace-wide socket table does not establish ownership."""
        with tempfile.TemporaryDirectory() as directory:
            proc = Path(directory)
            process = proc / "1234"
            (process / "fd").mkdir(parents=True)
            (process / "fd" / "3").touch()
            (process / "net").mkdir()
            table = process / "net" / "tcp"
            template = "header\n0: {} 00000000:0000 {} 00000000:00000000 00:00000000 00000000 1000 0 {} 1\n"
            for endpoint, state, inode, expected in (("0100007F:3039", "0A", "987", True),
                                                     ("0100007F:3039", "0A", "999", False),
                                                     ("00000000:3039", "0A", "987", False),
                                                     ("0100007F:3039", "01", "987", False)):
                with self.subTest(endpoint=endpoint, state=state, inode=inode):
                    table.write_text(template.format(endpoint, state, inode), encoding="ascii")
                    with patch("redis_compare.os.readlink", return_value="socket:[987]"):
                        self.assertEqual(owns_listener(1234, 12345, proc), expected)

    def test_foreign_listener_never_receives_protocol_or_preload(self):
        """端口被其他PID占用时不连接也不写入；Never connect or preload when another PID owns the port."""
        child = Child()
        case = dict(mode="fast", kind="bytes", operation="get", repeat=1, system="ling")
        with tempfile.TemporaryDirectory() as directory, patch("redis_compare.subprocess.Popen", return_value=child), \
                patch("redis_compare.free_port", return_value=12345), \
                patch("redis_compare.owns_listener", return_value=False), \
                patch("redis_compare.LingClient") as factory, patch("redis_compare.perform") as operation:
            result = run_case(case_arguments(), case, Path("unused"), Path(directory), None)
        self.assertFalse(result["valid"])
        self.assertIn("did not own", result["errors"][0]["error"])
        factory.assert_not_called()
        operation.assert_not_called()
        self.assertEqual(child.returncode, 0)

    def test_preload_eof_retains_unknown_write_and_key_without_replay(self):
        """发送后EOF必须保留Key和未知提交标记；EOF after sending retains key and unknown-write evidence."""
        child, client = Child(), MagicMock()
        client.request.return_value = "PONG"
        case = dict(mode="fast", kind="bytes", operation="get", repeat=1, system="ling")
        with tempfile.TemporaryDirectory() as directory, patch("redis_compare.subprocess.Popen", return_value=child), \
                patch("redis_compare.free_port", return_value=12345), \
                patch("redis_compare.owns_listener", return_value=True), \
                patch("redis_compare.LingClient", return_value=client), \
                patch("redis_compare.perform", side_effect=WireError("LOCAL_EOF: incomplete response")) as operation:
            result = run_case(case_arguments(), case, Path("unused"), Path(directory), None)
        failure = next(error for error in result["errors"] if error["phase"] == "preload")
        self.assertEqual((failure["key_index"], failure["key"]), (0, "cmp:000000000000"))
        self.assertTrue(failure["write_result_may_be_unknown"])
        self.assertFalse(result["valid"])
        self.assertEqual(operation.call_count, 1)
        client.close.assert_called_once()

    def test_metadata_time_is_included_before_opening_ttl_measurement_gate(self):
        """INFO消耗预算后拒绝测量，不产生计时样本；Reject an exhausted TTL after metadata, before timed work."""
        child, client, clock = Child(), MagicMock(), {"now": 0.0}
        client.request.return_value = "PONG"
        case = dict(mode="fast", kind="bytes", operation="set-px", repeat=1, system="ling")

        def metadata(*arguments):
            """模拟INFO耗时，避免依赖真实调度；Advance a fake clock without timing-sensitive sleeps."""
            clock["now"] = 2.0
            return {"memory": {"managed_bytes": 0}}

        with tempfile.TemporaryDirectory() as directory, patch("redis_compare.subprocess.Popen", return_value=child), \
                patch("redis_compare.free_port", return_value=12345), \
                patch("redis_compare.owns_listener", return_value=True), \
                patch("redis_compare.LingClient", return_value=client), \
                patch("redis_compare.time.monotonic", side_effect=lambda: clock["now"]), \
                patch("redis_compare.server_info", side_effect=metadata), \
                patch("redis_compare.resource_snapshot", return_value={"cpu_seconds": 0}), \
                patch("redis_compare.perform", return_value=1000) as operation:
            result = run_case(case_arguments(warmup=1, ttl_ms=1000), case, Path("unused"), Path(directory), None)
        self.assertFalse(result["valid"])
        self.assertTrue(any("TTL may expire" in error["error"] for error in result["errors"]))
        self.assertNotIn("start_ns", result)
        self.assertEqual(operation.call_count, 2)  # 一次预填充、一次预热；One preload and one warmup.
        self.assertEqual(result["threads_alive"], 0)

    def test_pinned_redis_builtin_is_accepted_and_extra_modules_stop_before_preload(self):
        """官方内置vectorset允许，额外模块拒绝；Accept the pinned built-in and reject extra modules before writes."""
        builtin = [b"name", b"vectorset", b"ver", 1, b"path", b"", b"args", []]
        external = [b"name", b"extra", b"ver", 1, b"path", b"/tmp/extra.so", b"args", []]
        for modules, reaches_preload in (([builtin], True), ([builtin, external], False), ([], False)):
            with self.subTest(modules=modules):
                child, client = Child(), MagicMock()
                client.request.return_value = b"PONG"
                metadata = {"config": [b"appendonly", b"no", b"appendfsync", b"always", b"save", b"",
                                       b"io-threads", b"1", b"maxmemory-policy", b"noeviction",
                                       b"maxmemory", b"67108864"], "modules": modules, "info": ""}
                case = dict(mode="fast", kind="bytes", operation="get", repeat=1, system="redis")
                with tempfile.TemporaryDirectory() as directory, patch("redis_compare.subprocess.Popen", return_value=child), \
                        patch("redis_compare.free_port", return_value=12345), \
                        patch("redis_compare.owns_listener", return_value=True), \
                        patch("redis_compare.RedisClient", return_value=client), \
                        patch("redis_compare.server_info", return_value=metadata), \
                        patch("redis_compare.perform", side_effect=WireError("LOCAL_EOF: incomplete response")) as operation:
                    result = run_case(case_arguments(), case, Path("unused"), Path(directory), None)
                self.assertEqual(operation.call_count, int(reaches_preload))
                self.assertEqual(result["effective_configuration"], metadata)
                self.assertFalse(result["configuration_mismatches"])
                if reaches_preload:
                    self.assertTrue(any(error["phase"] == "preload" and error["write_result_may_be_unknown"]
                                        for error in result["errors"]))
                else:
                    self.assertTrue(any("module mismatch" in error["error"] for error in result["errors"]))


if __name__ == "__main__":
    unittest.main()
