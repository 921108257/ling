"""固定四实例布局与实际负载覆盖核验；Run and verify the four planned acceptance instances."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import shutil
import signal
import subprocess
import sys
import time


POLICIES = ("noeviction", "lru", "lfu", "random", "ttl-first")
BINARY_SHA256 = "70b209554df2f236c6c4c30d6bbae5ec001a3b2754761038265483f659a0f285"
BINARY_REVISION = "441ffc7dbb2bf48d6257c0ca3b831f099554af1a"


def save(path, data):
    """替换完整 JSON，避免编排结果半写；Replace complete orchestration JSON atomically."""
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(data, indent=2, allow_nan=False) + "\n", encoding="utf-8")
    temporary.replace(path)


def read_json(path):
    """驱动正在更新或尚无结果时重试；Retry absent or partially written driver results."""
    try:
        if path.stat().st_size > 8 * 1024 * 1024:
            raise ValueError(f"Oversized result: {path}")
        return json.loads(path.read_text(encoding="utf-8"))
    except (FileNotFoundError, json.JSONDecodeError):
        return None


def sha256(path):
    """流式固定工具和制品身份；Hash tool and release identities without loading large files."""
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def plan(args):
    """两路 shadow 先行，五策略 pressure 各自顺序运行；Start shadows before pressure drivers."""
    common = [sys.executable, "-u", "-B", str(args.source / "benchmarks/v030.py"),
              "--server-binary", str(args.binary), "--binary-revision", BINARY_REVISION,
              "--stage", "pre-release", "--workers", "2", "--cycles-per-second", str(args.rate),
              "--churn-cycles", "100", "--journal-bytes", "4294967296", "--sample-seconds", "5",
              "--warmup-seconds", "3600", "--slow-consumer-delay", "0.05",
              "--consumer-reconnect-seconds", "120", "--consumer-disconnect-seconds", "0.2",
              "--resource-samples", "100000", "--finding-examples", "200", "--min-free-bytes", "10737418240"]
    layout = [("fast", "shadow", "0,1", "2,3", 3002026),
              ("reliable", "shadow", "8,9", "10,11", 3002028),
              ("fast", "pressure", "4,5", "6,7", 3002027),
              ("reliable", "pressure", "12,13", "14,15", 3002029)]
    entries = []
    for mode, phase, server_cpus, client_cpus, seed in layout:
        seconds = (3300 if args.seconds == 600 else 259500) if phase == "shadow" else (
            600 if args.seconds == 600 else 51840)
        name = mode + "-" + phase
        command = common + [str(args.output / name), "--event-mode", mode, "--phase", phase,
                            "--seconds", str(seconds), "--seed", str(seed),
                            "--server-cpus", server_cpus, "--client-cpus", client_cpus]
        if phase == "pressure":
            command += ["--policy", "all", "--payload-bytes", "32768", "--memory-bytes", "1048576", "--max-keys", "16"]
        entries.append({"name": name, "phase": phase, "seconds": seconds, "command": command,
                        "directory": args.output / name,
                        "policies": ("noeviction",) if phase == "shadow" else POLICIES})
    return entries


def phase_paths(entry):
    """只读取预先声明的阶段，不能由多余结果充数；Use only the explicitly planned phase paths."""
    return [entry["directory"] / f"{entry['name']}-{policy}" for policy in entry["policies"]]


def number(value):
    """时钟不能为 bool、NaN 或无穷；Require a finite numeric clock, excluding booleans."""
    return type(value) in (int, float) and math.isfinite(value) and value >= 0


def phase_problem(row, seconds, phase):
    """阶段必须完整且无已报告失败；Require full duration and no reported phase failures."""
    if not isinstance(row, dict):
        return "missing or malformed phase result"
    start, elapsed = row.get("load_started_monotonic_seconds"), row.get("load_stop_requested_elapsed_seconds")
    if not number(start) or not number(elapsed) or elapsed < seconds or not math.isfinite(start + elapsed):
        return "missing clock or load ended before requested duration"
    if row.get("status") != "duration_reached" or type(row.get("server_exit_code")) is not int or row["server_exit_code"] != 0:
        return "phase or server did not complete successfully"
    if not isinstance(row.get("findings"), dict) or row["findings"]:
        return "phase reported findings or omitted findings"
    criteria = row.get("criteria")
    if not isinstance(criteria, dict) or any(value is False for value in criteria.values()):
        return "phase criteria failed or are missing"
    required = ("no_findings", "full_duration", "all_workers_completed", "consumers_valid", "native_resources_available")
    if phase == "shadow":
        required += ("all_ten_types_checked", "shadow_writes_not_capacity_limited")
    # 关键证据必须是布尔 true，pressure 的 shadow 专项则明确不适用。
    # Critical evidence must be boolean true; shadow-only pressure fields are explicitly inapplicable.
    for name in required:
        if criteria.get(name) is not True:
            return f"required phase criterion is not true: {name}"
    return None


def shadow_ready(entry):
    """两名 worker 均有完成周期才确认负载开始；Require observed progress by both workers."""
    row = read_json(phase_paths(entry)[0] / "progress.json")
    if not isinstance(row, dict) or not number(row.get("load_started_monotonic_seconds")):
        return False
    counts = row.get("counts", {})
    return isinstance(counts, dict) and all(
        type(counts.get(f"worker_{worker}_cycles")) is int and counts[f"worker_{worker}_cycles"] > 0
        for worker in range(2))


def coverage(entries):
    """用实际共同负载区间覆盖十个 pressure 阶段，不能以预算替代证明。
    Check all ten pressure intervals against both shadows' actual common load interval.
    """
    intervals, errors = [], []
    for entry in entries:
        for directory in phase_paths(entry):
            row = read_json(directory / "result.json")
            problem = phase_problem(row, entry["seconds"], entry["phase"])
            if problem:
                errors.append(f"{directory.name}: {problem}")
            else:
                start = row["load_started_monotonic_seconds"]
                intervals.append({"name": directory.name, "phase": entry["phase"], "start": start,
                                  "end": start + row["load_stop_requested_elapsed_seconds"]})
    shadows = [row for row in intervals if row["phase"] == "shadow"]
    pressure = [row for row in intervals if row["phase"] == "pressure"]
    common = None
    if len(shadows) == 2:
        common = {"start": max(row["start"] for row in shadows), "end": min(row["end"] for row in shadows)}
        for row in pressure:
            if not common["start"] <= row["start"] <= row["end"] <= common["end"]:
                errors.append(f"{row['name']}: outside shadows' common actual load interval")
    if len(shadows) != 2 or len(pressure) != 10:
        errors.append("need two complete shadows and all ten complete pressure phases")
    return {"passed": not errors, "common_shadow_interval": common, "intervals": intervals, "errors": errors}


def proc_stat(pid):
    """PID 与启动 tick 联合辨认；Identify a process by PID and Linux start tick, excluding zombies."""
    try:
        raw = (Path("/proc") / str(pid) / "stat").read_text()
        fields = raw[raw.rfind(")") + 2:].split()
        if fields[0] in ("Z", "X"):
            return None
        return {"pid": pid, "pgrp": int(fields[2]), "session": int(fields[3]), "start_ticks": int(fields[19])}
    except (FileNotFoundError, ProcessLookupError):
        return None


def process_table():
    """读取进程身份，不执行全局信号；Inspect identities without sending any global signal."""
    rows = []
    for path in Path("/proc").iterdir():
        if path.name.isdecimal():
            row = proc_stat(int(path.name))
            if row is not None:
                rows.append(row)
    return rows


def exact_server(entry, row):
    """驱动已退出时，仍可用专属 executable 与 config 证明服务归属。
    Prove an orphan server by its unique output executable and explicitly planned config.
    """
    try:
        root = Path("/proc") / str(row["pid"])
        if Path(os.readlink(root / "exe")) != entry["directory"] / "ling":
            return False
        arguments = (root / "cmdline").read_bytes().split(b"\0")
        index = arguments.index(b"--config")
        return arguments[index + 1] in [os.fsencode(directory / "ling.toml") for directory in phase_paths(entry)]
    except (OSError, ValueError, IndexError):
        return False


def owned_members(entry, table):
    """私有 session、原驱动身份和已观察 tick 共同约束清理对象。
    Restrict ownership to the private session and verified, never reused process identities.
    """
    leader = entry.get("identity")
    leader_live = leader is not None and leader in table
    owned, unverified = [], []
    known = entry.setdefault("owned", {})
    for row in table:
        if row["session"] != entry["process"].pid or row["pgrp"] != entry["process"].pid:
            continue
        previous = known.get(row["pid"])
        if previous is not None and previous != row["start_ticks"]:
            unverified.append(row)
        elif (previous == row["start_ticks"] or row == leader
              or leader_live and row["start_ticks"] >= leader["start_ticks"] or exact_server(entry, row)):
            known[row["pid"]] = row["start_ticks"]
            owned.append(row)
        else:
            unverified.append(row)
    return owned, unverified


def signal_member(row, sig, events):
    """pidfd 打开后重新验身份，避免校验到发信号间 PID 被复用。
    Recheck identity after opening pidfd, so PID reuse cannot redirect a cleanup signal.
    """
    descriptor = None
    try:
        descriptor = os.pidfd_open(row["pid"], 0)
        if proc_stat(row["pid"]) != row:
            events.append({"action": "identity_changed; no signal", **row})
            return
        signal.pidfd_send_signal(descriptor, sig, None, 0)
        events.append({"action": "signal", "signal": int(sig), **row})
    except ProcessLookupError:
        pass
    finally:
        if descriptor is not None:
            os.close(descriptor)


def poll_exits(entries, output):
    """每轮检查所有驱动，发现退出立即落盘；Persist any newly observed driver exit immediately."""
    for entry in entries:
        if "process" not in entry:
            continue
        code = entry["process"].poll()
        if code is not None and "exit" not in entry:
            entry["exit"] = {"name": entry["name"], "pid": entry["process"].pid, "exit_code": code,
                             "observed_monotonic_seconds": time.monotonic()}
            save(output / "driver-exits.json", [item["exit"] for item in entries if "exit" in item])
            print(json.dumps(entry["exit"]), flush=True)


def monitor_problem(entries):
    """不等待前面的长跑，后启动阶段失败也立即停止；Detect later drivers' failures without serial waits."""
    for entry in entries:
        if "process" not in entry:
            continue
        checked = entry.setdefault("checked_results", set())
        for directory in phase_paths(entry):
            progress = read_json(directory / "progress.json")
            if isinstance(progress, dict) and isinstance(progress.get("findings"), dict) and progress["findings"]:
                return f"{directory.name}: progress reported findings"
            if directory in checked:
                continue
            row = read_json(directory / "result.json")
            if row is not None or "exit" in entry:
                problem = phase_problem(row, entry["seconds"], entry["phase"])
                if problem:
                    return f"{directory.name}: {problem}"
                checked.add(directory)
        if "exit" in entry and entry["exit"]["exit_code"] != 0:
            return f"{entry['name']}: driver exit {entry['exit']['exit_code']}"
    return None


def cleanup(entries, output, events, errors):
    """先让驱动收尾，退出后的遗留服务独立回收；Let drivers stop, then clean verified orphan servers."""
    deadline, sent = time.monotonic() + 60, set()
    remaining = []
    while True:
        try:
            table = process_table()
        except Exception as error:
            message = f"cleanup process inspection {type(error).__name__}: {error}"
            if message not in errors:
                errors.append(message)
            # 一次进程表读取失败不阻止其余已知身份清理；Still inspect every known identity independently.
            table = []
            for entry in entries:
                for pid in entry.get("owned", {}):
                    try:
                        row = proc_stat(pid)
                        if row is not None:
                            table.append(row)
                    except Exception as error:
                        message = f"cleanup PID {pid} inspection {type(error).__name__}: {error}"
                        if message not in errors:
                            errors.append(message)
        remaining = []
        for entry in entries:
            if "process" not in entry:
                continue
            try:
                members, unknown = owned_members(entry, table)
                remaining.extend({"name": entry["name"], **row} for row in members + unknown)
                if unknown:
                    message = f"{entry['name']}: unverified private-session members; not signalled: {unknown}"
                    if message not in errors:
                        errors.append(message)
                alive = entry["process"].poll() is None
                for row in members:
                    sig = signal.SIGKILL if time.monotonic() >= deadline else signal.SIGINT
                    if sig == signal.SIGINT and alive and row["pid"] != entry["process"].pid:
                        continue
                    key = (row["pid"], row["start_ticks"], int(sig))
                    if key not in sent:
                        signal_member(row, sig, events)
                        sent.add(key)
            except Exception as error:
                message = f"{entry['name']}: cleanup {type(error).__name__}: {error}"
                if message not in errors:
                    errors.append(message)
        try:
            poll_exits(entries, output)
        except Exception as error:
            message = f"cleanup exit recording {type(error).__name__}: {error}"
            if message not in errors:
                errors.append(message)
        active = any("process" in entry and entry["process"].poll() is None for entry in entries)
        if not remaining and not active:
            break
        if time.monotonic() >= deadline + 10:
            errors.append(f"cleanup did not finish; remaining identities: {remaining}")
            break
        time.sleep(.2)
    return remaining


def freeze(args, entries):
    """冻结编排与工具源码、Git 状态及必要环境；Freeze orchestration, tools, source status and environment."""
    destination = args.output / "tool_source"
    destination.mkdir()
    paths = [Path(__file__).resolve(), Path(__file__).with_name("test_run_v030_layout.py").resolve(),
             args.source / "benchmarks/v030.py", args.source / "benchmarks/v020.py"]
    hashes = {}
    for path in paths:
        shutil.copy2(path, destination / path.name)
        hashes[str(path)] = sha256(destination / path.name)
    metadata = {"created_utc": datetime.now(timezone.utc).isoformat(), "script_and_tool_sha256": hashes,
                "source_head": subprocess.check_output(["git", "-C", str(args.source), "rev-parse", "HEAD"], text=True).strip(),
                "source_status": subprocess.check_output(["git", "-C", str(args.source), "status", "--short"], text=True),
                "binary_sha256": BINARY_SHA256, "binary_revision": BINARY_REVISION,
                "platform": platform.platform(), "python": sys.version, "uid": os.getuid(),
                "cpu_count": os.cpu_count(), "cpu_affinity": sorted(os.sched_getaffinity(0)),
                "boot_id": Path("/proc/sys/kernel/random/boot_id").read_text().strip(),
                "commands": [{"name": entry["name"], "command": entry["command"]} for entry in entries],
                "coverage_contract": "two shadows' common actual load interval must contain all ten pressure intervals",
                "scope": "orchestration and coverage only; phase and resource acceptance remain separate"}
    save(args.output / "layout.json", metadata)
    return hashes


def orchestrate(args, entries, hashes):
    """统一轮询并在所有退出路径保存结果；Poll together and retain results on every exit path."""
    errors, events, logs, remaining = [], [], [], []
    beginning, pressure_started, next_observation = time.monotonic(), False, 0
    interrupted = False
    try:
        def launch(entry):
            """每个驱动拥有新 session，先登记 Popen 再验身份；Register the child before inspecting identity."""
            log = (args.output / (entry["name"] + "-driver.log")).open("xb")
            logs.append(log)
            entry["process"] = subprocess.Popen(entry["command"], cwd=args.source, stdout=log, stderr=log,
                                                start_new_session=True)
            entry["identity"] = proc_stat(entry["process"].pid)
            if entry["identity"] is None or any(entry["identity"][field] != entry["process"].pid
                                                for field in ("session", "pgrp")):
                raise RuntimeError(f"{entry['name']}: cannot verify private driver session")
            entry["owned"] = {entry["process"].pid: entry["identity"]["start_ticks"]}

        for entry in entries[:2]:
            launch(entry)
        while True:
            poll_exits(entries, args.output)
            problem = monitor_problem(entries)
            if problem:
                raise RuntimeError(problem)
            now = time.monotonic()
            if now >= next_observation:
                table = process_table()
                for entry in entries:
                    if "process" in entry:
                        _, unknown = owned_members(entry, table)
                        if unknown:
                            raise RuntimeError(f"{entry['name']}: unverified private-session process")
                next_observation = now + 1
            if not pressure_started and all(shadow_ready(entry) for entry in entries[:2]):
                save(args.output / "shadow-ready.json", {"observed_monotonic_seconds": now,
                     "progress": {entry["name"]: read_json(phase_paths(entry)[0] / "progress.json") for entry in entries[:2]}})
                for entry in entries[2:]:
                    launch(entry)
                pressure_started = True
            if pressure_started and all("exit" in entry for entry in entries):
                break
            if not pressure_started and now - beginning > 120:
                raise RuntimeError("two shadows did not both show worker progress within 120 seconds")
            if now - beginning > max(entry["seconds"] for entry in entries) + 300:
                raise RuntimeError("layout exceeded its duration plus 300-second shutdown budget")
            time.sleep(.2)
    except BaseException as error:
        interrupted = isinstance(error, KeyboardInterrupt)
        errors.append(f"{type(error).__name__}: {error}")
    finally:
        try:
            remaining = cleanup(entries, args.output, events, errors)
        except BaseException as error:
            errors.append(f"cleanup {type(error).__name__}: {error}")
        # 清理个别异常不能省略其他驱动退出信息；Always retain every launched driver's final exit field.
        for entry in entries:
            if "process" in entry:
                try:
                    code = entry["process"].poll()
                except Exception as error:
                    code = None
                    errors.append(f"{entry['name']}: final poll {type(error).__name__}: {error}")
                entry.setdefault("exit", {"name": entry["name"], "pid": entry["process"].pid,
                                         "observed_monotonic_seconds": time.monotonic()})["exit_code"] = code
                if code is None or code != 0:
                    errors.append(f"{entry['name']}: final driver exit {code}")
        exits = [entry["exit"] for entry in entries if "exit" in entry]
        try:
            save(args.output / "driver-exits.json", exits)
        except OSError as error:
            # 单个证据写入失败仍需关闭日志并尝试总结果；Continue log cleanup and final evidence after one write failure.
            errors.append(f"final exit recording {type(error).__name__}: {error}")
        for log in logs:
            try:
                log.close()
            except OSError as error:
                errors.append(f"driver log close: {error}")
    for path, digest in hashes.items():
        try:
            if not Path(path).is_file() or sha256(Path(path)) != digest:
                errors.append(f"frozen source changed during run: {path}")
        except OSError as error:
            errors.append(f"frozen source verification {path}: {error}")
    try:
        actual = coverage(entries)
    except Exception as error:
        actual = {"passed": False, "errors": [f"coverage {type(error).__name__}: {error}"]}
    result = {"orchestration_passed": not errors and actual["passed"], "errors": errors,
              "coverage": actual, "driver_exits": exits, "cleanup_events": events, "remaining_processes": remaining,
              "owned_process_identities": [{"name": entry["name"], "driver": entry.get("identity"),
                  "pid_start_ticks": entry.get("owned", {})} for entry in entries if "process" in entry],
              "scope": "orchestration and actual coverage; does not replace consumer/resource acceptance"}
    save(args.output / "layout-result.json", result)
    return 130 if interrupted else int(not result["orchestration_passed"])


def main():
    """仅允许专用 Linux 环境和固定公开制品；Require the dedicated Linux environment and pinned release."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--seconds", type=int, required=True)
    parser.add_argument("--rate", type=float, default=5)
    args = parser.parse_args()
    base = Path("/root/ling-acceptance")
    if sys.platform != "linux" or not (base / ".wsl-crash-acceptance").is_file() or (
            base / ".wsl-crash-acceptance").read_text() != "ling-acceptance\n":
        parser.error("Dedicated Linux acceptance environment required")
    for path in (args.output, args.source, args.binary):
        if path.resolve() != path or path == base or not path.is_relative_to(base):
            parser.error("Paths must be canonical children of the dedicated acceptance root")
    if args.seconds not in (600, 259200) or not math.isfinite(args.rate) or not .01 <= args.rate <= 5:
        parser.error("Use planned calibration or 72-hour duration and a rate no greater than five")
    if not hasattr(os, "pidfd_open") or not hasattr(signal, "pidfd_send_signal"):
        parser.error("Verified cleanup requires Linux pidfd support")
    descriptor = os.pidfd_open(os.getpid(), 0)
    os.close(descriptor)
    if not set(range(16)) <= set(os.sched_getaffinity(0)):
        parser.error("Planned layout requires CPU affinity 0 through 15")
    if sha256(args.binary) != BINARY_SHA256:
        parser.error("Public release binary hash differs")
    args.output.mkdir(exist_ok=False)
    entries = plan(args)
    hashes = freeze(args, entries)
    return orchestrate(args, entries, hashes)


if __name__ == "__main__":
    raise SystemExit(main())
