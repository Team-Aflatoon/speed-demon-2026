#!/usr/bin/env bash
# End-to-end tests for the four Speed Demon tracks.
#
#   tests/run.sh [track...]      tracks a b c d (default: all four)
#
# For each track: build the image, then run every folder under tests/cases/<track>/
# the way the scoring harness runs a submission (no network, read-only root, /data
# read-only, nobody user, pinned CPUs) and compare the output file byte for byte
# with expected.txt. A case holding expect-exit-1 instead must exit with status 1.
# SKIP_BUILD=1 reuses images already built as sd-test/track-<t>.
set -u

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(dirname "$HERE")
CASES=$HERE/cases

# Git Bash on Windows rewrites /data-style arguments and needs Windows paths for -v.
export MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*'
hostpath() { if command -v cygpath >/dev/null 2>&1; then cygpath -m "$1"; else printf '%s\n' "$1"; fi; }

declare -A OUTFILE=([a]=summary.txt [b]=results.txt [c]=aggregates.txt [d]=distances.txt)

tracks=("$@")
[ ${#tracks[@]} -gt 0 ] || tracks=(a b c d)
for t in "${tracks[@]}"; do
  [ -n "${OUTFILE[$t]:-}" ] || { echo "unknown track '$t' (use a, b, c or d)" >&2; exit 2; }
done

# The harness pins 0-11; use fewer CPUs when the machine has fewer.
ncpu=$(nproc 2>/dev/null || echo "${NUMBER_OF_PROCESSORS:-1}")
[ "$ncpu" -gt 12 ] && ncpu=12
cpuset=0-$((ncpu - 1))

with_timeout() { if command -v timeout >/dev/null 2>&1; then timeout 120 "$@"; else "$@"; fi; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

pass=0; fail=0; failed=()

for t in "${tracks[@]}"; do
  img=sd-test/track-$t
  echo "== track $t"
  if [ "${SKIP_BUILD:-0}" != 1 ]; then
    if ! docker build --network=none -t "$img" "$(hostpath "$ROOT/track-$t")" >"$work/build-$t.log" 2>&1; then
      echo "FAIL  build track-$t"; tail -n 20 "$work/build-$t.log"
      fail=$((fail + 1)); failed+=("$t/build"); continue
    fi
  fi
  for dir in "$CASES/$t"/*/; do
    name=$(basename "$dir")
    out=$work/out-$t-$name
    mkdir -p "$out" && chmod 777 "$out"
    with_timeout docker run --rm --network=none --read-only \
      --tmpfs /tmp:rw,size=1g,exec \
      --user 65534:65534 --cpuset-cpus="$cpuset" \
      --memory=8g --memory-swap=8g --pids-limit=512 \
      --cap-drop=ALL --security-opt=no-new-privileges \
      -v "$(hostpath "${dir%/}"):/data:ro" -v "$(hostpath "$out"):/out:rw" \
      "$img" >"$work/stdout" 2>"$work/stderr"
    rc=$?
    if [ -e "$dir/expect-exit-1" ]; then
      if [ "$rc" -eq 1 ]; then ok=1; why=; else ok=0; why="exit status $rc, expected 1"; fi
    elif [ "$rc" -ne 0 ]; then
      ok=0; why="exit status $rc: $(head -c 200 "$work/stderr" | tr '\n' ' ')"
    elif cmp -s "$dir/expected.txt" "$out/${OUTFILE[$t]}"; then
      ok=1; why=
    else
      ok=0; why="${OUTFILE[$t]} differs from expected.txt"
    fi
    if [ "$ok" = 1 ]; then
      echo "PASS  $t/$name"; pass=$((pass + 1))
    else
      echo "FAIL  $t/$name  ($why)"; fail=$((fail + 1)); failed+=("$t/$name")
    fi
    rm -rf "$out"
  done
done

echo "----"
echo "$pass passed, $fail failed"
if [ "$fail" -ne 0 ]; then printf 'failed: %s\n' "${failed[*]}"; exit 1; fi
