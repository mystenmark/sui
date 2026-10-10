#!/usr/bin/env python3
# Copyright (c) Mysten Labs, Inc.
# SPDX-License-Identifier: Apache-2.0

"""Summarizes an Instruments Time Profiler trace: the functions with the most samples, by self
time (the sample's top frame) and inclusive time (anywhere on the stack, once per sample).

    xctrace record --template 'Time Profiler' --output run.trace --launch -- <binary> <args>
    scripts/profile_summary.py run.trace [--top N] [--thread SUBSTRING] [--under FUNCTION]

`--under` keeps only samples with FUNCTION on the stack, to look inside one subsystem.
"""

import argparse
import collections
import re
import subprocess
import sys
import xml.etree.ElementTree as ET


def export(trace):
    toc = subprocess.run(
        ["xctrace", "export", "--input", trace, "--toc"], capture_output=True, text=True, check=True
    ).stdout
    schema = "time-profile" if 'schema="time-profile"' in toc else "time-sample"
    xpath = f'/trace-toc/run[@number="1"]/data/table[@schema="{schema}"]'
    return subprocess.run(
        ["xctrace", "export", "--input", trace, "--xpath", xpath],
        capture_output=True,
        check=True,
    ).stdout


def short(name):
    # Drop generic arguments and hashes so instantiations of one function count together.
    name = re.sub(r"::h[0-9a-f]{16}$", "", name)
    out, depth = [], 0
    for c in name:
        if c == "<":
            depth += 1
            if depth == 1:
                out.append("<..>")
        elif c == ">":
            depth -= 1
        elif depth == 0:
            out.append(c)
    return "".join(out)


def main():
    p = argparse.ArgumentParser()
    p.add_argument("trace")
    p.add_argument("--top", type=int, default=40)
    p.add_argument("--thread", default=None)
    p.add_argument("--under", default=None)
    p.add_argument(
        "--callers",
        default=None,
        help="for samples with FUNCTION on the stack, count the nearest frames above it from "
        "crates other than std, core and the system",
    )
    args = p.parse_args()

    root = ET.fromstring(export(args.trace))
    # Elements are interned: later rows refer to earlier ones by `ref`.
    by_id = {}

    def resolve(e):
        if e is None:
            return None
        if "ref" in e.attrib:
            return by_id[e.attrib["ref"]]
        if "id" in e.attrib:
            by_id[e.attrib["id"]] = e
        for child in e:
            resolve(child)
        return e

    self_time = collections.Counter()
    inclusive = collections.Counter()
    total = 0
    for row in root.iter("row"):
        row = resolve(row)
        thread = resolve(row.find("thread"))
        if args.thread and (thread is None or args.thread not in thread.attrib.get("fmt", "")):
            continue
        bt = resolve(row.find("backtrace"))
        if bt is None:
            continue
        frames = []
        for f in bt.iter("frame"):
            f = resolve(f)
            frames.append(short(f.attrib.get("name", "?")))
        if not frames:
            continue
        if args.under and not any(args.under in f for f in frames):
            continue
        if args.callers:
            idx = next((i for i, f in enumerate(frames) if args.callers in f), None)
            if idx is None:
                continue
            caller = next(
                (f for f in frames[idx + 1 :] if not re.match(r"^(_?R|std|core|alloc|_|<..>|clock|mach)", f)),
                "?",
            )
            total += 1
            self_time[caller] += 1
            continue
        total += 1
        self_time[frames[0]] += 1
        for f in set(frames):
            inclusive[f] += 1

    if total == 0:
        sys.exit("no samples")
    print(f"{total} samples")
    print("\n== callers ==" if args.callers else "\n== self ==")
    for name, n in self_time.most_common(args.top):
        print(f"{100 * n / total:6.2f}%  {name}")
    if args.callers:
        return
    print("\n== inclusive ==")
    for name, n in inclusive.most_common(args.top):
        print(f"{100 * n / total:6.2f}%  {name}")


if __name__ == "__main__":
    main()
