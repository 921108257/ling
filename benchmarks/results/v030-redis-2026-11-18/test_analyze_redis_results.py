"""临时分析器离线证据测试；Offline evidence tests without WSL or product processes."""
import contextlib
import copy
import gzip
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("redis_audit", Path(__file__).with_name("analyze-redis-results.py"))
AUDIT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(AUDIT)


def write_json(path, value):
    """只构造临时夹具；Write test fixtures only."""
    path.write_text(json.dumps(value), encoding="utf-8")


@contextlib.contextmanager
def complete_evidence():
    """72轮各四个已知样本，分位数独立手算；72 cases with four independently known samples each."""
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        (root / "inputs").mkdir()
        inputs = {}
        for name in ("controller", "protocol_helper", "tests", "ling", "redis", "redis_source_archive"):
            path = root / "inputs" / name
            path.write_bytes((name + " fixture, never executable").encode())
            inputs[name] = {"copy": str(path.relative_to(root)), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
        fixture_identity = dict(AUDIT.EXPECTED_IDENTITY)
        for label, key in (("ling", "ling_sha256"), ("redis", "redis_sha256"),
                           ("redis_source_archive", "redis_source_sha256")):
            fixture_identity[key] = inputs[label]["sha256"]
        environment = {"arguments": dict(AUDIT.FORMAL_ARGUMENTS), "inputs": inputs, "pipeline": 1, "consumers": 0,
            "ling_revision_operator_asserted": fixture_identity["ling_revision"],
            "redis_pin": {"version": "8.10.3", "sha256": inputs["redis_source_archive"]["sha256"]},
            "redis_source_archive_verified": True,
            "binary_versions": {"ling": {"returncode": 0, "stdout": "ling 0.2.0", "stderr": ""},
                                "redis": {"returncode": 0, "stdout": "Redis server v=8.10.3 sha=fixture", "stderr": ""}}}
        rows, schedule, groups = [], [], []
        elapsed = 10.000004
        metrics = {"successes_per_second": 4 / elapsed, "p50_us": 2.0, "p95_us": 4.0,
                   "p99_us": 4.0, "p999_us": 4.0, "server_cpu_seconds": 1.0, "client_cpu_seconds": .1}
        for mode in ("fast", "reliable"):
            for kind in ("string", "bytes"):
                for operation in ("get", "set", "set-px"):
                    for repeat in (1, 2, 3):
                        for system in ("ling", "redis"):
                            case = dict(mode=mode, kind=kind, operation=operation, repeat=repeat, system=system)
                            schedule.append(case)
                            row = case | dict(valid=True, errors=[], threads_alive=0, connections=4, pipeline=1,
                                worker_counts=[1, 1, 1, 1], successes=4, start_ns=1_000_000_000,
                                planned_end_ns=11_000_000_000, measured_end_ns=11_000_004_000,
                                elapsed_seconds=elapsed, resources_before={"cpu_seconds": 1, "rss_bytes": 100,
                                "peak_rss_bytes": 200}, resources_after={"cpu_seconds": 2, "rss_bytes": 150,
                                "peak_rss_bytes": 200}, **metrics)
                            rows.append(row)
                            directory = root / "-".join(str(case[key]) for key in AUDIT.IDENTITY)
                            directory.mkdir()
                            write_json(directory / "result.json", row)
                            with gzip.open(directory / "latencies-ns.json.gz", "wt", encoding="ascii") as output:
                                output.write("[4000,1000,3000,2000]\n")
                    for system in ("ling", "redis"):
                        groups.append(dict(mode=mode, kind=kind, operation=operation, system=system,
                            repeats=3, all_valid=True, ranges={metric: {"min": value, "median": value, "max": value}
                                                             for metric, value in metrics.items()}))
        groups.sort(key=lambda row: tuple(row[key] for key in AUDIT.GROUP))
        summary = dict(valid=True, planned_cases=72, executed_cases=72, results=rows, groups=groups)
        for name, value in (("environment", environment), ("schedule", schedule), ("results", rows), ("summary", summary)):
            write_json(root / (name + ".json"), value)
        # 测试夹具显式替换外部配置；审计器仍不得从 environment 推导这些预期值。
        # Fake fixture bytes need explicit external test pins, never self-reported expectations.
        with patch.dict(AUDIT.EXPECTED_IDENTITY, fixture_identity, clear=True):
            yield root, rows, environment, summary


def first_case(root, rows):
    """确定夹具首轮路径；Locate the first fixture repeat."""
    return root / "-".join(str(rows[0][key]) for key in AUDIT.IDENTITY)


class RedisAuditTests(unittest.TestCase):
    """从损坏证据验证核验器，不运行受测工具；Exercise bad evidence without running the load tool."""
    def test_complete_matrix_recomputes_raw_quantiles_rate_and_cpu(self):
        with complete_evidence() as (root, rows, environment, summary):
            before = {str(path.relative_to(root)): path.read_bytes() for path in root.rglob("*") if path.is_file()}
            report = AUDIT.audit(root)
            self.assertTrue(report["valid"], report)
            self.assertEqual((report["case_count"], report["group_count"]), (72, 24))
            case = report["cases"][0]
            self.assertEqual([case[field] for field in ("p50_us", "p95_us", "p99_us", "p999_us")], [2, 4, 4, 4])
            self.assertEqual(case["server_cpu_seconds"], 1)
            self.assertEqual(case["successes_per_second"], 4 / 10.000004)
            self.assertEqual(before, {str(path.relative_to(root)): path.read_bytes() for path in root.rglob("*") if path.is_file()})

    def test_public_acceptance_identity_is_fixed_outside_evidence(self):
        self.assertEqual(AUDIT.EXPECTED_IDENTITY, {
            "ling_sha256": "70b209554df2f236c6c4c30d6bbae5ec001a3b2754761038265483f659a0f285",
            "ling_revision": "441ffc7dbb2bf48d6257c0ca3b831f099554af1a", "ling_version": "ling 0.2.0",
            "redis_sha256": "1ea6a05a0108e60c7a4656592138714da2c21888f72b2937301eed487f6b778a",
            "redis_version": "8.10.3",
            "redis_source_sha256": "edde778f6bdee7581e42fc139ca9e07e2822212092f7501f99989656e3629d02"})

    def test_self_consistent_manifest_cannot_choose_release_or_redis_identity(self):
        for label in ("ling", "redis", "redis_source_archive"):
            with self.subTest(label=label), complete_evidence() as (root, rows, environment, summary):
                path = root / environment["inputs"][label]["copy"]
                path.write_bytes(b"different but self-consistent input")
                environment["inputs"][label]["sha256"] = hashlib.sha256(path.read_bytes()).hexdigest()
                if label == "redis_source_archive":
                    environment["redis_pin"]["sha256"] = environment["inputs"][label]["sha256"]
                write_json(root / "environment.json", environment)
                report = AUDIT.audit(root)
                self.assertTrue(report["inputs"][label]["valid"])
                self.assertFalse(report["valid"])
                self.assertIn("expected input identity mismatch: " + label + "_sha256", report["errors"])

    def test_recorded_revision_versions_pin_and_archive_status_must_match(self):
        mutations = {
            "ling_revision": lambda env: env.update(ling_revision_operator_asserted="wrong"),
            "redis_pin": lambda env: env["redis_pin"].update(version="8.10.30"),
            "redis_source_archive_verified": lambda env: env.update(redis_source_archive_verified=False),
            "ling_version": lambda env: env["binary_versions"]["ling"].update(stdout="ling 0.2.1"),
            "redis_version": lambda env: env["binary_versions"]["redis"].update(stdout="Redis server v=8.10.30"),
        }
        for field, mutate in mutations.items():
            with self.subTest(field=field), complete_evidence() as (root, rows, environment, summary):
                mutate(environment)
                write_json(root / "environment.json", environment)
                report = AUDIT.audit(root)
                self.assertFalse(report["valid"])
                self.assertIn("expected input identity mismatch: " + field, report["errors"])

    def test_integer_parser_streams_across_chunks(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "samples.gz"
            with gzip.open(path, "wt", encoding="ascii") as output:
                output.write("[" + " " * 65533 + "18446744073709551615,0,17]\n")
            self.assertEqual(list(AUDIT.read_latencies(path)), [2**64 - 1, 0, 17])

    def test_parser_rejects_bad_numbers_array_shapes_and_extra_data(self):
        for payload in ("[01]", "[-1]", "[1.0]", "[true]", "[NaN]", "[1,]", "[1 2]",
                        "[18446744073709551616]", "[1", "[1][]", "{}", "[1e3]", "[1,\v2]"):
            with self.subTest(payload=payload), tempfile.TemporaryDirectory() as temporary:
                path = Path(temporary) / "bad.gz"
                with gzip.open(path, "wt", encoding="ascii") as output:
                    output.write(payload)
                with self.assertRaises(ValueError):
                    AUDIT.read_latencies(path)

    def test_parser_enforces_sample_and_byte_budgets(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "samples.gz"
            with gzip.open(path, "wt", encoding="ascii") as output:
                output.write("[1,2,3]")
            with self.assertRaisesRegex(ValueError, "sample budget"):
                AUDIT.read_latencies(path, limit=2)
            previous = AUDIT.MAX_RAW_BYTES
            try:
                AUDIT.MAX_RAW_BYTES = 5
                with self.assertRaisesRegex(ValueError, "byte budget"):
                    AUDIT.read_latencies(path)
            finally:
                AUDIT.MAX_RAW_BYTES = previous

    def test_gzip_crc_and_truncated_trailer_reject_complete_json(self):
        for mutation in ("crc", "truncated"):
            with self.subTest(mutation=mutation), complete_evidence() as (root, rows, environment, summary):
                path = first_case(root, rows) / "latencies-ns.json.gz"
                data = bytearray(path.read_bytes())
                if mutation == "crc":
                    data[-8] ^= 0xff
                else:
                    data = data[:-4]
                path.write_bytes(data)
                report = AUDIT.audit(root)
                self.assertFalse(report["valid"])
                self.assertTrue(any("unreadable" in reason for reason in report["cases"][0]["errors"]))

    def test_missing_or_extra_raw_samples_do_not_pass_aggregate_counts(self):
        for samples in ([1000, 2000, 3000], [1000, 2000, 3000, 4000, 5000]):
            with self.subTest(samples=samples), complete_evidence() as (root, rows, environment, summary):
                with gzip.open(first_case(root, rows) / "latencies-ns.json.gz", "wt", encoding="ascii") as output:
                    json.dump(samples, output)
                report = AUDIT.audit(root)
                self.assertFalse(report["valid"])
                self.assertIn("success count differs from raw samples", report["cases"][0]["errors"])

    def test_raw_percentile_mismatch_is_detected(self):
        with complete_evidence() as (root, rows, environment, summary):
            with gzip.open(first_case(root, rows) / "latencies-ns.json.gz", "wt", encoding="ascii") as output:
                json.dump([1000, 2000, 3000, 9000], output)
            report = AUDIT.audit(root)
            self.assertIn("p99_us differs from raw nearest rank", report["cases"][0]["errors"])

    def test_claimed_ten_second_rate_rejects_tail_completion(self):
        with complete_evidence() as (root, rows, environment, summary):
            rows[0]["successes_per_second"] = .4
            write_json(first_case(root, rows) / "result.json", rows[0])
            write_json(root / "results.json", rows)
            report = AUDIT.audit(root)
            self.assertIn("throughput mismatch", report["cases"][0]["errors"])

    def test_duplicate_repeat_and_missing_case_are_rejected(self):
        with complete_evidence() as (root, rows, environment, summary):
            rows[2] = copy.deepcopy(rows[0])
            write_json(root / "results.json", rows)
            report = AUDIT.audit(root)
            self.assertFalse(report["valid"])
            self.assertTrue(any("72-case matrix" in reason for reason in report["errors"]))
            self.assertTrue(any("repeat identities" in reason for reason in report["errors"]))

    def test_failed_row_even_with_existing_statistics_is_rejected(self):
        with complete_evidence() as (root, rows, environment, summary):
            rows[0].update(valid=False, errors=[{"phase": "measured", "error": "timeout"}], threads_alive=1)
            write_json(root / "results.json", rows)
            write_json(first_case(root, rows) / "result.json", rows[0])
            report = AUDIT.audit(root)
            self.assertFalse(report["valid"])
            self.assertEqual(len(report["cases"][0]["errors"]), 3)

    def test_cpu_endpoint_delta_and_rss_scope_are_checked(self):
        with complete_evidence() as (root, rows, environment, summary):
            rows[0]["server_cpu_seconds"] = 0
            rows[0]["resources_after"]["peak_rss_bytes"] = 90
            write_json(root / "results.json", rows)
            write_json(first_case(root, rows) / "result.json", rows[0])
            report = AUDIT.audit(root)
            self.assertIn("server CPU endpoint delta mismatch", report["cases"][0]["errors"])
            self.assertIn("after RSS / lifetime peak invalid or unavailable", report["cases"][0]["errors"])

    def test_frozen_input_hash_and_input_directory_boundary_are_enforced(self):
        for mutation in ("hash", "escape"):
            with self.subTest(mutation=mutation), complete_evidence() as (root, rows, environment, summary):
                if mutation == "hash":
                    (root / "inputs" / "ling").write_bytes(b"changed")
                else:
                    (root / "outside-inputs").write_bytes(b"not an input")
                    environment["inputs"]["ling"]["copy"] = "outside-inputs"
                    write_json(root / "environment.json", environment)
                report = AUDIT.audit(root)
                self.assertFalse(report["valid"])
                self.assertTrue(any("input" in reason for reason in report["errors"]))

    def test_incomplete_summary_and_group_ranges_are_rejected(self):
        with complete_evidence() as (root, rows, environment, summary):
            summary["executed_cases"] = 71
            summary["groups"][0]["ranges"]["p99_us"]["median"] = 1
            write_json(root / "summary.json", summary)
            report = AUDIT.audit(root)
            self.assertIn("summary is incomplete or invalid", report["errors"])
            self.assertIn("summary group ranges differ from independent aggregation", report["errors"])

    def test_formal_arguments_and_interleaving_are_checked(self):
        with complete_evidence() as (root, rows, environment, summary):
            environment["arguments"]["seconds"] = 3
            write_json(root / "environment.json", environment)
            schedule = json.loads((root / "schedule.json").read_text())
            schedule[1], schedule[2] = schedule[2], schedule[1]
            write_json(root / "schedule.json", schedule)
            report = AUDIT.audit(root)
            self.assertIn("formal run arguments mismatch", report["errors"])
            self.assertIn("schedule is not adjacent interleaved system pairs", report["errors"])

    def test_duplicate_json_keys_and_nonfinite_numbers_are_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "metadata.json"
            for payload in ('{"valid":true,"valid":false}', '{"value":Infinity}'):
                with self.subTest(payload=payload):
                    path.write_text(payload, encoding="utf-8")
                    with self.assertRaises(ValueError):
                        AUDIT.read_json(path)

    def test_malformed_resource_object_returns_invalid_report(self):
        with complete_evidence() as (root, rows, environment, summary):
            rows[0]["resources_before"] = []
            write_json(root / "results.json", rows)
            write_json(first_case(root, rows) / "result.json", rows[0])
            report = AUDIT.audit(root)
            self.assertFalse(report["valid"])
            self.assertTrue(any("malformed" in reason for reason in report["cases"][0]["errors"]))


if __name__ == "__main__":
    unittest.main()
