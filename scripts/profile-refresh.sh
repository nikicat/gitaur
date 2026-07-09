#!/usr/bin/env bash
# Record a samply CPU profile of `aurox <args>` and emit a flat self/total
# time report. Profile is saved as profile.json.gz alongside a syms sidecar;
# load it later with `samply load profile.json.gz`.
#
# Usage: scripts/profile-refresh.sh [-o OUT] [-- <aurox args>]
# Defaults to `aurox -Sy` and ./profile.json.gz.
set -euo pipefail

out=profile.json.gz
args=(-Sy)
while [[ $# -gt 0 ]]; do
  case "$1" in
    -o) out=$2; shift 2 ;;
    --) shift; args=("$@"); break ;;
    *)  args=("$@"); break ;;
  esac
done

command -v samply >/dev/null || { echo "samply not installed: cargo install samply" >&2; exit 1; }

paranoid=$(cat /proc/sys/kernel/perf_event_paranoid)
mlock=$(cat /proc/sys/kernel/perf_event_mlock_kb)
if (( paranoid > 1 )) || (( mlock < 2048 )); then
  echo "perf sysctls insufficient (paranoid=$paranoid mlock_kb=$mlock); run:" >&2
  echo "  sudo sh -c 'echo 1 > /proc/sys/kernel/perf_event_paranoid && echo 2048 > /proc/sys/kernel/perf_event_mlock_kb'" >&2
  exit 1
fi

bin=$(cargo build --release --message-format=json 2>/dev/null \
  | python3 -c 'import json,sys
for l in sys.stdin:
  try: m=json.loads(l)
  except: continue
  if m.get("reason")=="compiler-artifact" and m.get("target",{}).get("name")=="aurox" and m.get("executable"):
    print(m["executable"]); break')
[[ -x "$bin" ]] || { echo "could not locate aurox binary" >&2; exit 1; }

echo "binary: $bin" >&2
echo "args:   ${args[*]}" >&2
samply record --unstable-presymbolicate -s -o "$out" "$bin" "${args[@]}"

echo
echo "=== flat self/total time ==="
exec python3 "$(dirname "$0")/symbolize-profile.py" "$out" "$bin"
