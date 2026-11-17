"""0.2 测量工具的协议和验收契约；Protocol and acceptance contracts for the 0.2 benchmark."""
import copy
import socket
import struct
import threading
import unittest

from v020 import WireError, acceptance, decode, encode, packet, quantiles, receive


class Protocol(unittest.TestCase):
    """独立黄金字节和分段读取；Independent golden bytes and fragmented reads."""
    def test_hello_and_ping_golden_bytes(self):
        hello = bytes.fromhex("4c4701010000000000000000000100000018"
                              "050000000548454c4c4f0700000001030000000000000001")
        self.assertEqual(packet(1, 1, encode("HELLO") + encode([1])), hello)
        ping = bytes.fromhex("4c470101000000000000000000020000000e"
                             "050000000450494e470700000000")
        self.assertEqual(packet(1, 2, encode("PING") + encode([])), ping)

    def test_fragmented_success_and_fatal_error(self):
        left, right = socket.socketpair()
        try:
            wire = bytes.fromhex("4c470102000000000000000000020000000a"
                                 "000500000004504f4e47")
            def send():
                """独立拆分固定响应；Fragment an independently encoded response."""
                for byte in wire:
                    right.sendall(bytes([byte]))
            worker = threading.Thread(target=send)
            worker.start()
            self.assertEqual(receive(left), (2, 2, "PONG"))
            worker.join()
            payload = encode("ERR_BUSY") + encode("Connection limit")
            right.sendall(packet(6, 0, payload))
            with self.assertRaisesRegex(WireError, "ERR_BUSY"):
                receive(left)
        finally:
            left.close()
            right.close()

    def test_lossless_values_and_reject_bad_lengths(self):
        self.assertEqual(decode(b"\x03" + struct.pack(">Q", 2**64 - 1))[0], 2**64 - 1)
        for data in (b"\x05\xff\xff\xff\xff", b"\x07\xff\xff\xff\xff", b"\x01\x02", b"\x03\x00"):
            with self.assertRaises(WireError):
                decode(data)
        self.assertEqual(decode(encode({"bytes": b"\x00\xff", "flag": False}))[0],
                         {"bytes": b"\x00\xff", "flag": False})


class Acceptance(unittest.TestCase):
    """消费错误独立于生产成功；Consumer failures are independent of producer success."""
    def row(self):
        """有积压的完整确认样本；A completely acknowledged backlog sample."""
        return {"scenario": "backlog", "notification_mode": "on", "errors": {}, "successes": 10,
                "consumer": {"received": 10, "acks": 10, "errors": {}, "termination": "requested_stop"}}

    def test_consumer_early_failure_or_missing_ack_invalidates(self):
        row = self.row()
        self.assertTrue(acceptance(row)["valid"])
        for changes in ({"termination": "error"}, {"acks": 9}, {"duplicates": 1}, {"errors": {"ERR_OFFSET_EXPIRED": 1}},
                        {"termination": "running"}):
            broken = copy.deepcopy(row)
            broken["consumer"].update(changes)
            self.assertFalse(acceptance(broken)["valid"])

    def test_idle_notification_poll_and_duration_contract(self):
        row = self.row()
        row.update(scenario="idle-100", subscription_poll_delta=0, requested_seconds=3, elapsed_seconds=3)
        self.assertTrue(acceptance(row)["valid"])
        row["subscription_poll_delta"] = 1
        self.assertFalse(acceptance(row)["valid"])
        row["notification_mode"] = "legacy"
        self.assertTrue(acceptance(row)["valid"])
        row["elapsed_seconds"] = 1
        self.assertFalse(acceptance(row)["valid"])

    def test_nearest_rank_is_per_run(self):
        self.assertEqual(quantiles([4000, 1000, 3000, 2000]),
                         {"p50_us": 2, "p99_us": 4, "p999_us": 4})
        self.assertIsNone(quantiles([])["p50_us"])


if __name__ == "__main__":
    unittest.main()
