#!/usr/bin/env python3
"""从已登记的版本小节生成发行说明；Build release notes from a recorded version section."""

import argparse
from pathlib import Path
import re


def main():
    """缺失或空版本记录时拒绝发布；Reject missing or empty version entries."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    changelog = Path("CHANGELOG.md").read_text(encoding="utf-8")
    section = re.search(
        rf"^## \[{re.escape(args.version)}\][^\n]*\n(.*?)(?=^## |\Z)",
        changelog,
        re.MULTILINE | re.DOTALL,
    )
    if section is None or not section[1].strip():
        parser.error(f"CHANGELOG.md has no nonempty section for {args.version}")
    if args.output:
        header = Path("packaging/release-notes.md").read_text(encoding="utf-8").strip()
        args.output.write_text(
            f"{header}\n\n## {args.version}\n\n{section[1].strip()}\n",
            encoding="utf-8",
        )
    print(f"Validated release notes for {args.version}")


if __name__ == "__main__":
    main()
