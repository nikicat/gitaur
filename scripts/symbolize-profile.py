#!/usr/bin/env python3
"""Aggregate a samply profile into flat self/total time tables.

samply's --unstable-presymbolicate only resolves dynamic symbols; release Rust
binaries keep most names in .symtab. We symbolize the leftover "0x<rva>" frame
names against the binary's `nm -C` output.

Usage: symbolize-profile.py PROFILE.json.gz BINARY
"""
import bisect
import collections
import gzip
import json
import subprocess
import sys


def load_nm(binary):
    out = subprocess.run(["nm", "-C", "--defined-only", binary],
                         capture_output=True, text=True, check=True).stdout
    syms = []
    for line in out.splitlines():
        parts = line.split(maxsplit=2)
        if len(parts) < 3:
            continue
        addr_hex, typ, name = parts
        try:
            addr = int(addr_hex, 16)
        except ValueError:
            continue
        if typ.lower() in ("t", "w"):
            syms.append((addr, name))
    syms.sort()
    return [a for a, _ in syms], [n for _, n in syms]


def main(profile_path, binary):
    addrs, names = load_nm(binary)

    def lookup(rva):
        i = bisect.bisect_right(addrs, rva) - 1
        return names[i] if i >= 0 else None

    with gzip.open(profile_path, "rt") as f:
        data = json.load(f)
    interval = data.get("meta", {}).get("interval", 1.0)

    self_ms = collections.Counter()
    total_ms = collections.Counter()
    for thread in data["threads"]:
        sa = thread["stringArray"]
        funcs = thread["funcTable"]
        frames = thread["frameTable"]
        stacks = thread["stackTable"]
        samples = thread["samples"]
        resolved = []
        for fn_idx in funcs["name"]:
            name = sa[fn_idx]
            if name.startswith("0x"):
                try:
                    name = lookup(int(name, 16)) or f"[+{name[2:]}]"
                except ValueError:
                    pass
            resolved.append(name)
        weights = samples.get("weight") or [1] * len(samples["stack"])
        for si, w in zip(samples["stack"], weights):
            if si is None:
                continue
            seen = set()
            leaf = True
            cur = si
            while cur is not None:
                name = resolved[frames["func"][stacks["frame"][cur]]]
                if leaf:
                    self_ms[name] += w * interval
                    leaf = False
                seen.add(name)
                cur = stacks["prefix"][cur]
            for n in seen:
                total_ms[n] += w * interval

    total = sum(self_ms.values())
    print(f"sampled time: {total:.0f} ms")
    print(f"\n=== TOP 25 SELF time (where CPU burns) ===")
    for fn, ms in self_ms.most_common(25):
        print(f"  {ms:7.0f} ms  {100*ms/total:5.1f}%  {fn[:110]}")
    print(f"\n=== TOP 20 TOTAL time (any frame on stack) ===")
    for fn, ms in total_ms.most_common(20):
        print(f"  {ms:7.0f} ms  {100*ms/total:5.1f}%  {fn[:110]}")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        print(__doc__, file=sys.stderr)
        sys.exit(2)
    main(sys.argv[1], sys.argv[2])
