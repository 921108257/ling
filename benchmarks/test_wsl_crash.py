"""WSL 验收工具的纯函数与保护测试；Pure recovery and safety tests without WSL operations."""
from pathlib import Path
from collections import deque
import socket
import struct
import tempfile
import threading
import unittest
from unittest.mock import Mock, patch
import zlib

from wsl_crash import (BASE, DISTRO, Evidence, LINUX, MAX_PENDING_EVENTS, Wsl, configuration,
                       acknowledge, distro_name, drain, judge_recovery, linux_path, load_evidence, main,
                       parse_journal, snapshot_tools, windows_output)
from v020 import Client, HEADER, WireError, decode, encode, exact, packet


STREAM = "01" * 16


def event(key, shard, sequence):
    """独立构造完整身份；Construct complete identities independently."""
    return {"event_id": {"stream_id": STREAM, "db": 0, "shard": shard, "sequence": sequence},
            "event_type": "KEY_CREATED", "key": key, "generation": {"epoch": "02" * 16, "counter": 1},
            "revision": 1, "timestamp_ms": 1000}


def identity(value):
    """测试不使用生产 ID 辅助函数；Do not reuse the production identity helper."""
    value = value["event_id"]
    return value["stream_id"], value["db"], value["shard"], value["sequence"]


def record(events):
    """直接构造冻结 v1 字节；Build frozen v1 bytes directly."""
    payload = b"\x01" + struct.pack(">I", len(events))
    for value in events:
        eid, key = value["event_id"], value["key"].encode()
        payload += struct.pack(">HIQBI", eid["db"], eid["shard"], eid["sequence"], 1, len(key)) + key
        payload += bytes.fromhex(value["generation"]["epoch"]) + struct.pack(">QQQ", 1, 1, 1000)
    return struct.pack(">II", len(payload), zlib.crc32(payload)) + payload + b"LGCMT001"


def journal(events):
    """固定两分片头，独立 CRC；Two-shard header with independent CRC."""
    header = b"LGJRNL01" + struct.pack(">I", 1) + bytes.fromhex(STREAM) + struct.pack(">II", 16, 2)
    return header + struct.pack(">I", zlib.crc32(header)) + record(events)


class Guards(unittest.TestCase):
    """不调用 WSL 的定点操作保护；Targeted-operation safeguards without invoking WSL."""
    def test_distro_never_accepts_default_or_global_selectors(self):
        self.assertEqual(distro_name(DISTRO), DISTRO)
        for value in ("Ubuntu", "ubuntu", "default", "--shutdown", "", "ling-acceptance ", "LING-ACCEPTANCE"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                distro_name(value)

    def test_linux_paths_stay_inside_canonical_acceptance_root(self):
        self.assertEqual(linux_path(BASE + "/runs/one"), BASE + "/runs/one")
        for value in (BASE, "/", "/tmp/one", BASE + "-other/one", BASE + "/../source",
                      BASE + "/./run", BASE + "//run", BASE + "/run/", BASE + "/run;$HOME",
                      "C:/Windows", BASE + "/a\nb"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                linux_path(value)

    def test_host_output_rejects_wsl_unc_and_relative_paths(self):
        self.assertEqual(windows_output(r"C:\results\crash"), r"C:\results\crash")
        for value in ("results", r"C:results", r"\\wsl$\ling-acceptance\root\results",
                      r"\\wsl.localhost\Ubuntu\root", r"\\server\share\results", r"C:\results\..\other"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                windows_output(value)

    def test_termination_is_guarded_and_has_exact_distro(self):
        wsl = Wsl(DISTRO)
        result = Mock(returncode=0, stdout=b"", stderr=b"")
        callback = Mock()
        with patch.object(wsl, "run") as guard, patch("wsl_crash.subprocess.run", return_value=result) as run:
            wsl.terminate(BASE + "/run", BASE + "/ling", callback)
            guard.assert_called_once_with("guard", run=BASE + "/run", binary=BASE + "/ling")
            callback.assert_called_once()
            self.assertEqual(run.call_args.args[0], ["wsl.exe", "--terminate", DISTRO])
        with patch.object(wsl, "run", side_effect=RuntimeError("marker missing")), patch("wsl_crash.subprocess.run") as run:
            with self.assertRaises(RuntimeError):
                wsl.terminate(BASE + "/run", BASE + "/ling")
            run.assert_not_called()
        wsl.name = "Ubuntu"
        with patch.object(wsl, "run"), patch("wsl_crash.subprocess.run") as run:
            with self.assertRaises(ValueError):
                wsl.terminate(BASE + "/run", BASE + "/ling")
            run.assert_not_called()

    def test_linux_helper_is_valid_and_contains_marker_ext4_guards(self):
        compile(LINUX, "acceptance-linux-helper", "exec")
        self.assertIn("marker.read_text() != 'ling-acceptance\\n'", LINUX)
        self.assertIn("!= 'ext4'", LINUX)
        self.assertNotIn("fsync", LINUX)
        self.assertNotIn("--shutdown", LINUX)

    def test_controller_without_execute_never_calls_wsl(self):
        arguments = ["wsl_crash.py", "--binary", BASE + "/ling", "--output", r"C:\results\never-created"]
        with patch("sys.argv", arguments), patch("wsl_crash.subprocess.run") as run:
            with self.assertRaisesRegex(SystemExit, "No operations performed"):
                main()
            run.assert_not_called()

    def test_tool_source_evidence_preserves_actual_protocol_helper(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "tools"
            sources = snapshot_tools(output)
            self.assertEqual({item["copy"] for item in sources},
                             {"wsl_crash.py", "v020.py", "test_wsl_crash.py"})
            for source in sources:
                self.assertEqual((output / source["copy"]).read_bytes(), Path(source["source"]).read_bytes())
                self.assertEqual(len(source["sha256"]), 64)


class Journal(unittest.TestCase):
    """完整记录与明确短尾边界；Complete records and identifiable short-tail boundaries."""
    def test_two_shard_events_and_precise_tail_classification(self):
        events = [event("a", 1, 1), event("b", 0, 1)]
        data = journal(events)
        self.assertEqual(parse_journal(data)["events"], events)
        self.assertEqual(parse_journal(data)["status"], "valid")
        self.assertEqual(parse_journal(data + b"\x00\x00")["status"], "repairable_tail")
        self.assertEqual(parse_journal(data[:-3])["status"], "repairable_tail")
        self.assertEqual(parse_journal(data[:-12])["status"], "corrupt")
        damaged = bytearray(data)
        damaged[48] ^= 1
        self.assertEqual(parse_journal(bytes(damaged))["status"], "corrupt")

    def test_sequence_gaps_are_per_shard_and_partial_record_not_visible(self):
        first = event("a", 0, 1)
        self.assertEqual(parse_journal(journal([first, event("b", 0, 3)]))["status"], "corrupt")
        partial = journal([first]) + record([event("b", 0, 2)])[:-3]
        result = parse_journal(partial)
        self.assertEqual(result["status"], "repairable_tail")
        self.assertEqual(result["events"], [first])


class Recovery(unittest.TestCase):
    """未知提交与确认空洞的集合契约；Set contracts for unknown commits and ACK holes."""
    def fixture(self):
        """跨分片消费乱序仍然有效；Cross-shard delivery may arrive in either order."""
        events = [event("a", 0, 1), event("b", 1, 1), event("c", 0, 2), event("d", 1, 2)]
        before = {"batches": [{"batch": "known", "keys": ["a", "b", "c", "d"], "status": "success"}],
                  "acked": {identity(events[2])}, "unknown_acks": {identity(events[3])},
                  "seen": {identity(item): item for item in events}, "holes": {identity(events[0])}}
        parsed = parse_journal(journal(events))
        continuation = [event("next0", 0, 3), event("next1", 1, 3)]
        return before, parsed, [events[3], events[1], events[2], events[0]], [events[1], events[0]], continuation

    def judge(self, fixture):
        before, parsed, audit, work, continuation = fixture
        return judge_recovery(before, parsed, audit, work, STREAM, 0, continuation)

    def test_out_of_order_shards_and_unknown_ack_both_outcomes_pass(self):
        fixture = self.fixture()
        self.assertTrue(self.judge(fixture)["valid"])
        fixture[3].append(fixture[2][0])
        self.assertTrue(self.judge(fixture)["valid"])
        fixture[2].append(fixture[2][0])
        self.assertEqual(self.judge(fixture)["audit_duplicates"], 1)
        self.assertTrue(self.judge(fixture)["valid"])

    def test_unknown_write_all_or_none_but_never_partial(self):
        fixture = self.fixture()
        fixture[0]["batches"].append({"batch": "unknown", "keys": ["x", "y"], "status": "unknown"})
        self.assertTrue(self.judge(fixture)["valid"])
        unknown = [event("x", 0, 3), event("y", 1, 3)]
        fixture[1]["transactions"].append(unknown)
        fixture[1]["events"].extend(unknown)
        fixture[2].extend(unknown)
        fixture[3].extend(unknown)
        fixture[4][:] = [event("next0", 0, 4), event("next1", 1, 4)]
        self.assertTrue(self.judge(fixture)["valid"])
        fixture[1]["events"].pop()
        self.assertFalse(self.judge(fixture)["valid"])

    def test_confirmed_ack_missing_success_changed_stream_and_restored_cache_fail(self):
        fixture = self.fixture()
        fixture[3].append(fixture[2][2])
        self.assertIn("confirmed ACK was redelivered", self.judge(fixture)["failures"])
        fixture = self.fixture()
        fixture[1]["events"].pop()
        self.assertFalse(self.judge(fixture)["valid"])
        fixture = self.fixture()
        before, parsed, audit, work, continuation = fixture
        result = judge_recovery(before, parsed, audit, work, "ff" * 16, 1, continuation)
        self.assertIn("stream_id changed across recovery", result["failures"])
        self.assertIn("cache values were restored", result["failures"])

    def test_split_transaction_missing_hole_and_bad_sequence_fail(self):
        fixture = self.fixture()
        transaction = fixture[1]["transactions"][0]
        fixture[1]["transactions"] = [transaction[:2], transaction[2:]]
        self.assertIn("MSET was split into physical transactions", self.judge(fixture)["failures"])
        fixture = self.fixture()
        fixture[3].pop()
        self.assertFalse(self.judge(fixture)["valid"])
        fixture = self.fixture()
        fixture[4][0]["event_id"]["sequence"] += 1
        self.assertFalse(self.judge(fixture)["valid"])

    def test_work_metadata_must_equal_original_event(self):
        fixture = self.fixture()
        fixture[3][0] = dict(fixture[3][0], revision=99)
        self.assertIn("work-group replay metadata changed", self.judge(fixture)["failures"])

    def test_duplicate_audit_metadata_cannot_be_hidden_by_later_good_event(self):
        fixture = self.fixture()
        fixture[2].insert(0, dict(fixture[2][-1], revision=99))
        self.assertIn("audit replay differs from committed journal events", self.judge(fixture)["failures"])

    def test_ledger_distinguishes_intent_success_and_uncertain_ack(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "evidence.jsonl"
            evidence = Evidence(path)
            evidence.add("write_intent", batch="one", keys=["a"])
            evidence.add("write_intent", batch="two", keys=["b"])
            evidence.add("write_result", batch="one", status="success")
            value = event("a", 0, 1)
            evidence.add("event_before", event=value)
            evidence.add("ack_intent", id=identity(value), phase="before")
            evidence.close()
            loaded = load_evidence(path)
            self.assertEqual([batch["status"] for batch in loaded["batches"]], ["success", "unknown"])
            self.assertEqual(loaded["unknown_acks"], {identity(value)})
            self.assertEqual(loaded["acked"], set())

    def test_interleaved_ack_responses_drain_more_than_client_inbox_capacity(self):
        """真实字节流补满窗口，不靠增大缓存吞掉积压；Refill a real stream without enlarging Client's inbox."""
        self.assertIn(f"max_pending_events={MAX_PENDING_EVENTS}", configuration(12345))
        self.assertLess(MAX_PENDING_EVENTS, 256)
        self.assertGreater(MAX_PENDING_EVENTS, 64)
        expected = [event(f"key-{index}", index % 2, index // 2 + 1) for index in range(600)]
        left, right = socket.socketpair()
        left.settimeout(3)
        right.settimeout(3)
        client = Client.__new__(Client)
        client.stream, client.request_id, client.events, client.subscribed = left, 0, deque(), True
        acknowledgements, errors, records, high_water = [], [], [], [0]

        class Recorder:
            """保留意图与成功的先后；Retain intention/success order with bounded inbox observations."""
            def add(self, kind, **fields):
                records.append({"kind": kind, **fields})
                high_water[0] = max(high_water[0], len(client.events))

        def send_event(index):
            """测试自行生成线上二进制身份；Construct wire identity independently in the test."""
            value = expected[index]
            wire = dict(value, event_id=dict(value["event_id"], stream_id=bytes.fromhex(STREAM)),
                        generation=dict(value["generation"], epoch=bytes.fromhex("02" * 16)))
            right.sendall(packet(3, 0, encode(wire)))

        def serve():
            """每次 ACK 先推新事件再回响应，复现缓存压力；Push before each ACK reply to reproduce inbox pressure."""
            try:
                sent = min(MAX_PENDING_EVENTS, len(expected))
                for index in range(sent):
                    send_event(index)
                while True:
                    _, _, kind, _, request_id, size = HEADER.unpack(exact(right, HEADER.size))
                    payload = exact(right, size)
                    if kind == 4:
                        value, offset = decode(payload)
                        self.assertEqual(offset, size)
                        acknowledgements.append((value["stream_id"].hex(), value["db"], value["shard"], value["sequence"]))
                        if sent < len(expected):
                            send_event(sent)
                            sent += 1
                        right.sendall(packet(2, request_id, b"\0" + encode(True)))
                    else:
                        name, offset = decode(payload)
                        arguments, offset = decode(payload, offset)
                        self.assertEqual((name, arguments, offset), ("PING", [], size))
                        right.sendall(packet(2, request_id, b"\0" + encode("PONG")))
                        return
            except BaseException as error:
                errors.append(error)

        worker = threading.Thread(target=serve, daemon=True)
        worker.start()
        try:
            received = drain(client, {identity(value) for value in expected}, Recorder(), "audit", 8)
            self.assertEqual(received, expected)
            self.assertEqual(acknowledgements, [identity(value) for value in expected])
            self.assertLessEqual(high_water[0], MAX_PENDING_EVENTS)
            self.assertEqual([row["kind"] for row in records if row["kind"].startswith("ack_")],
                             [kind for _ in expected for kind in ("ack_intent", "ack_success")])
            self.assertTrue(records[-1]["quiet_fence_confirmed"])
        finally:
            client.close()
            worker.join(4)
            right.close()
        self.assertFalse(worker.is_alive())
        self.assertEqual(errors, [])

    def test_ack_without_complete_response_remains_unknown(self):
        """收到事件不能代替 ACK 成功响应；An interleaved event never substitutes for an ACK response."""
        left, right = socket.socketpair()
        left.settimeout(2)
        value = event("pending", 0, 1)
        client = Client.__new__(Client)
        client.stream, client.request_id, client.events, client.subscribed = left, 0, deque(), True
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "evidence.jsonl"
            evidence = Evidence(path)
            wire = dict(value, event_id=dict(value["event_id"], stream_id=bytes.fromhex(STREAM)),
                        generation=dict(value["generation"], epoch=bytes.fromhex("02" * 16)))
            right.sendall(packet(3, 0, encode(wire)))
            right.shutdown(socket.SHUT_WR)
            try:
                with self.assertRaisesRegex(WireError, "LOCAL_EOF"):
                    acknowledge(client, value, evidence, "before")
            finally:
                evidence.close()
                client.close()
                right.close()
            loaded = load_evidence(path)
            self.assertEqual(loaded["acked"], set())
            self.assertEqual(loaded["unknown_acks"], {identity(value)})


if __name__ == "__main__":
    unittest.main()
