#!/usr/bin/env python3
"""从已停止、已归档校准直接推导正式计划，不启动服务或修改原件。
Derive a formal plan from stopped, archived calibration without starting services or editing evidence.
"""
import argparse
from collections import Counter
from fractions import Fraction
import importlib.util
import json
import math
from pathlib import Path, PurePosixPath
import sys
import tempfile
import tomllib

sys.dont_write_bytecode = True
REPO = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("v030_plan_checks", Path(__file__).with_name("evaluate-v030-acceptance.py"))
CHECKS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHECKS)
GROUPS, POLICIES = CHECKS.GROUPS, CHECKS.POLICIES
FACTOR, MARGIN = Fraction(4, 5), Fraction(5, 4)
EXPECTED_BINARY = "70b209554df2f236c6c4c30d6bbae5ec001a3b2754761038265483f659a0f285"
EXPECTED_REVISION = "441ffc7dbb2bf48d6257c0ca3b831f099554af1a"


def fraction(value):
    """有限数字只经十进制文本转成有理数；Convert finite numbers through decimal text only."""
    return Fraction(str(CHECKS.number(value)))


def ceil_milliseconds(value):
    """毫秒向上取整不降低门槛余量；Round a positive limit upward to whole milliseconds."""
    return math.ceil(value * 1000) / 1000


def scaled_total(delta, elapsed, duration):
    """精确计算总量下限，超64位计数也不经浮点；Scale exact totals without floating-point counts."""
    CHECKS.require(CHECKS.integer(delta) > 0 and fraction(elapsed) > 0, "positive calibration delta and elapsed required")
    return math.ceil(FACTOR * CHECKS.integer(delta) / fraction(elapsed) * fraction(duration))


def scaled_window(delta):
    """分钟窗只缩放计数下界，不按局部跨度放大；Scale a whole-window lower bound without local-span normalization."""
    CHECKS.require(CHECKS.integer(delta) > 0, "positive full-window baseline required")
    return math.ceil(FACTOR * CHECKS.integer(delta))


def offline_budget(reconnects, seconds, elapsed, duration, maximum_interval):
    """保留4次/600秒的相位边界余量；Retain phase-edge slack for four reconnects per 600 seconds."""
    count, offline = CHECKS.integer(reconnects), fraction(seconds)
    CHECKS.require(count > 0 and offline > 0 and maximum_interval > 0, "slow consumer needs actual offline evidence")
    ratio = fraction(duration) / fraction(elapsed)
    return {"min_reconnects": math.floor(FACTOR * count * ratio),
            "max_reconnects": math.ceil(MARGIN * count * ratio) + 1,
            "min_seconds": float(FACTOR * offline * ratio),
            "max_seconds": float(MARGIN * offline * ratio + fraction(maximum_interval))}


def source_path(value):
    """绑定文件必须在仓库内，计划可以随归档复核；Bound sources remain inside the repository for later verification."""
    path = Path(value).resolve()
    CHECKS.require(path.is_relative_to(REPO), "source outside repository: " + str(path))
    return path.relative_to(REPO).as_posix()


def archive_sources(archive, root):
    """核对归档外层摘要及每个被使用原件，不展开归档；Check archive digests and consumed originals without extracting bundles."""
    manifest = CHECKS.read_json(archive / "manifest.json")
    CHECKS.require(manifest["run_id"] == root.name and manifest["git_path_filters_preserve_bytes"] is True,
                   "archive identity/filter verification differs")
    sources = {source_path(archive / "manifest.json"): CHECKS.sha256(archive / "manifest.json")}
    seen, used = set(), {}
    for row in manifest["files"]:
        path = CHECKS.relative(archive, row["path"])
        CHECKS.require(row["path"] not in seen and path.stat().st_size == row["bytes"] and CHECKS.sha256(path) == row["sha256"],
                       "archive member size/hash differs")
        seen.add(row["path"])
        sources[source_path(path)] = row["sha256"]
    CHECKS.require(seen >= {"evidence-files.manifest.json", "summary.json", "archive-verification.json",
                           "analysis.json", "extra.json", "v030_analyze.py", "analyze-v030-calibration-extra.py"},
                   "required archive sources absent from outer manifest")
    inventory = CHECKS.read_json(archive / "evidence-files.manifest.json")
    CHECKS.require(Path(inventory["source_root"]).resolve() == root, "archive original root differs")
    rows = inventory["files"] + inventory["exact_binary_aliases"]
    original = {row["path"]: row for row in rows}
    CHECKS.require(len(original) == len(rows), "duplicate original inventory path")
    for row in rows:
        if row.get("storage") == "direct":
            CHECKS.require(row["path"] in seen, "direct original absent from outer manifest")
            continue
        stored = row
        if row.get("storage") == "exact_binary_alias":
            stored = original[row["stored_as"]]
            CHECKS.require(stored.get("storage") == "archive" and stored["sha256"] == row["sha256"]
                           and stored["bytes"] == row["bytes"] and stored["archive"] == row["archive"],
                           "invalid original binary archive alias")
        CHECKS.require(stored.get("storage") == "archive" and stored["archive"] in seen,
                       "original has no inventoried distributable archive")

    def bind(path):
        """校准计数只来自清单已绑定的原始文件；Read calibration counts only from inventoried original files."""
        relative = path.relative_to(root).as_posix()
        row = original["raw/" + relative]
        CHECKS.require(path.stat().st_size == row["bytes"] and CHECKS.sha256(path) == row["sha256"],
                       "calibration original differs: " + relative)
        used[path] = (row["bytes"], row["sha256"])
        return CHECKS.read_json(path) if path.suffix == ".json" else None

    def recheck():
        """结束时重新核对已读原件；Recheck every consumed original after derivation."""
        for path, (size, digest) in used.items():
            CHECKS.require(path.stat().st_size == size and CHECKS.sha256(path) == digest,
                           "calibration original changed during derivation: " + str(path))

    return sources, bind, recheck


def offline_intervals(base, directory, result, summary):
    """重读离线开闭原件并取得最大真实单次时长；Re-read paired offline originals and retain each group's longest real interval."""
    diagnostics, opened, maximum, totals, reconnects = base.Diagnostics(), {}, dict.fromkeys(GROUPS, 0.0), Counter(), Counter()
    reader = base.JsonLines(directory / "errors.jsonl.gz", diagnostics)
    for row in reader.rows():
        category = row.get("category")
        if category not in ("planned_consumer_disconnect_started", "planned_consumer_disconnect_completed"):
            continue
        group = row["consumer"]
        CHECKS.require(group in GROUPS, "unknown offline consumer")
        if category.endswith("started"):
            CHECKS.require(group not in opened, "overlapping offline interval")
            opened[group] = CHECKS.number(row["monotonic_seconds"])
        else:
            start, stop = CHECKS.number(row["started_monotonic_seconds"]), CHECKS.number(row["completed_monotonic_seconds"])
            CHECKS.require(group in opened and opened.pop(group) == start and stop >= start,
                           "unmatched offline interval")
            actual = stop - start
            CHECKS.require(math.isclose(actual, CHECKS.number(row["duration_seconds"]), rel_tol=0, abs_tol=1e-6)
                           and type(row["reconnected"]) is bool, "invalid offline completion")
            maximum[group] = max(maximum[group], actual)
            totals[group] += actual
            reconnects[group] += int(row["reconnected"])
    CHECKS.require(reader.stats["complete"] is True and not diagnostics.counts and not opened,
                   "offline evidence incomplete")
    CHECKS.require(summary["offline"]["timeline_complete"] is True, "analyzed offline evidence incomplete")
    for group in GROUPS:
        final = result["consumers"][group]
        CHECKS.require(math.isclose(final["planned_disconnected_seconds"], totals[group], rel_tol=0, abs_tol=1e-6)
                       and final["reconnects"] == reconnects[group] and final["planned_offline"] is False,
                       "offline records/final totals differ")
    return maximum


def capacity_projection(resources, duration):
    """首日/单段是启动历史情景，72小时回收仍待实测；Startup scenarios do not prove retention reclamation over 72 hours."""
    scenario = resources["capacity_scenarios"]
    CHECKS.require(all(value is True for value in scenario["prerequisites_for_emitting_scenarios"].values()),
                   "capacity projection prerequisites failed")
    state = resources["counter_observations"]["journal_transaction_bytes.sum"]
    CHECKS.require(not state["rate_unavailable_reasons"], "append counter unavailable")
    rates = {"whole_sample_start_normalized": Fraction(CHECKS.integer(state["delta"])) / fraction(state["sample_start_span_seconds"])}
    adjacent = state["positive_max_adjacent_start_normalized"]
    CHECKS.require(adjacent is not None, "no observed positive append interval")
    rates["positive_max_adjacent_start_normalized"] = Fraction(CHECKS.integer(adjacent["delta"])) / fraction(adjacent["start_span_seconds"])
    fixed = resources["fixed_60s_summary"]["maximum_lower_bounds"]["journal_transaction_bytes.sum"]
    rates["maximum_fixed_60s_lower_bound"] = Fraction(CHECKS.integer(fixed["delta_lower_bound"]), 60)
    rate = max(rates.values())
    initial, peak = CHECKS.integer(scenario["initial_active_bytes"]), CHECKS.integer(scenario["observed_active_peak_bytes"])
    # 4MiB buffer给2MiB载荷上限；16字节记录框架及289字节拓扑检查点单列余量。
    # A 4MiB buffer limits payload to 2MiB; reserve framing and the eight-stream checkpoint explicitly.
    record, checkpoint, threshold = 2 * 2**20 + 16, 28 * 2 * 4 + 65, 3 * 2**29
    projections = {}
    for seconds in (86400, 51840, 259500):
        projected = max(peak, initial + math.ceil(MARGIN * rate * seconds))
        projections[str(seconds)] = {"active_bytes_historical_scenario": str(projected),
            "with_record_checkpoint_reserve_bytes": str(projected + record + checkpoint),
            "remaining_to_event_admission_bytes": str(threshold - projected - record - checkpoint)}
    startup = 86400 if duration == 259500 else duration
    ready = CHECKS.integer(projections[str(startup)]["with_record_checkpoint_reserve_bytes"]) < threshold
    return {"startup_scenario_seconds": startup, "startup_scenario_below_event_admission": ready,
        "append_rates_exact": {name: str(value) for name, value in rates.items()}, "selected_rate_exact": str(rate),
        "initial_active_bytes": str(initial), "observed_active_peak_bytes": str(peak),
        "event_total_active_admission_bytes": str(threshold), "maximum_record_reserve_bytes": str(record),
        "topology_checkpoint_reserve_bytes": str(checkpoint), "projections": projections,
        "scope": "1.25 times observed-rate scenarios, not statistical or future upper bounds; 259500-second no-reclamation projection is not an automatic rejection",
        "retention_validation": "After 24 hours verify real compaction/reclamation evidence, retained events and clean Journal counters; 72-hour capacity remains a test hypothesis."}


def generate(root, archive, source_root):
    """只接受完整原件，所有正式下限从原件推导；Derive every formal lower bound from complete original evidence."""
    source = PurePosixPath(source_root)
    CHECKS.require(source.is_absolute() and str(source) == source_root and ".." not in source.parts,
                   "normalized absolute formal WSL source root required")
    CHECKS.require(source.name != root.name, "formal run must have a new run identity")
    sources, bind, recheck = archive_sources(archive, root)
    layout, completed = bind(root / "layout.json"), bind(root / "layout-result.json")
    summary = CHECKS.read_json(archive / "summary.json")
    CHECKS.require(summary["controller_exit"] == 0 and summary["metadata_diagnostics"] == []
                   and summary["nested_gzip_failure_count"] == 0, "calibration archive reports a failure")
    verification = CHECKS.read_json(archive / "archive-verification.json")
    CHECKS.require(all(verification[key] is True for key in ("all_originals_rehashed", "source_file_sets_unchanged",
                    "direct_copies_sha256_size_match")) and verification["nested_gzip_failure_count"] == 0,
                   "archive verification incomplete")
    CHECKS.require(completed["orchestration_passed"] is True and completed["errors"] == []
                   and completed["cleanup_events"] == [] and completed["remaining_processes"] == [], "calibration orchestration failed")
    CHECKS.require(layout["binary_sha256"] == EXPECTED_BINARY and layout["binary_revision"] == EXPECTED_REVISION,
                   "calibration public binary differs")
    analysis_path, extra_path = archive / "analysis.json", archive / "extra.json"
    analysis, extra = CHECKS.read_json(analysis_path), CHECKS.read_json(extra_path)
    CHECKS.require(analysis["format"] == "ling-v030-observations-1" and analysis["window_seconds"] == 60,
                   "wrong analysis format")
    CHECKS.require(extra["format"] == "ling-v030-calibration-supplement-1" and Path(extra["root"]).resolve() == root,
                   "wrong supplementary root or format")
    CHECKS.require(extra["source_sha256"]["analysis_report"] == CHECKS.sha256(analysis_path)
                   and extra["source_sha256"]["analyzer"] == CHECKS.sha256(CHECKS.ANALYZER)
                   == CHECKS.sha256(archive / "v030_analyze.py")
                   and extra["source_sha256"]["helper"] == CHECKS.sha256(archive / "analyze-v030-calibration-extra.py"),
                   "supplementary tool/report binding differs")
    CHECKS.require(extra["diagnostics"]["counts"] == {} and extra["integrity"]["all_phase_gzip_eof_crc_complete"] is True
                   and extra["integrity"]["phases_with_supplementary_diagnostics"] == 0, "supplementary evidence failed")
    order, analyzed, supplements = [], {}, {}
    for row in analysis["phases"]:
        directory = Path(row["directory"]).resolve()
        CHECKS.require(directory.is_relative_to(root), "analysis path outside actual calibration root")
        name, instance = directory.name, directory.parent.name
        CHECKS.require(name not in analyzed and instance in CHECKS.INSTANCES, "duplicate/unexpected analyzed phase")
        analyzed[name] = row
        if instance not in order:
            order.append(instance)
    for row in extra["phases"]:
        CHECKS.require(row["name"] not in supplements, "duplicate supplementary phase")
        supplements[row["name"]] = row
    expected = {f"{mode}-{phase}-{policy}" for mode in ("fast", "reliable") for phase in ("shadow", "pressure")
                for policy in (("noeviction",) if phase == "shadow" else POLICIES)}
    CHECKS.require(set(analyzed) == set(supplements) == expected and set(order) == CHECKS.INSTANCES,
                   "twelve exact calibration phases required")
    for row in analyzed.values():
        directory = Path(row["directory"]).resolve()
        for filename in ("result.json", "ling.toml", "resources.jsonl.gz", "errors.jsonl.gz",
                         "consumer-events.jsonl.gz", "server-complete.log.gz"):
            bind(directory / filename)
    base = CHECKS.analyzer_module()
    # 上游报告无输入摘要，所以按同路径、同顺序复算字节；不信任手填minimum。
    # The upstream report lacks input hashes: recompute bytes at the same paths/order instead of trusting hand-entered minima.
    with tempfile.TemporaryDirectory(prefix="ling-v030-plan-") as temporary:
        output = Path(temporary) / "analysis.json"
        CHECKS.require(base.main([str(root / name) for name in order] + ["--output", str(output)]) == 0
                       and CHECKS.sha256(output) == CHECKS.sha256(analysis_path), "analysis differs from current raw evidence")
    recorded = {}
    for path, digest in layout["script_and_tool_sha256"].items():
        name = PurePosixPath(path).name
        CHECKS.require(name not in recorded, "ambiguous layout tool basename")
        recorded[name] = digest
        bind(root / "tool_source" / name)
        CHECKS.require(CHECKS.sha256(root / "tool_source" / name) == digest, "layout frozen tool differs")
    for name in ("run-v030-layout.py", "v030.py", "v020.py"):
        CHECKS.require(CHECKS.sha256(REPO / "benchmarks" / name) == recorded[name], "current tool differs from calibrated tool: " + name)
    plan = {"format": "ling-v030-acceptance-plan-1", "source_root": str(source), "run_id": source.name,
        "binary": {"sha256": EXPECTED_BINARY, "revision": EXPECTED_REVISION},
        "layout_sha256": recorded["run-v030-layout.py"], "analyzer_sha256": CHECKS.sha256(CHECKS.ANALYZER),
        "analysis_directories": order, "analysis_workers": None, "instances": {},
        "limits": {"window_seconds": 60, "warmup_seconds": 3600, "rss_bytes_per_hour": 1048576,
                   "handles_per_hour": .1, "threads_per_hour": .1},
        "calibration": {"factor": .8, "source_sha256": sources, "phases": {}}, "phases": [],
        "derivation": {"total": "ceil((4/5) * (load_stop-load_start) / actual_calibration_load_seconds * formal_seconds), using exact Fraction decimal time",
            "window": "ceil((4/5) * minimum complete 60-second contained-bracket counter lower bound); no local-span rate conversion",
            "active": "0.8 * minimum full-window active_seconds delta; floating seconds retained, not rounded upward",
            "sample_limits": "ceil_to_milliseconds(1.25 * max global observed value across all 12 resource streams, terminal included)",
            "slow_offline": "min_reconnects=floor(.8*cal_load_reconnects/Tcal*T); max_reconnects=ceil(1.25*cal_load_reconnects/Tcal*T)+1; min_seconds=.8*cal_load_offline/Tcal*T; max_seconds=1.25*cal_load_offline/Tcal*T+max_observed_single_offline",
            "scope": "0.8 and 1.25 are frozen engineering margins, not statistical bounds. Active liveness and unique ACK progress are checked independently.",
            "phases": {}}, "readiness": {"ready_to_freeze": True, "blocking": []}}
    gap = sample_duration = Fraction(0)
    for instance in order:
        environment = bind(root / instance / "environment.json")
        settings = dict(environment["settings"])
        duration = 259500 if settings["phase"] == "shadow" else 51840
        CHECKS.require(settings["workers"] == 2 and settings["sample_seconds"] == 5 and settings["warmup_seconds"] == 3600
                       and settings["seconds"] == (3300 if settings["phase"] == "shadow" else 600), "calibration timing/worker settings differ")
        CHECKS.require(environment["script_sha256"] == recorded["v030.py"]
                       and environment["tool_sources_sha256"]["benchmarks/v020.py"] == recorded["v020.py"], "instance tool dependency differs")
        for filename, digest in (("v030.py", recorded["v030.py"]), ("v020.py", recorded["v020.py"])):
            bind(root / instance / "tool_source" / filename)
            CHECKS.require(CHECKS.sha256(root / instance / "tool_source" / filename) == digest, "instance frozen dependency bytes differ")
        bind(root / instance / "ling")
        CHECKS.require(environment["binary"]["sha256"] == EXPECTED_BINARY == CHECKS.sha256(root / instance / "ling")
                       and environment["binary"]["revision_asserted_by_operator"] == EXPECTED_REVISION, "instance binary differs")
        settings.update(seconds=duration, output=str(source / instance))
        plan["instances"][instance] = {"settings": settings, "script_sha256": recorded["v030.py"], "v020_sha256": recorded["v020.py"]}
        policies = ("noeviction",) if settings["phase"] == "shadow" else POLICIES
        for policy in policies:
            name, relative = instance + "-" + policy, instance + "/" + instance + "-" + policy
            directory = root / relative
            result = bind(directory / "result.json")
            config = tomllib.loads((directory / "ling.toml").read_text(encoding="utf-8"))
            CHECKS.require(result["config_sha256"] == CHECKS.sha256(directory / "ling.toml")
                           and config["database"]["databases"] == 2 and config["engine"]["shard_count"] == 4
                           and config["events"]["buffer_bytes"] == 4194304 and config["events"]["retention_days"] == 1
                           and config["events"]["mode"] == settings["event_mode"], "calibration config differs from capacity/topology assumptions")
            if settings["event_mode"] == "reliable":
                CHECKS.require(config["event_journal"]["max_bytes"] == 4294967296, "calibration Journal configuration differs")
            observed, supplement = analyzed[name], supplements[name]
            CHECKS.require(Path(observed["directory"]).resolve() == directory and supplement["settings"] == environment["settings"]
                           and supplement["relative_directory"].replace("\\", "/") == relative, "phase source binding differs")
            CHECKS.require(result["status"] == "duration_reached" and result["requested_seconds"] == environment["settings"]["seconds"], "calibration phase incomplete")
            CHECKS.require(result["event_mode"] == settings["event_mode"] and result["phase"] == settings["phase"]
                           and result["policy"] == policy and result["seed"] == settings["seed"], "calibration phase identity differs")
            elapsed = CHECKS.number(result["load_stop_requested_elapsed_seconds"])
            totals, window_metrics = CHECKS.metric_sets(2)
            first, last = (base.snapshot_metrics(result[key], 2, key) for key in ("load_start", "load_stop"))
            deltas = {key: CHECKS.integer(last[key]) - CHECKS.integer(first[key]) for key in totals}
            minima = observed["summary"]["observed_minima"]
            full = CHECKS.integer(observed["summary"]["full_windows"])
            CHECKS.require(full > 0, "calibration has no full minute")
            for key in window_metrics + ["active_seconds." + group for group in GROUPS]:
                CHECKS.require(minima[key]["usable_full_windows"] == full and minima[key]["unavailable_full_windows"] == 0,
                               "calibration contains unavailable full-window metric: " + key)
            minimum_window = {key: CHECKS.integer(minima[key]["value"]) for key in window_metrics}
            active = {group: CHECKS.number(minima["active_seconds." + group]["value"]) for group in GROUPS}
            CHECKS.require(all(value > 0 for value in active.values()), "zero active calibration baseline")
            maximum_offline = offline_intervals(base, directory, result, observed["summary"])
            budgets, observed_offline = {}, {}
            for group in GROUPS:
                before, after = result["load_start"]["consumers"][group], result["load_stop"]["consumers"][group]
                count = CHECKS.integer(after["reconnects"]) - CHECKS.integer(before["reconnects"])
                seconds = CHECKS.number(after["planned_disconnected_seconds"]) - CHECKS.number(before["planned_disconnected_seconds"])
                observed_offline[group] = {"load_reconnects": count, "load_offline_seconds": seconds,
                                           "maximum_complete_interval_seconds": maximum_offline[group]}
                budgets[group] = offline_budget(count, seconds, elapsed, duration, maximum_offline[group]) if group.startswith("slow-") else {
                    "min_reconnects": 0, "max_reconnects": 0, "min_seconds": 0, "max_seconds": 0}
                if group.startswith("fast-"):
                    CHECKS.require(count == seconds == maximum_offline[group] == 0, "fast consumer went offline")
            baseline = {"actual_load_seconds": elapsed, "total_deltas": {key: str(value) for key, value in deltas.items()},
                        "minimum_window": {key: str(value) for key, value in minimum_window.items()}, "minimum_window_active_seconds": active}
            phase = {key: result[key] for key in ("event_mode", "phase", "policy", "seed")}
            phase.update(requested_seconds=duration, directory=relative,
                minimum_totals={key: scaled_total(value, elapsed, duration) for key, value in deltas.items()},
                minimum_window={key: scaled_window(value) for key, value in minimum_window.items()},
                minimum_window_active_seconds={group: value * .8 for group, value in active.items()}, offline=budgets,
                capacity={"minimum_oom": int(settings["phase"] == "pressure" and policy == "noeviction"),
                          "minimum_evicted": int(settings["phase"] == "pressure" and policy != "noeviction")})
            # ttl-first只选择校准实际出现的一种触限证据，不要求两项都出现。
            # Freeze one capacity proof actually seen for ttl-first; both kinds are not required.
            before, after = (result[key]["workload"]["server_errors"] for key in ("load_start", "load_stop"))
            oom = CHECKS.integer(after.get("ERR_OUT_OF_MEMORY", 0)) - CHECKS.integer(before.get("ERR_OUT_OF_MEMORY", 0))
            evicted = sum(CHECKS.integer(row["metrics"]["by_kind.Evicted"]["delta"]) for row in observed["windows"]
                          if row["metrics"]["by_kind.Evicted"]["delta"] is not None)
            if policy == "ttl-first" and evicted == 0:
                phase["capacity"] = {"minimum_oom": 1, "minimum_evicted": 0}
            CHECKS.require(oom >= phase["capacity"]["minimum_oom"] and evicted >= phase["capacity"]["minimum_evicted"],
                           "calibration lacks required real capacity proof")
            CHECKS.validate_errors_and_capacity(phase, result, observed)
            CHECKS.validate_error_records(base, directory / "errors.jsonl.gz", phase, result)
            # 校准本身只要求全程存活及真实掉线账目，不套用外推后的正式总量。
            # Validate calibration liveness with its exact observed budgets, not extrapolated formal totals.
            actual_phase = dict(phase, offline={group: {"min_reconnects": item["load_reconnects"], "max_reconnects": item["load_reconnects"],
                "min_seconds": item["load_offline_seconds"], "max_seconds": item["load_offline_seconds"]} for group, item in observed_offline.items()})
            CHECKS.validate_consumers(actual_phase, result)
            resources = supplement["resources"]
            CHECKS.require(resources["source"]["complete"] is True and resources["sample_time_problems"] == {}, "resource timeline incomplete")
            rows = CHECKS.integer(observed["summary"]["resource_rows"])
            CHECKS.require(rows == result["resource_samples_saved"] == result["native_resources"]["attempts"]
                           and CHECKS.integer(observed["summary"]["resource_samples_omitted"]) == 0, "resource samples missing")
            for field in ("cpu_seconds", "rss_bytes", "handles", "threads"):
                availability = observed["summary"]["native_availability"][field]
                CHECKS.require(availability["valid"] == rows and availability["missing"] == 0, "native field unavailable")
            gap = max(gap, fraction(resources["maximum_sample_start_gap_seconds"]))
            sample_duration = max(sample_duration, fraction(resources["maximum_sample_duration_seconds"]))
            derivation = {"offline_observed": observed_offline, "load_oom": str(oom), "load_evicted_lower_bound": str(evicted)}
            if settings["event_mode"] == "reliable":
                derivation["journal_capacity"] = capacity_projection(resources, duration)
                if not derivation["journal_capacity"]["startup_scenario_below_event_admission"]:
                    plan["readiness"]["blocking"].append(name + ": first-day/per-phase historical capacity scenario plus record reserve exceeds ordinary admission")
            plan["calibration"]["phases"][name] = baseline
            plan["phases"].append(phase)
            plan["derivation"]["phases"][name] = derivation
    CHECKS.require(gap > 0 and sample_duration > 0, "positive observed sample limits required")
    plan["limits"].update(max_sample_start_gap_seconds=ceil_milliseconds(MARGIN * gap),
                          max_sample_duration_seconds=ceil_milliseconds(MARGIN * sample_duration))
    plan["derivation"]["sample_observed_maxima"] = {"start_gap_seconds": float(gap), "duration_seconds": float(sample_duration)}
    for path in (Path(__file__).resolve(), Path(CHECKS.__file__).resolve()):
        sources[source_path(path)] = CHECKS.sha256(path)
    plan["readiness"]["ready_to_freeze"] = not plan["readiness"]["blocking"]
    recheck()
    CHECKS.validate_plan(plan, root.with_name(source.name))
    if not plan["readiness"]["ready_to_freeze"]:
        plan["format"] = "ling-v030-acceptance-plan-draft-1"
    return plan


def main(argv=None):
    """输出必须全新且位于原件之外；Create a new output outside original evidence."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--calibration-root", type=Path, required=True)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--source-root", required=True, help="new formal WSL output root, not the calibration root")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args(argv)
    root, archive, output = args.calibration_root.resolve(), args.archive.resolve(), args.output.resolve()
    CHECKS.require(not output.is_relative_to(root) and not output.is_relative_to(archive), "output must be outside calibration originals/archive")
    plan = generate(root, archive, args.source_root)
    with output.open("x", encoding="utf-8", newline="\n") as stream:
        json.dump(plan, stream, ensure_ascii=False, allow_nan=False, indent=2)
        stream.write("\n")
    print(json.dumps({"output": str(output), "ready_to_freeze": plan["readiness"]["ready_to_freeze"],
                      "blocking": plan["readiness"]["blocking"], "phases": len(plan["phases"])}))
    return 0 if plan["readiness"]["ready_to_freeze"] else 2


if __name__ == "__main__":
    raise SystemExit(main())
