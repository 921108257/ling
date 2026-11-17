#!/usr/bin/env python3
"""专用 WSL 强制终止验收；宿主证据独立于被终止的发行版。
Dedicated WSL termination acceptance with evidence outside the terminated distro.

This measures VM/process crash recovery, never physical power-loss durability.
No WSL operation runs on import; --execute is required. Only ling-acceptance is allowed.
"""
import argparse
from collections import Counter
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path, PurePosixPath, PureWindowsPath
import platform
import random
import re
import select
import socket
import struct
import subprocess
import sys
import threading
import time
import uuid
import zlib

from v020 import Client, WireError, receive

sys.dont_write_bytecode = True
DISTRO = "ling-acceptance"
BASE = "/root/ling-acceptance"
MARKER = BASE + "/.wsl-crash-acceptance"
MAX_FILE = 256 * 1024 * 1024
COMMIT = b"LGCMT001"
# ACK 等待期间仍可能收到整个交付窗口；低于共享 Client 的 256 项缓存硬限。
# A whole delivery window can precede an ACK reply; stay below Client's 256-event hard cap.
MAX_PENDING_EVENTS = 128


def linux_path(value):
    """只允许专用根下的规范绝对路径；Allow canonical absolute paths below the dedicated root."""
    if (not isinstance(value, str) or not value.startswith(BASE + "/")
            or not re.fullmatch(r"[A-Za-z0-9_./-]+", value)
            or any(part in ("", ".", "..") for part in value.split("/")[1:])
            or str(PurePosixPath(value)) != value):
        raise ValueError("Linux path must be a canonical child of " + BASE)
    return value


def distro_name(value):
    """拒绝默认发行版与全局关闭入口；Reject default distro and global shutdown selectors."""
    if value != DISTRO:
        raise ValueError("Only the dedicated ling-acceptance distro is allowed")
    return value


def windows_output(value):
    """证据必须位于 Windows 本地绝对路径；Evidence needs a local absolute Windows path."""
    path = PureWindowsPath(value)
    if (not path.is_absolute() or not re.fullmatch(r"[A-Za-z]:", path.drive)
            or ".." in path.parts):
        raise ValueError("Output must be a local Windows drive path, never a WSL/UNC path")
    return value


def serializable(value):
    """字节以十六进制保留，整数不转浮点；Preserve bytes as hex and integers without float conversion."""
    if isinstance(value, bytes):
        return {"hex": value.hex()}
    if isinstance(value, dict):
        return {str(key): serializable(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [serializable(item) for item in value]
    return value


def write_json(path, value):
    """仅写宿主结果，不同步 Linux 日志；Write host results without syncing the Linux journal."""
    with path.open("x", encoding="utf-8", newline="\n") as stream:
        json.dump(serializable(value), stream, ensure_ascii=False, indent=2)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())


def snapshot_tools(directory):
    """保存实际执行的控制器和协议辅助源码；Preserve the exact controller and imported protocol helper."""
    directory.mkdir()
    sources = [Path(__file__).resolve(), Path(sys.modules[Client.__module__].__file__).resolve(),
               Path(__file__).with_name("test_wsl_crash.py").resolve()]
    result = []
    for source in sources:
        data = source.read_bytes()
        with (directory / source.name).open("xb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        result.append({"source": str(source), "copy": source.name,
                       "sha256": hashlib.sha256(data).hexdigest()})
    return result


class Evidence:
    """每条发送意图、响应与 ACK 都在 Windows fsync；Fsync every intent, response and ACK on Windows."""
    def __init__(self, path):
        self.stream = path.open("x", encoding="utf-8", newline="\n")
        self.lock = threading.Lock()

    def add(self, kind, **fields):
        """先持久化意图再发送；Persist intentions before the network operation."""
        row = {"kind": kind, "monotonic_ns": time.monotonic_ns(), **fields}
        with self.lock:
            self.stream.write(json.dumps(serializable(row), ensure_ascii=False) + "\n")
            self.stream.flush()
            os.fsync(self.stream.fileno())
        return row

    def close(self):
        """在线程退出后关闭宿主证据；Close host evidence after workers have stopped."""
        self.stream.close()


def event_id(event):
    """不使用跨分片消费顺序；Never infer order across different shards."""
    value = event["event_id"]
    return (value["stream_id"], value["db"], value["shard"], value["sequence"])


def normalize_event(event):
    """验证并规范化线上事件身份；Validate and normalize wire event identity."""
    if not isinstance(event, dict) or set(event) != {"event_id", "key", "event_type", "generation", "revision", "timestamp_ms"}:
        raise WireError("LOCAL_EVENT: invalid event fields")
    identity, generation = event["event_id"], event["generation"]
    if (not isinstance(identity, dict) or set(identity) != {"stream_id", "db", "shard", "sequence"}
            or not isinstance(identity["stream_id"], bytes) or len(identity["stream_id"]) != 16
            or any(type(identity[name]) is not int for name in ("db", "shard", "sequence"))
            or identity["db"] != 0 or not 0 <= identity["shard"] < 2 or identity["sequence"] < 1
            or not isinstance(generation, dict) or set(generation) != {"epoch", "counter"}
            or not isinstance(generation["epoch"], bytes) or len(generation["epoch"]) != 16
            or type(generation["counter"]) is not int or generation["counter"] < 1
            or type(event["revision"]) is not int or event["revision"] < 1
            or type(event["timestamp_ms"]) is not int or event["timestamp_ms"] < 0
            or event["event_type"] != "KEY_CREATED" or not isinstance(event["key"], str) or not event["key"]):
        raise WireError("LOCAL_EVENT: invalid event identity or metadata")
    result = dict(event)
    result["event_id"] = {**identity, "stream_id": identity["stream_id"].hex()}
    result["generation"] = {**generation, "epoch": generation["epoch"].hex()}
    return result


def parse_journal(data):
    """独立只读解析 v1 完整事务；短尾分类不替服务端修复。
    Independently read complete v1 transactions; classify tails without repairing them.
    """
    result = {"status": "valid", "transactions": [], "events": [], "offset": 0}
    if (len(data) < 40 or data[:8] != b"LGJRNL01" or int.from_bytes(data[8:12], "big") != 1
            or zlib.crc32(data[:36]) != int.from_bytes(data[36:40], "big")):
        return {**result, "status": "corrupt", "reason": "header"}
    result.update(stream_id=data[12:28].hex(), databases=int.from_bytes(data[28:32], "big"),
                  shards=int.from_bytes(data[32:36], "big"))
    if not 1 <= result["databases"] <= 65536 or not 1 <= result["shards"] <= 65536:
        return {**result, "status": "corrupt", "reason": "topology"}
    position, sequences = 40, {}
    while position < len(data):
        result["offset"] = position
        if len(data) - position < 8:
            return {**result, "status": "repairable_tail", "reason": "short_header"}
        size, checksum = struct.unpack_from(">II", data, position)
        end = position + 8 + size
        if not 0 < size <= MAX_FILE or end > len(data):
            return {**result, "status": "corrupt", "reason": "length"}
        payload = data[position + 8:end]
        if zlib.crc32(payload) != checksum:
            return {**result, "status": "corrupt", "reason": "crc"}
        trailer = data[end:end + 8]
        if trailer != COMMIT:
            status = "repairable_tail" if len(trailer) < 8 and COMMIT.startswith(trailer) else "corrupt"
            return {**result, "status": status, "reason": "commit_trailer"}
        if payload[0] == 1:
            try:
                cursor = 1
                count = int.from_bytes(payload[cursor:cursor + 4], "big")
                cursor += 4
                if not count or count > len(payload) // 60:
                    raise ValueError("event count")
                transaction = []
                for _ in range(count):
                    if cursor + 19 > len(payload):
                        raise ValueError("event header")
                    db, shard, sequence, kind, key_size = struct.unpack_from(">HIQBI", payload, cursor)
                    cursor += 19
                    if not key_size or cursor + key_size + 40 > len(payload):
                        raise ValueError("event key")
                    key = payload[cursor:cursor + key_size].decode("utf-8")
                    cursor += key_size
                    epoch = payload[cursor:cursor + 16].hex()
                    counter, revision, timestamp = struct.unpack_from(">QQQ", payload, cursor + 16)
                    cursor += 40
                    if (db >= result["databases"] or shard >= result["shards"] or kind != 1
                            or not counter or not revision or sequence != sequences.get((db, shard), 0) + 1):
                        raise ValueError("event coordinates or sequence")
                    sequences[(db, shard)] = sequence
                    transaction.append({"event_id": {"stream_id": result["stream_id"], "db": db,
                                                     "shard": shard, "sequence": sequence},
                                        "event_type": "KEY_CREATED", "key": key,
                                        "generation": {"epoch": epoch, "counter": counter},
                                        "revision": revision, "timestamp_ms": timestamp})
                if cursor != len(payload):
                    raise ValueError("event trailing bytes")
            except (ValueError, UnicodeDecodeError, struct.error) as error:
                return {**result, "status": "corrupt", "reason": str(error)}
            result["transactions"].append(transaction)
            result["events"].extend(transaction)
        elif payload[0] != 2:
            return {**result, "status": "corrupt", "reason": "unexpected record tag or compaction"}
        position = end + 8
    result["offset"] = position
    return result


def load_evidence(path):
    """从落盘证据重建已确认与未知结果；Rebuild confirmed/unknown outcomes from the persisted ledger."""
    batches, acked, unknown_acks, seen, holes = {}, set(), set(), {}, set()
    with path.open(encoding="utf-8") as stream:
        for line in stream:
            row = json.loads(line)
            kind = row["kind"]
            if kind == "write_intent":
                if row["batch"] in batches:
                    raise ValueError("duplicate write intent")
                batches[row["batch"]] = {"batch": row["batch"], "keys": row["keys"], "status": "unknown"}
            elif kind == "write_result":
                batches[row["batch"]]["status"] = row["status"]
            elif kind == "event_before":
                event = row["event"]
                if event_id(event) in seen and seen[event_id(event)] != event:
                    raise ValueError("pre-crash duplicate event changed metadata")
                seen[event_id(event)] = event
            elif kind == "ack_intent" and row["phase"] == "before":
                unknown_acks.add(tuple(row["id"]))
            elif kind == "ack_success" and row["phase"] == "before":
                identity = tuple(row["id"])
                acked.add(identity)
                unknown_acks.discard(identity)
            elif kind == "hole":
                holes.add(tuple(row["id"]))
    return {"batches": list(batches.values()), "acked": acked, "unknown_acks": unknown_acks,
            "seen": seen, "holes": holes}


def judge_recovery(before, parsed, audit, work, stream_id, cache_size, continuation=()):
    """按独立集合判定事务/ACK，不把未知当失败或假定跨分片顺序。
    Judge transaction/ACK sets without treating unknown as absent or imposing cross-shard order.
    """
    failures, resolved = [], []
    if parsed["status"] not in ("valid", "repairable_tail"):
        failures.append("crash journal is not a valid committed prefix")
    if parsed.get("stream_id") != stream_id:
        failures.append("stream_id changed across recovery")
    if cache_size != 0:
        failures.append("cache values were restored")
    expected = {event_id(event): event for event in parsed["events"]}
    keys = Counter(event["key"] for event in parsed["events"])
    if len(expected) != len(parsed["events"]) or any(count != 1 for count in keys.values()):
        failures.append("duplicate committed identity or key")
    key_batches = {key: batch["batch"] for batch in before["batches"] for key in batch["keys"]}
    for batch in before["batches"]:
        present = sum(keys[key] > 0 for key in batch["keys"])
        complete = present == len(batch["keys"])
        if present not in (0, len(batch["keys"])):
            failures.append("partial transaction: " + batch["batch"])
        if batch["status"] == "success" and not complete:
            failures.append("successful batch missing: " + batch["batch"])
        if batch["status"] == "rejected" and present:
            failures.append("rejected batch committed: " + batch["batch"])
        if batch["status"] == "unknown":
            resolved.append({"batch": batch["batch"], "recovered": present, "total": len(batch["keys"])})
    for transaction in parsed["transactions"]:
        owners = {key_batches.get(event["key"]) for event in transaction}
        if None in owners or len(owners) != 1:
            failures.append("journal transaction contains unexpected or mixed batch keys")
        elif {event["key"] for event in transaction} != set(next(
                batch["keys"] for batch in before["batches"] if batch["batch"] in owners)):
            failures.append("MSET was split into physical transactions")
    audit_map = {event_id(event): event for event in audit}
    if audit_map != expected or any(expected.get(event_id(event)) != event for event in audit):
        failures.append("audit replay differs from committed journal events")
    for identity, event in before["seen"].items():
        if expected.get(identity) != event:
            failures.append("pre-crash delivered event missing or changed")
    work_ids = {event_id(event) for event in work}
    if any(expected.get(event_id(event)) != event for event in work):
        failures.append("work-group replay metadata changed")
    if work_ids & before["acked"]:
        failures.append("confirmed ACK was redelivered")
    mandatory = set(expected) - before["acked"] - before["unknown_acks"]
    if not mandatory <= work_ids:
        failures.append("unacknowledged events or ACK holes were lost")
    if not work_ids <= set(expected):
        failures.append("unexpected work-group replay")
    if not before["holes"] or not before["holes"] <= mandatory:
        failures.append("no valid pre-crash ACK hole")
    if not any(hole[:3] == ack[:3] and hole[3] < ack[3]
               for hole in before["holes"] for ack in before["acked"]):
        failures.append("no confirmed ACK beyond a same-shard hole")
    tails = {}
    for identity in expected:
        tails[identity[:3]] = max(tails.get(identity[:3], 0), identity[3])
    for event in sorted(continuation, key=event_id):
        identity = event_id(event)
        if identity[0] != stream_id or identity[3] != tails.get(identity[:3], 0) + 1:
            failures.append("continuation sequence is not the next per-shard value")
        tails[identity[:3]] = identity[3]
    if not continuation:
        failures.append("no post-recovery continuation events")
    return {"valid": not failures, "failures": sorted(set(failures)), "unknown_commits": resolved,
            "committed_events": len(expected), "confirmed_acks": len(before["acked"]),
            "unknown_acks": len(before["unknown_acks"]), "audit_duplicates": len(audit) - len(audit_map),
            "work_duplicates": len(work) - len(work_ids)}


# Linux 端只运行固定动作；所有路径再做 realpath、marker 与 ext4 校验。
# Linux runs fixed actions only; realpath, marker and ext4 checks repeat on that side.
LINUX = r'''
import hashlib, json, os, pathlib, signal, socket, subprocess, sys
base = pathlib.Path('/root/ling-acceptance')
marker = base / '.wsl-crash-acceptance'
action, raw = sys.argv[1:3]
args = json.loads(raw)
if base.resolve() != base or marker.is_symlink() or marker.read_text() != 'ling-acceptance\n':
    raise RuntimeError('Dedicated acceptance marker is missing or invalid')
def checked(value, exists=True):
    path = pathlib.Path(value)
    if not str(path).startswith(str(base) + '/') or path.resolve() != path:
        raise RuntimeError('Path escapes dedicated acceptance root')
    parent = path if path.exists() else path.parent
    while not parent.exists(): parent = parent.parent
    if subprocess.check_output(['findmnt', '-n', '-o', 'FSTYPE', '-T', str(parent)], text=True).strip() != 'ext4':
        raise RuntimeError('Acceptance paths must reside on ext4')
    if exists and not path.exists(): raise RuntimeError('Required path does not exist')
    return path
def owned(run):
    pid = int((run / 'server.pid').read_text())
    proc = pathlib.Path('/proc') / str(pid)
    if pid < 2 or proc.joinpath('exe').resolve() != checked(args['binary']):
        raise RuntimeError('Server PID no longer identifies the acceptance binary')
    command = proc.joinpath('cmdline').read_bytes().split(b'\x00')
    if command[:3] != [args['binary'].encode(), b'--config', str(run / 'ling.toml').encode()]:
        raise RuntimeError('Server PID is not owned by this acceptance cycle')
    return pid
if action == 'preflight':
    binary, source, session = checked(args['binary']), checked(args['source']), checked(args['session'], False)
    if session.exists(): raise RuntimeError('Linux session output already exists')
    print(json.dumps({'binary_sha256': hashlib.file_digest(binary.open('rb'), 'sha256').hexdigest(),
      'binary_version': subprocess.check_output([str(binary), '--version'], text=True).strip(),
      'source_revision': subprocess.check_output(['git', '-C', str(source), 'rev-parse', 'HEAD'], text=True).strip(),
      'source_status': subprocess.check_output(['git', '-C', str(source), 'status', '--porcelain'], text=True),
      'uname': list(os.uname()), 'cpuinfo': pathlib.Path('/proc/cpuinfo').read_text(),
      'meminfo': pathlib.Path('/proc/meminfo').read_text(),
      'mount': subprocess.check_output(['findmnt', '-J', '-T', str(base)], text=True)}))
elif action == 'probe':
    with socket.socket() as sock:
        sock.settimeout(10); sock.bind(('0.0.0.0', args['port'])); sock.listen(1)
        with sock.accept()[0] as conn:
            conn.settimeout(5); data = conn.recv(128); conn.sendall(data)
elif action == 'prepare':
    run = checked(args['run'], False)
    run.mkdir(parents=True, exist_ok=False)
    (run / 'ling.toml').write_text(args['config'])
elif action == 'launch':
    run, binary = checked(args['run']), checked(args['binary'])
    if hashlib.file_digest(binary.open('rb'), 'sha256').hexdigest() != args['sha256']:
        raise RuntimeError('Acceptance binary changed')
    (run / 'server.pid').write_text(str(os.getpid()))
    os.chdir(run)
    log = os.open(run / args['log'], os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    os.dup2(log, 1); os.dup2(log, 2); os.close(log)
    os.execv(binary, [str(binary), '--config', str(run / 'ling.toml')])
elif action == 'guard':
    run = checked(args['run']); print(owned(run))
elif action == 'stop':
    run = checked(args['run'])
    try: os.kill(owned(run), signal.SIGTERM)
    except FileNotFoundError: pass
elif action == 'manifest':
    run = checked(args['run']); files = []
    for path in sorted(run.rglob('*')):
        if path.is_symlink(): raise RuntimeError('Unexpected symlink in evidence')
        if path.is_file():
            checked(str(path))
            if path.stat().st_size > 268435456: raise RuntimeError('Evidence file budget exceeded')
            files.append({'path': str(path.relative_to(run)), 'size': path.stat().st_size})
    print(json.dumps(files))
elif action == 'export':
    path = checked(args['path'])
    if not path.is_file() or path.stat().st_size > 268435456: raise RuntimeError('Invalid evidence file')
    with path.open('rb') as stream:
        while data := stream.read(1048576): sys.stdout.buffer.write(data)
else: raise RuntimeError('Unknown fixed acceptance action')
'''


class Wsl:
    """唯一发行版入口，不执行 shell 拼接；One distro entry point without shell interpolation."""
    def __init__(self, name):
        self.name = distro_name(name)

    def command(self, action, **args):
        """动作及参数走 argv 和 JSON；Pass fixed actions through argv and JSON."""
        return ["wsl.exe", "--distribution", distro_name(self.name), "--user", "root", "--exec", "python3", "-c",
                LINUX, action, json.dumps(args)]

    def run(self, action, **args):
        """限时读取环境与文件清单；Bound environment and manifest requests."""
        result = subprocess.run(self.command(action, **args), capture_output=True, timeout=30)
        if result.returncode:
            raise RuntimeError(result.stderr.decode("utf-8", errors="replace").strip())
        return result.stdout.decode("utf-8").strip()

    def terminate(self, run, binary, on_request=None):
        """每次先校验专用标记和服务器 PID，再定点终止；Guard marker and server PID before targeted termination."""
        self.run("guard", run=linux_path(run), binary=linux_path(binary))
        if on_request is not None:
            on_request()
        result = subprocess.run(["wsl.exe", "--terminate", distro_name(self.name)],
                                capture_output=True, timeout=30)
        return {"returncode": result.returncode, "stdout_hex": result.stdout.hex(), "stderr_hex": result.stderr.hex()}

    def snapshot(self, run, target):
        """只读导出崩溃文件；不调用 sync、fsync 或修复。
        Export crash files read-only without sync, fsync or repair on Linux.
        """
        target.mkdir()
        manifest = json.loads(self.run("manifest", run=run))
        for item in manifest:
            relative = PurePosixPath(item["path"])
            if relative.is_absolute() or ".." in relative.parts:
                raise RuntimeError("Invalid relative evidence path")
            destination = target.joinpath(*relative.parts)
            destination.parent.mkdir(parents=True, exist_ok=True)
            with destination.open("xb") as stream:
                result = subprocess.run(self.command("export", path=run + "/" + str(relative)),
                                        stdout=stream, stderr=subprocess.PIPE, timeout=60)
                stream.flush()
                os.fsync(stream.fileno())
            if result.returncode or destination.stat().st_size != item["size"]:
                raise RuntimeError("Evidence export failed: " + str(relative))
            item["sha256"] = hashlib.sha256(destination.read_bytes()).hexdigest()
        write_json(target / "manifest.json", manifest)


def free_port():
    """宿主回环确认后选择未占用端口；Choose a free port after proving host loopback."""
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def close_client(client):
    """取消本进程的阻塞 socket 读取；Unblock socket reads owned by this process."""
    try:
        client.stream.shutdown(socket.SHUT_RDWR)
    except OSError:
        pass
    client.close()


def forwarding_probe(wsl):
    """实际验证 Windows localhost 到专用 WSL；Prove Windows localhost reaches the dedicated WSL distro."""
    port, token = free_port(), uuid.uuid4().hex.encode()
    process = subprocess.Popen(wsl.command("probe", port=port), stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    deadline, connected = time.monotonic() + 10, False
    while time.monotonic() < deadline and process.poll() is None:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=.5) as stream:
                stream.sendall(token)
                connected = stream.recv(128) == token
            break
        except OSError:
            time.sleep(.05)
    stdout, stderr = process.communicate(timeout=15)
    if not connected or process.returncode:
        raise RuntimeError("WSL localhost forwarding probe failed: " + stderr.decode("utf-8", errors="replace"))
    return {"port": port, "nonce": token.decode(), "verified": True, "stdout_hex": stdout.hex()}


def configuration(port):
    """固定两分片、有界交付窗口、长租约；Fix shards, a bounded delivery window and long leases."""
    return f'''[server]
host="0.0.0.0"
port={port}
max_connections=16
idle_timeout_ms=600000
request_timeout_ms=30000
max_request_bytes=1048576
max_response_bytes=1048576
[engine]
shard_count=2
max_memory_bytes=536870912
[events]
mode="reliable"
buffer_bytes=67108864
max_groups=8
max_pending_events={MAX_PENDING_EVENTS}
retry_interval_ms=600000
consumer_timeout_ms=600000
[event_journal]
dir="journal"
max_bytes=268435456
[log]
level="info"
'''


def connect_ready(port, process, seconds):
    """握手成功且进程仍在运行才判就绪；Require a handshake and a live server before readiness."""
    deadline, last = time.monotonic() + seconds, None
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError("Acceptance server exited during startup")
        try:
            client = Client(port)
            if not client.hello.get("reliable_events") or not client.hello.get("events"):
                client.close()
                raise RuntimeError("Server did not enable Reliable events")
            return client
        except (OSError, WireError) as error:
            last = error
            time.sleep(.05)
    raise RuntimeError("Server/localhost readiness timed out: " + str(last))


def next_event(client, deadline):
    """只在帧可读时读整帧；截止期不丢弃半帧。
    Read complete frames only after readiness; deadlines never discard partial frames.
    """
    if client.events:
        return normalize_event(client.events.popleft())
    while time.monotonic() < deadline:
        if not select.select([client.stream], [], [], max(0, deadline - time.monotonic()))[0]:
            return None
        kind, _, value = receive(client.stream)
        if kind == 3:
            return normalize_event(value)
        if kind != 5:
            raise WireError("LOCAL_PROTOCOL: unexpected subscription control response")
    return None


def acknowledge(client, event, evidence, phase):
    """ACK 意图和成功分开落盘；Persist ACK intent and success separately."""
    identity = event_id(event)
    evidence.add("ack_intent", id=identity, phase=phase)
    if client.request("ACK", event["event_id"] | {"stream_id": bytes.fromhex(identity[0])}, kind=4) is not True:
        raise WireError("LOCAL_RESULT: ACK was not true")
    evidence.add("ack_success", id=identity, phase=phase)


def write_batch(client, number, count, evidence, phase="before"):
    """唯一 Key 标识每个 MSET；Unique keys identify every MSET transaction."""
    keys = [f"{phase}:b{number:06d}:k{index:02d}" for index in range(count)]
    batch = f"{phase}:{number}"
    evidence.add("write_intent", batch=batch, keys=keys, phase=phase)
    try:
        value = client.request("MSET", [part for key in keys for part in (key, b"wsl-crash-value")])
        if value is not True:
            raise WireError("LOCAL_RESULT: MSET was not true")
    except (OSError, WireError) as error:
        text = str(error)
        status = "rejected" if text.startswith("ERR_") and not text.startswith("ERR_COMMIT_UNKNOWN") else "unknown"
        evidence.add("write_result", batch=batch, status=status, error=text, phase=phase)
        raise
    evidence.add("write_result", batch=batch, status="success", phase=phase)
    return keys


def drain(client, expected, evidence, phase, seconds, optional=()):
    """已知集合加静默窗口及 PING 栅栏；Use a known set, quiet window and responsiveness fence."""
    deadline, received, seen = time.monotonic() + seconds, [], set()
    quiet = False
    allowed = set(expected) | set(optional)
    while time.monotonic() < deadline:
        complete = set(expected) <= seen
        event = next_event(client, min(deadline, time.monotonic() + .3) if complete else deadline)
        if event is None:
            if complete:
                if client.request("PING") != "PONG":
                    raise WireError("LOCAL_RESULT: recovery PING failed")
                if not client.events and not select.select([client.stream], [], [], .3)[0]:
                    quiet = True
                    break
            continue
        identity = event_id(event)
        evidence.add("recovered_event", event=event, phase=phase)
        received.append(event)
        seen.add(identity)
        if identity not in allowed:
            raise WireError("LOCAL_RECOVERY: unexpected or confirmed-ACK event " + repr(identity))
        acknowledge(client, event, evidence, phase)
    evidence.add("drain_end", phase=phase, required=len(expected), allowed=len(allowed),
                 distinct=len(seen), received=len(received), complete=set(expected) <= seen,
                 pending_inbox=len(client.events), quiet_fence_confirmed=quiet,
                 quiet_window_seconds=.6, deadline_reached=time.monotonic() >= deadline)
    if not quiet:
        raise WireError("LOCAL_RECOVERY: drain deadline reached without a complete quiet fence")
    return received


def run_cycle(args, wsl, environment, session, number, rng):
    """每轮独立 Journal，崩溃前不停负载；Fresh journal per cycle with active load until termination."""
    target = args.output / f"cycle-{number:03d}"
    target.mkdir()
    run = session + f"/cycle-{number:03d}"
    port, evidence = free_port(), Evidence(target / "evidence.jsonl")
    row = {"cycle": number, "linux_path": run, "port": port, "status": "failed", "failures": [],
           "limitations": ["WSL VM/process crash, not physical power loss", "host/WSL scheduling cannot prove the kernel's exact in-flight instruction"]}
    clients, workers, processes, launch_logs = [], [], [], []
    stop, writing = threading.Event(), threading.Event()
    state = {"attempts": 0, "successes": 0, "worker_errors": [], "in_flight": False}
    config = configuration(port)
    (target / "ling.toml").write_text(config, encoding="utf-8")

    def start(label):
        """日志保留两侧；Retain both Linux server and host launcher logs."""
        log = (target / (label + "-launcher.log")).open("xb")
        launch_logs.append(log)
        process = subprocess.Popen(wsl.command("launch", run=run, binary=args.binary,
                                               sha256=environment["binary_sha256"], log=label + ".log"),
                                   stdout=log, stderr=log)
        processes.append(process)
        return process

    try:
        wsl.run("prepare", run=run, config=config)
        process = start("before")
        writer = connect_ready(port, process, args.startup_seconds)
        clients.append(writer)
        row["hello_before"] = serializable(writer.hello)
        audit = Client(port)
        audit.subscribe("before:*", "audit")
        close_client(audit)
        subscriber = Client(port)
        clients.append(subscriber)
        subscriber.subscribe("before:*", "work")
        write_batch(writer, 0, args.keys_per_batch, evidence)
        holes = {}
        for _ in range(args.keys_per_batch):
            event = next_event(subscriber, time.monotonic() + args.recovery_seconds)
            if event is None:
                raise RuntimeError("Warmup event missing")
            evidence.add("event_before", event=event)
            identity = event_id(event)
            if identity[2] not in holes:
                holes[identity[2]] = identity
                evidence.add("hole", id=identity)
            else:
                acknowledge(subscriber, event, evidence, "before")

        def consume():
            """保留早期洞，后续 ACK 正常落盘；Retain early holes while persisting later ACKs."""
            try:
                while not stop.is_set():
                    event = next_event(subscriber, time.monotonic() + .2)
                    if event is not None:
                        evidence.add("event_before", event=event)
                        if event_id(event) not in holes.values():
                            acknowledge(subscriber, event, evidence, "before")
            except (OSError, WireError) as error:
                state["worker_errors"].append({"worker": "consumer", "error": str(error),
                                               "monotonic_ns": time.monotonic_ns()})

        def produce():
            """不断生成新批次，主线程终止前不主动完成；Generate batches until the main thread terminates WSL."""
            writing.set()
            try:
                while not stop.is_set():
                    state["attempts"] += 1
                    state["in_flight"] = True
                    write_batch(writer, state["attempts"], args.keys_per_batch, evidence)
                    state["in_flight"] = False
                    state["successes"] += 1
                    stop.wait(args.write_pause)
            except (OSError, WireError) as error:
                state["worker_errors"].append({"worker": "writer", "error": str(error),
                                               "monotonic_ns": time.monotonic_ns()})
            finally:
                state["in_flight"] = False
                writing.clear()

        workers = [threading.Thread(target=consume, daemon=True), threading.Thread(target=produce, daemon=True)]
        for worker in workers:
            worker.start()
        delay = rng.uniform(args.crash_min_seconds, args.crash_max_seconds)
        time.sleep(delay)
        deadline = time.monotonic() + 2
        while writing.is_set() and not state["in_flight"] and time.monotonic() < deadline:
            time.sleep(.001)
        if not writing.is_set() or not state["in_flight"]:
            raise RuntimeError("Writer stopped before termination; cycle cannot claim active-load crash")
        def mark_termination():
            """在最后一次保护检查之后记录真实发起边界；Record invocation after the final guard check."""
            if not writing.is_set():
                raise RuntimeError("Writer stopped during termination guard")
            row["termination_boundary"] = {"random_delay_seconds": delay, "writer_running": writing.is_set(),
                                           "in_flight_at_request": state["in_flight"], "attempts": state["attempts"],
                                           "successes": state["successes"], "monotonic_ns": time.monotonic_ns()}
            evidence.add("terminate_requested", **row["termination_boundary"])
        row["terminate"] = wsl.terminate(run, args.binary, mark_termination)
        evidence.add("terminate_result", **row["terminate"])
        if row["terminate"]["returncode"]:
            raise RuntimeError("Targeted WSL termination failed")
        row["crashed_launcher_returncode"] = process.wait(timeout=30)
        stop.set()
        for client in clients:
            close_client(client)
        for worker in workers:
            worker.join(timeout=15)
        if any(worker.is_alive() for worker in workers):
            raise RuntimeError("Host workload did not stop after termination")
        row["load"] = state.copy()
        if any(error["monotonic_ns"] < row["termination_boundary"]["monotonic_ns"]
               for error in state["worker_errors"]):
            row["failures"].append("workload failed before the termination request")
        wsl.snapshot(run, target / "crashed")
        raw = (target / "crashed" / "journal" / "journal.lgj").read_bytes()
        parsed = parse_journal(raw)
        write_json(target / "crash-journal.json", parsed)
        before = load_evidence(target / "evidence.jsonl")
        process = start("recovery")
        try:
            observer = connect_ready(port, process, args.startup_seconds)
        except RuntimeError as error:
            wsl.snapshot(run, target / "rejected")
            recovered_raw = (target / "rejected" / "journal" / "journal.lgj").read_bytes()
            server_log = (target / "rejected" / "recovery.log").read_text(encoding="utf-8", errors="replace")
            safe = process.poll() is not None and "ERR_JOURNAL_UNAVAILABLE" in server_log and recovered_raw == raw
            row.update(status="safe_rejection" if safe else "failed", startup_error=str(error),
                       raw_preserved=recovered_raw == raw, recovery_semantics_verified=False)
            row["failures"].append("startup rejected; complete recovery could not be verified")
            return row
        clients.append(observer)
        row["hello_after"] = serializable(observer.hello)
        cache_size = observer.request("DBSIZE")
        audit = Client(port)
        clients.append(audit)
        audit.subscribe("before:*", "audit")
        expected = {event_id(event) for event in parsed["events"]}
        audit_events = drain(audit, expected, evidence, "audit", args.recovery_seconds)
        replay = Client(port)
        clients.append(replay)
        replay.subscribe("before:*", "work")
        work_events = drain(replay, expected - before["acked"] - before["unknown_acks"], evidence,
                            "work", args.recovery_seconds, before["unknown_acks"])
        continuation_client = Client(port)
        clients.append(continuation_client)
        continuation_client.subscribe("after:*", "continuation")
        continuation_keys = write_batch(observer, 0, args.keys_per_batch, evidence, "after")
        continuation = []
        for _ in range(args.keys_per_batch):
            event = next_event(continuation_client, time.monotonic() + args.recovery_seconds)
            if event is None:
                break
            evidence.add("continuation", event=event)
            continuation.append(event)
            acknowledge(continuation_client, event, evidence, "continuation")
        judgement = judge_recovery(before, parsed, audit_events, work_events,
                                   observer.hello["stream_id"].hex(), cache_size, continuation)
        if len(continuation) != args.keys_per_batch or {event["key"] for event in continuation} != set(continuation_keys):
            judgement["failures"].append("continuation batch incomplete")
            judgement["valid"] = False
        row.update(status="passed" if judgement["valid"] else "failed", recovery=judgement,
                   recovery_semantics_verified=judgement["valid"])
        row["failures"].extend(judgement["failures"])
    except Exception as error:
        row["failures"].append(type(error).__name__ + ": " + str(error))
    finally:
        stop.set()
        for client in clients:
            try:
                close_client(client)
            except OSError:
                pass
        for worker in workers:
            worker.join(timeout=15)
        if processes and processes[-1].poll() is None:
            try:
                wsl.run("stop", run=run, binary=args.binary)
                processes[-1].wait(timeout=15)
            except Exception as error:
                row["failures"].append("Owned server cleanup: " + str(error))
        try:
            wsl.snapshot(run, target / "final")
        except Exception as error:
            row["failures"].append("Final evidence export: " + str(error))
        if any(worker.is_alive() for worker in workers):
            row["failures"].append("Host worker still alive")
        for log in launch_logs:
            log.close()
        evidence.close()
        if row["failures"] and row["status"] == "passed":
            row["status"] = "failed"
        write_json(target / "result.json", row)
    return row


def parser():
    """执行必须显式开启，默认三十轮；Require explicit execution, defaulting to thirty cycles."""
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--execute", action="store_true", help="run targeted WSL termination cycles")
    result.add_argument("--distro", type=distro_name, default=DISTRO)
    result.add_argument("--binary", type=linux_path, required=True)
    result.add_argument("--source", type=linux_path, default=BASE + "/source")
    result.add_argument("--output", type=windows_output, required=True)
    result.add_argument("--cycles", type=int, default=30)
    result.add_argument("--seed", type=int, default=20261118)
    result.add_argument("--keys-per-batch", type=int, default=8)
    result.add_argument("--crash-min-seconds", type=float, default=.25)
    result.add_argument("--crash-max-seconds", type=float, default=1.5)
    result.add_argument("--write-pause", type=float, default=.02)
    result.add_argument("--startup-seconds", type=float, default=30)
    result.add_argument("--recovery-seconds", type=float, default=60)
    return result


def main():
    """先保护环境，再产生独立循环和汇总；Guard the environment before creating isolated cycles and summary."""
    args = parser().parse_args()
    if not args.execute:
        raise SystemExit("No operations performed; --execute is required")
    if os.name != "nt":
        raise SystemExit("Run this controller from Windows, outside the terminated WSL distro")
    if not (1 <= args.cycles <= 1000 and 4 <= args.keys_per_batch <= 64
            and 0 < args.crash_min_seconds <= args.crash_max_seconds <= 10
            and .01 <= args.write_pause <= .2 and 1 <= args.startup_seconds <= 120
            and 1 <= args.recovery_seconds <= 300):
        raise SystemExit("Invalid bounded acceptance parameters")
    args.output = Path(args.output)
    if args.output.exists() or not args.output.parent.is_dir():
        raise SystemExit("Output must not exist and its Windows parent must exist")
    args.output = args.output.parent.resolve(strict=True) / args.output.name
    windows_output(str(args.output))
    session = BASE + "/crash-runs/" + datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ-") + uuid.uuid4().hex[:12]
    wsl = Wsl(args.distro)
    args.output.mkdir()
    request = {"parameters": vars(args) | {"output": str(args.output)}, "linux_session": session,
               "marker": MARKER, "host": {"platform": platform.platform(), "python": sys.version},
               "tool_sources": snapshot_tools(args.output / "tool-source"),
               "provenance_limit": "Source revision and binary hash are observed independently; their build relationship is not attested by this controller",
               "scope": "WSL crash recovery; not physical power loss"}
    write_json(args.output / "request.json", request)
    try:
        environment = json.loads(wsl.run("preflight", binary=args.binary, source=args.source, session=session))
        forwarding = forwarding_probe(wsl)
    except Exception as error:
        write_json(args.output / "setup-failure.json", {"error": type(error).__name__ + ": " + str(error)})
        return 1
    write_json(args.output / "run.json", request | {"environment": environment, "localhost_forwarding": forwarding})
    rng, results = random.Random(args.seed), []
    for number in range(1, args.cycles + 1):
        row = run_cycle(args, wsl, environment, session, number, rng)
        results.append(row)
        print(json.dumps({"cycle": number, "status": row["status"], "failures": row["failures"]}), flush=True)
    summary = {"planned_cycles": args.cycles, "completed_cycles": len(results),
               "statuses": dict(Counter(row["status"] for row in results)), "cycles": results,
               "all_passed": all(row["status"] == "passed" for row in results)}
    write_json(args.output / "summary.json", summary)
    return 0 if summary["all_passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
