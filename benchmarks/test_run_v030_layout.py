"""编排器的纯离线回归；Exercise scheduling, coverage and cleanup without Linux or real processes."""
import importlib.util
from pathlib import Path
import signal
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location("v030_layout", Path(__file__).with_name("run-v030-layout.py"))
layout = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(layout)


class Clock:
    """推进虚拟时间，不真实等待；Advance virtual time without sleeping."""
    def __init__(self):
        self.now = 0

    def monotonic(self):
        return self.now

    def sleep(self, seconds):
        self.now += seconds


class Process:
    """只保存模拟退出状态；Keep a simulated exit state without starting a process."""
    def __init__(self, pid, code=None):
        self.pid, self.code = pid, code

    def poll(self):
        return self.code


def identity(pid, start=10, session=None):
    """构造明确的私有 session；Build an explicit private-session identity."""
    return {"pid": pid, "pgrp": session or pid, "session": session or pid, "start_ticks": start}


def good_result(start=100, seconds=3300, phase="shadow"):
    """保存真实区间必需字段；Provide the fields needed to prove an actual load interval."""
    return {"status": "duration_reached", "server_exit_code": 0, "findings": {},
            "criteria": {"no_findings": True, "full_duration": True, "all_workers_completed": True,
                         "consumers_valid": True, "native_resources_available": True,
                         "all_ten_types_checked": True if phase == "shadow" else None,
                         "shadow_writes_not_capacity_limited": True if phase == "shadow" else None,
                         "resource_trends": "recorded; compare against the separately committed acceptance plan"},
            "load_started_monotonic_seconds": start, "load_stop_requested_elapsed_seconds": seconds}


class LayoutTests(unittest.TestCase):
    """每项独立临时目录，禁止真实 Popen 和信号；Use temporary files and mocked process boundaries."""
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.args = SimpleNamespace(output=self.root / "out", source=self.root / "source",
                                    binary=self.root / "ling", seconds=600, rate=5)
        self.args.output.mkdir()
        self.entries = layout.plan(self.args)

    def results(self):
        """十个 pressure 均落在两路实际 shadow 内；Place ten pressure phases inside both actual shadows."""
        for entry in self.entries:
            for number, directory in enumerate(layout.phase_paths(entry)):
                directory.mkdir(parents=True)
                start = (100 if entry["name"] == "fast-shadow" else 101) if entry["phase"] == "shadow" else 110 + number * 605
                layout.save(directory / "result.json", good_result(start, entry["seconds"], entry["phase"]))

    def test_planned_order_and_both_duration_budgets(self):
        """长跑额外 300 秒必须反映在计划；The 72-hour shadow plan includes its extra 300 seconds."""
        self.assertEqual([row["name"] for row in self.entries],
                         ["fast-shadow", "reliable-shadow", "fast-pressure", "reliable-pressure"])
        self.assertEqual([row["seconds"] for row in self.entries], [3300, 3300, 600, 600])
        self.args.seconds = 259200
        self.assertEqual([row["seconds"] for row in layout.plan(self.args)], [259500, 259500, 51840, 51840])
        self.assertEqual([len(layout.phase_paths(row)) for row in self.entries], [1, 1, 5, 5])

    def test_readiness_requires_both_workers_and_finite_start(self):
        """首份零周期进度不能启动 pressure；An initial zero-cycle progress file cannot release pressure."""
        directory = layout.phase_paths(self.entries[0])[0]
        directory.mkdir(parents=True)
        row = {"load_started_monotonic_seconds": 100, "counts": {"worker_0_cycles": 1, "worker_1_cycles": 0}}
        layout.save(directory / "progress.json", row)
        self.assertFalse(layout.shadow_ready(self.entries[0]))
        row["counts"]["worker_1_cycles"] = 1
        layout.save(directory / "progress.json", row)
        self.assertTrue(layout.shadow_ready(self.entries[0]))
        row["load_started_monotonic_seconds"] = True
        layout.save(directory / "progress.json", row)
        self.assertFalse(layout.shadow_ready(self.entries[0]))

    def test_actual_common_coverage_accepts_all_ten_phases(self):
        """实际区间而非配置预算决定覆盖；Actual timestamps, rather than budgets, prove coverage."""
        self.results()
        result = layout.coverage(self.entries)
        self.assertTrue(result["passed"], result["errors"])
        self.assertEqual(result["common_shadow_interval"], {"start": 101, "end": 3400})
        self.assertEqual(len(result["intervals"]), 12)

    def test_pressure_outside_either_shadow_is_rejected(self):
        """pressure 早一秒或晚一秒均不能通过；Reject pressure that starts early or ends late."""
        self.results()
        directory = layout.phase_paths(self.entries[-1])[-1]
        for start in (100, 2801):
            with self.subTest(start=start):
                layout.save(directory / "result.json", good_result(start, 600, "pressure"))
                result = layout.coverage(self.entries)
                self.assertFalse(result["passed"])
                self.assertTrue(any("outside shadows" in error for error in result["errors"]))

    def test_missing_policy_cannot_be_replaced_by_extra_result(self):
        """遗漏策略不得用任意其他文件补足；An unrelated result cannot replace a missing planned policy."""
        self.results()
        layout.phase_paths(self.entries[2])[2].joinpath("result.json").unlink()
        extra = self.entries[2]["directory"] / "unplanned"
        extra.mkdir()
        layout.save(extra / "result.json", good_result(120, 600, "pressure"))
        self.assertFalse(layout.coverage(self.entries)["passed"])

    def test_incomplete_and_failed_phase_results_are_rejected(self):
        """短跑、崩溃、发现和验收失败均保留为失败；Reject incomplete or failed phase evidence."""
        mutations = [{"load_stop_requested_elapsed_seconds": 599}, {"status": "ended_early"},
                     {"server_exit_code": -9}, {"server_exit_code": False}, {"findings": {"broken": 1}},
                     {"criteria": {"full_duration": False}}, {"criteria": None},
                     {"load_started_monotonic_seconds": float("nan")},
                     {"load_stop_requested_elapsed_seconds": True}]
        for changes in mutations:
            with self.subTest(changes=changes):
                self.assertIsNotNone(layout.phase_problem({**good_result(seconds=600), **changes}, 600, "shadow"))

    def test_required_criteria_reject_missing_null_false_and_numeric_values(self):
        """关键项缺失、null、假或数字不能当通过；Only boolean true can satisfy required criteria."""
        common = ("no_findings", "full_duration", "all_workers_completed", "consumers_valid", "native_resources_available")
        for phase in ("shadow", "pressure"):
            required = common + (("all_ten_types_checked", "shadow_writes_not_capacity_limited") if phase == "shadow" else ())
            for name in required:
                for value in ("missing", None, False, 0, 1, 1.0, "true"):
                    with self.subTest(phase=phase, name=name, value=value):
                        row = good_result(seconds=600, phase=phase)
                        if value == "missing":
                            del row["criteria"][name]
                        else:
                            row["criteria"][name] = value
                        self.assertIsNotNone(layout.phase_problem(row, 600, phase))

    def test_pressure_accepts_explicitly_inapplicable_shadow_criteria(self):
        """pressure 的两项 shadow null 与资源趋势说明不应误报；Accept explicit inapplicability and trend text."""
        row = good_result(seconds=600, phase="pressure")
        self.assertIsNone(row["criteria"]["all_ten_types_checked"])
        self.assertIsNone(row["criteria"]["shadow_writes_not_capacity_limited"])
        self.assertIsNone(layout.phase_problem(row, 600, "pressure"))
        self.assertIsNone(layout.phase_problem(good_result(seconds=600), 600, "shadow"))

    def test_monitor_and_coverage_reject_missing_required_evidence(self):
        """轮询和最终覆盖都必须使用强制关键项检查；Both monitoring and final coverage enforce required evidence."""
        self.results()
        entry = self.entries[-1]
        path = layout.phase_paths(entry)[0] / "result.json"
        row = layout.read_json(path)
        del row["criteria"]["native_resources_available"]
        layout.save(path, row)
        entry["process"] = Process(1000)
        self.assertIn("native_resources_available", layout.monitor_problem(self.entries))
        actual = layout.coverage(self.entries)
        self.assertFalse(actual["passed"])
        self.assertTrue(any("native_resources_available" in error for error in actual["errors"]))

    def test_partially_written_json_is_retried(self):
        """未写完的结果不会变成完整成功；A partial JSON write remains unavailable."""
        path = self.root / "partial.json"
        path.write_text('{"status": ', encoding="utf-8")
        self.assertIsNone(layout.read_json(path))

    def test_progress_findings_stop_monitor_before_final_result(self):
        """运行中发现不等待阶段结果；A live finding fails monitoring before a final result exists."""
        entry = self.entries[-1]
        entry["process"] = Process(1000)
        directory = layout.phase_paths(entry)[0]
        directory.mkdir(parents=True)
        layout.save(directory / "progress.json", {"findings": {"consumer_failure": 1}})
        self.assertEqual(layout.monitor_problem(self.entries), f"{directory.name}: progress reported findings")
        self.assertFalse((directory / "result.json").exists())

    def test_missing_or_incomplete_progress_is_retried(self):
        """缺失和写中进度不误判发现；Retry missing and partially written progress without false findings."""
        entry = self.entries[-1]
        entry["process"] = Process(1000)
        directory = layout.phase_paths(entry)[0]
        directory.mkdir(parents=True)
        self.assertIsNone(layout.monitor_problem(self.entries))
        path = directory / "progress.json"
        path.write_text('{"findings": ', encoding="utf-8")
        self.assertIsNone(layout.monitor_problem(self.entries))
        layout.save(path, {"findings": {}})
        self.assertIsNone(layout.monitor_problem(self.entries))

    def test_monitor_detects_later_pressure_exit_while_shadows_live(self):
        """后启动驱动失败必须即时发现；A later pressure failure must not wait for live shadows."""
        self.results()
        clock, launched = Clock(), []

        def popen(command, **kwargs):
            entry = next(row for row in self.entries if row["command"] == command)
            process = Process(1000 + len(launched), 9 if entry["name"] == "reliable-pressure" else None)
            launched.append((entry["name"], process))
            return process

        def cleanup(entries, output, events, errors):
            for entry in entries:
                if "process" in entry and entry["process"].code is None:
                    entry["process"].code = 130
            return []

        with patch.object(layout.subprocess, "Popen", side_effect=popen), \
                patch.object(layout, "proc_stat", side_effect=lambda pid: identity(pid)), \
                patch.object(layout, "process_table", return_value=[]), \
                patch.object(layout, "shadow_ready", return_value=True), \
                patch.object(layout, "cleanup", side_effect=cleanup), \
                patch.object(layout.time, "monotonic", side_effect=clock.monotonic), \
                patch.object(layout.time, "sleep", side_effect=clock.sleep):
            self.assertEqual(layout.orchestrate(self.args, self.entries, {}), 1)
        result = layout.read_json(self.args.output / "layout-result.json")
        self.assertTrue(any("reliable-pressure: driver exit 9" in error for error in result["errors"]))
        self.assertLess(clock.now, 1)
        self.assertEqual({row["name"]: row["exit_code"] for row in result["driver_exits"]},
                         {"fast-shadow": 130, "reliable-shadow": 130, "fast-pressure": 130, "reliable-pressure": 9})

    def test_pressure_waits_for_both_shadows(self):
        """调度门槛必须在两路进展之后；Both shadow readiness gates precede any pressure start."""
        self.results()
        clock, launches = Clock(), []

        def popen(command, **kwargs):
            entry = next(row for row in self.entries if row["command"] == command)
            process = Process(1000 + len(launches))
            launches.append((entry["name"], clock.now, process))
            if len(launches) == 4:
                for _, _, child in launches:
                    child.code = 0
            return process

        def ready(entry):
            return clock.now >= (.2 if entry["name"] == "fast-shadow" else .6)

        with patch.object(layout.subprocess, "Popen", side_effect=popen), \
                patch.object(layout, "proc_stat", side_effect=lambda pid: identity(pid)), \
                patch.object(layout, "process_table", return_value=[]), \
                patch.object(layout, "shadow_ready", side_effect=ready), \
                patch.object(layout, "cleanup", return_value=[]), \
                patch.object(layout.time, "monotonic", side_effect=clock.monotonic), \
                patch.object(layout.time, "sleep", side_effect=clock.sleep):
            self.assertEqual(layout.orchestrate(self.args, self.entries, {}), 0)
        self.assertEqual([when for _, when, _ in launches[:2]], [0, 0])
        self.assertTrue(all(when >= .6 for _, when, _ in launches[2:]))

    def test_startup_exception_still_saves_all_launched_exits(self):
        """启动中途失败也记录先前进程；Retain all previously launched exits after a later spawn failure."""
        children = [Process(1000), Process(1001)]

        def cleanup(entries, output, events, errors):
            for process in children:
                process.code = 130
            return []

        with patch.object(layout.subprocess, "Popen", side_effect=children + [OSError("spawn failed")]), \
                patch.object(layout, "proc_stat", side_effect=lambda pid: identity(pid)), \
                patch.object(layout, "process_table", return_value=[]), \
                patch.object(layout, "shadow_ready", return_value=True), \
                patch.object(layout, "cleanup", side_effect=cleanup):
            self.assertEqual(layout.orchestrate(self.args, self.entries, {}), 1)
        rows = layout.read_json(self.args.output / "driver-exits.json")
        self.assertEqual([row["exit_code"] for row in rows], [130, 130])
        self.assertEqual(len(rows), 2)

    def test_keyboard_interrupt_still_saves_exit_and_result(self):
        """中断不省略退出码；An interrupt still persists all launched exit codes."""
        children = [Process(1000), Process(1001)]

        def cleanup(entries, output, events, errors):
            for process in children:
                process.code = 130
            return []

        with patch.object(layout.subprocess, "Popen", side_effect=children), \
                patch.object(layout, "proc_stat", side_effect=lambda pid: identity(pid)), \
                patch.object(layout, "process_table", return_value=[]), \
                patch.object(layout, "shadow_ready", return_value=False), \
                patch.object(layout, "cleanup", side_effect=cleanup), \
                patch.object(layout.time, "sleep", side_effect=KeyboardInterrupt):
            self.assertEqual(layout.orchestrate(self.args, self.entries, {}), 130)
        self.assertEqual(len(layout.read_json(self.args.output / "driver-exits.json")), 2)
        self.assertFalse(layout.read_json(self.args.output / "layout-result.json")["orchestration_passed"])

    def test_final_exit_write_failure_still_closes_logs_and_saves_result(self):
        """退出文件一次写失败不截断收尾；A failed exit-file write cannot skip log closure or final evidence."""
        children, logs = [Process(1000), Process(1001)], []
        save = layout.save

        def popen(command, **kwargs):
            logs.append(kwargs["stdout"])
            return children[len(logs) - 1]

        def cleanup(entries, output, events, errors):
            for child in children:
                child.code = 130
            return []

        def save_once(path, data):
            if path.name == "driver-exits.json":
                raise OSError("single exit-file write failure")
            return save(path, data)

        with patch.object(layout.subprocess, "Popen", side_effect=popen), \
                patch.object(layout, "proc_stat", side_effect=lambda pid: identity(pid)), \
                patch.object(layout, "process_table", return_value=[]), \
                patch.object(layout, "shadow_ready", return_value=False), \
                patch.object(layout, "cleanup", side_effect=cleanup), \
                patch.object(layout.time, "sleep", side_effect=KeyboardInterrupt), \
                patch.object(layout, "save", side_effect=save_once):
            self.assertEqual(layout.orchestrate(self.args, self.entries, {}), 130)
        self.assertEqual(len(logs), 2)
        self.assertTrue(all(log.closed for log in logs))
        result = layout.read_json(self.args.output / "layout-result.json")
        self.assertFalse(result["orchestration_passed"])
        self.assertEqual([row["exit_code"] for row in result["driver_exits"]], [130, 130])
        self.assertTrue(any("final exit recording OSError: single exit-file write failure" in error
                            for error in result["errors"]))

    def test_orphan_owned_server_is_cleaned_after_driver_exit(self):
        """驱动已退出不代表它的服务已退出；Clean a verified server even when its driver has exited."""
        entry, clock = self.entries[0], Clock()
        driver, server = identity(1000), identity(1001, 11, 1000)
        entry.update(process=Process(1000, 0), identity=driver, owned={1000: 10, 1001: 11})
        alive, signals = [server], []

        def stop(row, sig, events):
            signals.append((row, sig))
            alive.clear()

        with patch.object(layout, "process_table", side_effect=lambda: list(alive)), \
                patch.object(layout, "signal_member", side_effect=stop), \
                patch.object(layout.time, "monotonic", side_effect=clock.monotonic), \
                patch.object(layout.time, "sleep", side_effect=clock.sleep):
            errors = []
            self.assertEqual(layout.cleanup([entry], self.args.output, [], errors), [])
        self.assertFalse(errors)
        self.assertEqual(signals, [(server, signal.SIGINT)])

    def test_unknown_or_reused_identity_cannot_be_owned(self):
        """已退出 session、复用 PID 及其他 session 不能误杀；Reject unverified or reused identities."""
        entry = self.entries[0]
        entry.update(process=Process(1000, 0), identity=identity(1000), owned={1000: 10, 1001: 11})
        reused = identity(1001, 99, 1000)
        foreign = identity(2001, 11, 2000)
        unobserved = identity(1002, 12, 1000)
        with patch.object(layout, "exact_server", return_value=False):
            owned, unknown = layout.owned_members(entry, [reused, foreign, unobserved])
        self.assertEqual(owned, [])
        self.assertEqual(unknown, [reused, unobserved])
        with patch.object(layout, "exact_server", return_value=True):
            owned, unknown = layout.owned_members(entry, [unobserved])
        self.assertEqual(owned, [unobserved])
        self.assertEqual(unknown, [])

    def test_exact_server_requires_both_unique_executable_and_config(self):
        """仅相同服务名不足以证明归属；A server name alone is insufficient ownership proof."""
        entry, row = self.entries[0], identity(1001, 11, 1000)
        executable = str(entry["directory"] / "ling")
        config = layout.phase_paths(entry)[0] / "ling.toml"
        with patch.object(layout.os, "readlink", return_value=executable), \
                patch.object(Path, "read_bytes", return_value=b"ling\0--config\0" + layout.os.fsencode(config) + b"\0"):
            self.assertTrue(layout.exact_server(entry, row))
        with patch.object(layout.os, "readlink", return_value=executable), \
                patch.object(Path, "read_bytes", return_value=b"ling\0--config\0/other/ling.toml\0"):
            self.assertFalse(layout.exact_server(entry, row))
        with patch.object(layout.os, "readlink", return_value="/other/ling"):
            self.assertFalse(layout.exact_server(entry, row))

    def test_pidfd_signal_rechecks_start_tick(self):
        """打开 pidfd 后身份改变不能发信号；A changed identity after pidfd open prevents signalling."""
        row = identity(1000)
        for current in (identity(1000, 99), None):
            with self.subTest(current=current), \
                    patch.object(layout.os, "pidfd_open", return_value=55, create=True), \
                    patch.object(layout.os, "close") as close, \
                    patch.object(layout, "proc_stat", return_value=current), \
                    patch.object(layout.signal, "pidfd_send_signal", create=True) as send:
                layout.signal_member(row, signal.SIGINT, [])
                send.assert_not_called()
                close.assert_called_once_with(55)

    def test_cleanup_continues_after_one_child_signal_fails(self):
        """一个清理错误不能跳过其他实例；A signal failure cannot skip the other private instances."""
        entries = self.entries[:2]
        servers = [identity(1001, 11, 1000), identity(2001, 11, 2000)]
        for entry, pid in zip(entries, (1000, 2000)):
            entry.update(process=Process(pid, 0), identity=identity(pid), owned={pid + 1: 11})
        calls, clock = [], Clock()

        def send(row, sig, events):
            calls.append(row["pid"])
            if row["pid"] == 1001:
                raise PermissionError("denied")

        with patch.object(layout, "process_table", side_effect=[servers, []]), \
                patch.object(layout, "signal_member", side_effect=send), \
                patch.object(layout.time, "monotonic", side_effect=clock.monotonic), \
                patch.object(layout.time, "sleep", side_effect=clock.sleep):
            errors = []
            layout.cleanup(entries, self.args.output, [], errors)
        self.assertEqual(calls, [1001, 2001])
        self.assertTrue(any("PermissionError" in error for error in errors))

    def test_cleanup_waits_for_driver_then_kills_only_verified_survivors(self):
        """先给驱动完整收尾窗口，再升级信号；Give the driver a grace period before killing verified survivors."""
        entry, clock = self.entries[0], Clock()
        driver, server = identity(1000), identity(1001, 11, 1000)
        entry.update(process=Process(1000), identity=driver, owned={1000: 10, 1001: 11})
        alive, signals = [driver, server], []

        def send(row, sig, events):
            signals.append((row["pid"], sig, clock.now))
            if sig == signal.SIGKILL:
                alive.remove(row)
                if row["pid"] == 1000:
                    entry["process"].code = -9

        with patch.object(layout, "process_table", side_effect=lambda: list(alive)), \
                patch.object(layout, "signal_member", side_effect=send), \
                patch.object(layout.signal, "SIGKILL", 9, create=True), \
                patch.object(layout.time, "monotonic", side_effect=clock.monotonic), \
                patch.object(layout.time, "sleep", side_effect=clock.sleep):
            errors = []
            layout.cleanup([entry], self.args.output, [], errors)
        self.assertFalse(errors)
        self.assertEqual(signals[0][:2], (1000, signal.SIGINT))
        self.assertEqual(len(signals), 3)
        self.assertEqual({(pid, sig) for pid, sig, _ in signals[1:]},
                         {(1000, 9), (1001, 9)})
        self.assertTrue(all(now >= 60 for _, _, now in signals[1:]))

    def test_cleanup_table_failure_still_cleans_each_known_identity(self):
        """全表读取失败后仍逐个检查已知进程；Inspect known identities independently after a table failure."""
        entry, clock = self.entries[0], Clock()
        server = identity(1001, 11, 1000)
        entry.update(process=Process(1000, 0), identity=identity(1000), owned={1001: 11})
        alive, signals = [server], []

        def send(row, sig, events):
            signals.append(row)
            alive.clear()

        with patch.object(layout, "process_table", side_effect=OSError("table unavailable")), \
                patch.object(layout, "proc_stat", side_effect=lambda pid: alive[0] if alive else None), \
                patch.object(layout, "signal_member", side_effect=send), \
                patch.object(layout.time, "monotonic", side_effect=clock.monotonic), \
                patch.object(layout.time, "sleep", side_effect=clock.sleep):
            errors = []
            layout.cleanup([entry], self.args.output, [], errors)
        self.assertEqual(signals, [server])
        self.assertTrue(any("table unavailable" in error for error in errors))

    def test_zombies_are_excluded_and_normal_exit_does_not_wait(self):
        """僵尸已退出，不能消耗完整清理等待；Zombies do not consume the cleanup grace period."""
        stat = "123 (name with ) parentheses) Z " + " ".join(["0"] * 20)
        with patch.object(Path, "read_text", return_value=stat):
            self.assertIsNone(layout.proc_stat(123))
        entry = self.entries[0]
        entry.update(process=Process(1000, 0), identity=identity(1000))
        with patch.object(layout, "process_table", return_value=[]), patch.object(layout.time, "sleep") as sleep:
            self.assertEqual(layout.cleanup([entry], self.args.output, [], []), [])
            sleep.assert_not_called()

    def test_freeze_copies_scripts_and_hashes_exact_bytes(self):
        """冻结副本和摘要必须一致，并保存脏状态；Freeze exact bytes and retain dirty source status."""
        scripts = self.args.source / "benchmarks"
        scripts.mkdir(parents=True)
        for name in ("v030.py", "v020.py"):
            (scripts / name).write_text(f"# frozen {name}\n", encoding="utf-8")
        original = Path.read_text

        def read(path, *args, **kwargs):
            return "boot-test\n" if str(path).replace("\\", "/").endswith("/kernel/random/boot_id") else original(path, *args, **kwargs)

        with patch.object(layout.subprocess, "check_output", side_effect=["head-test\n", " M benchmarks/v030.py\n"]), \
                patch.object(layout.os, "getuid", return_value=0, create=True), \
                patch.object(layout.os, "sched_getaffinity", return_value=set(range(16)), create=True), \
                patch.object(Path, "read_text", new=read):
            hashes = layout.freeze(self.args, self.entries)
        metadata = layout.read_json(self.args.output / "layout.json")
        self.assertEqual(metadata["source_head"], "head-test")
        self.assertIn(" M benchmarks/v030.py", metadata["source_status"])
        self.assertEqual(len(hashes), 4)
        for source, digest in hashes.items():
            copy = self.args.output / "tool_source" / Path(source).name
            self.assertEqual(copy.read_bytes(), Path(source).read_bytes())
            self.assertEqual(layout.sha256(copy), digest)


if __name__ == "__main__":
    unittest.main()
