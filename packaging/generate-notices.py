#!/usr/bin/env python3
"""汇集发行目标的依赖许可原文；Collect dependency license texts for a release target."""

import argparse
import json
import os
import re
import subprocess
from html.parser import HTMLParser
from pathlib import Path


class LibraryText(HTMLParser):
    """保留标准库声明的正文与段落；Preserve standard-library notice text and paragraphs."""

    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.parts = []

    def handle_data(self, data):
        self.parts.append(data)

    def handle_endtag(self, tag):
        if tag in {"p", "pre", "div", "li", "h1", "h2", "h3"}:
            self.parts.append("\n")


def command(*args):
    """命令失败即停止打包；Stop packaging if the dependency inventory cannot be read."""
    return subprocess.check_output(args, text=True, encoding="utf-8")


def append_text(parts, heading, text):
    """每份原文保持可辨认的边界；Keep an identifiable boundary around each source text."""
    parts.extend(["\n" + "=" * 78 + "\n", heading + "\n", "=" * 78 + "\n", text, "\n"])


def main():
    """依据锁文件和当前编译器生成随包声明；Generate notices from locked crates and rustc."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    # 与构建使用同一工作区；与开发依赖分开取图，避免测试专用组件进入发行声明。
    # Use the build workspace and omit dev edges, keeping test-only components out of this inventory.
    os.chdir(root)
    metadata = json.loads(command("cargo", "metadata", "--locked", "--format-version", "1"))
    tree = command("cargo", "tree", "--locked", "-p", "ling-server", "-p", "ling-cli",
                   "-e", "normal,build", "--target", args.target, "--prefix", "none", "--format", "{p}")
    selected = set(re.findall(r"^([^ ]+) v(\S+)", tree, re.MULTILINE))
    packages = sorted((p for p in metadata["packages"]
                       if p["source"] and (p["name"], p["version"]) in selected),
                      key=lambda p: (p["name"], p["version"]))
    if not packages:
        raise RuntimeError("Release dependency inventory is empty")
    parts = ["Third-party notices for ling\n",
             "第三方组件保留各自许可；项目根 LICENSE 仅适用于本项目自有材料。\n",
             "Third-party components retain their own terms; the root LICENSE covers ling-owned material.\n",
             f"Target: {args.target}\n",
             "Scope: locked normal/build dependencies of ling and ling-cli; includes build-time components.\n",
             "Alternative license texts are retained together without changing their upstream terms.\n"]
    for package in packages:
        directory = Path(package["manifest_path"]).parent
        files = {p for p in directory.rglob("*") if p.is_file() and re.match(
            r"^(LICENSE|LICENCE|COPYING|NOTICE|COPYRIGHT|AUTHORS)([._-]|$)", p.name, re.IGNORECASE)}
        if package["license_file"]:
            files.add(directory / package["license_file"])
        if not files:
            raise RuntimeError(f"Missing license texts: {package['name']} {package['version']}")
        for path in sorted(files):
            heading = (f"{package['name']} {package['version']} | {package['license']}\n"
                       f"Source: https://crates.io/crates/{package['name']}/{package['version']}\n"
                       f"File: {path.relative_to(directory).as_posix()}")
            append_text(parts, heading, path.read_text(encoding="utf-8"))
    # 标准库不在 Cargo.lock 中；保留当前编译器自带清单及许可全文，包括内嵌组件。
    # The standard library is absent from Cargo.lock; include this compiler's notices and license texts.
    sysroot = Path(command("rustc", "--print", "sysroot").strip())
    rust_docs = sysroot / "share/doc/rust"
    library_html = (rust_docs / "COPYRIGHT-library.html").read_text(encoding="utf-8")
    library = LibraryText()
    library.feed(library_html)
    append_text(parts, "Rust standard library | " + command("rustc", "--version").strip(),
                "".join(library.parts))
    expressions = re.findall(r"<b>License:</b> ([^<]+)", library_html)
    license_ids = set(re.findall(r"[A-Za-z0-9][A-Za-z0-9.+-]*", " ".join(expressions)))
    templates = sorted(path for path in (rust_docs / "licenses").glob("*.txt")
                       if path.stem in license_ids)
    if not templates:
        raise RuntimeError("Rust standard-library license texts are unavailable")
    for path in templates:
        append_text(parts, "Rust standard-library license text: " + path.name,
                    path.read_text(encoding="utf-8"))
    if "linux-musl" in args.target:
        # musl-tools 的运行库被静态链接；读取 CI 所安装发行包的实际声明。
        # musl-tools' runtime is linked statically; copy the notice of the installed CI package.
        append_text(parts, "musl C library | installed distribution copyright",
                    Path("/usr/share/doc/musl/copyright").read_text(encoding="utf-8"))
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text("".join(parts), encoding="utf-8", newline="\n")
    print(f"Collected notices for {len(packages)} crates and the Rust standard library: {args.output}")


if __name__ == "__main__":
    main()
