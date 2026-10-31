#!/usr/bin/env bash
# Local stand-in for the scoring harness: builds one track offline, runs it N times
# under the board's sandbox flags, and reports wall time, cgroup memory.peak and the
# output sha256 for each run.
#
#   bench/bench.sh <a|b|c|d> [runs] [dataset-volume-subdir]
#   (TRACK_DIR=<folder> TAG=<image> bench alternative builds of a track)
#
# Inputs live in the docker volume `sd-data` (subdir a..d for seed 1, s7-a.. for
# seed 7); they are pre-read before timing so, as on the box, the page cache is warm
# and not charged to the run's cgroup.
set -euo pipefail
track=$1
runs=${2:-5}
sub=${3:-$track}
here=$(cd "$(dirname "$0")/.." && pwd)
dir=${TRACK_DIR:-$here/track-$track}
tag=${TAG:-sd-bench/track-$track}
declare -A OUT=([a]=summary.txt [b]=results.txt [c]=aggregates.txt [d]=distances.txt)
declare -A EXPECT=(
  [a]=495ca09682b9673ef836a506496177680bb4a3af54255174befddb1f83481cd3
  [b]=f86973c210228141bcb6f1f1b10188ad2a9b65f71e7cd7cf4590b38de4e80927
  [c]=d8088b1cd42ba1338b1345b107b55db7b96ef1f5066f3ff64fd26a8912a0f536
  [d]=a23dbdf0a3e49cc8901310a9ee1dc6a9a569a85297b6da75c2a7002cb2ed277e
)
expect=${EXPECT_SHA:-${EXPECT[$track]}}
DEB=debian:13-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad

docker build -q --network=none -t "$tag" "$dir" >/dev/null
cmd=$(docker image inspect -f '{{range .Config.Entrypoint}}{{.}} {{end}}{{range .Config.Cmd}}{{.}} {{end}}' "$tag")
size=$(docker image inspect -f '{{.Size}}' "$tag")
echo "image $tag $((size / 1000000)) MB, cmd: $cmd"

export MSYS_NO_PATHCONV=1
docker run --rm -v sd-data:/d:ro "$DEB" sh -c "cat /d/$sub/* > /dev/null"

for i in $(seq 1 "$runs"); do
  docker run --rm -v sd-out-$track:/out "$DEB" sh -c 'rm -rf /out/* && chmod 777 /out'
  docker run --rm --network=none --read-only --tmpfs /tmp:rw,size=1g,exec \
    --memory=8g --memory-swap=8g --pids-limit=512 --cpuset-cpus=0-11 \
    --cap-drop=ALL --security-opt=no-new-privileges --user 65534:65534 --ulimit nofile=8192:8192 \
    -e TRACK="$track" \
    --mount "type=volume,src=sd-data,dst=/data,volume-subpath=$sub,readonly" \
    -v sd-out-$track:/out --entrypoint /bin/sh "$tag" -c "
      s=\$(date +%s%N); $cmd; rc=\$?; e=\$(date +%s%N)
      peak=\$(cat /sys/fs/cgroup/memory.peak)
      files=\$(ls /out | tr '\n' ' ')
      sum=\$(sha256sum /out/${OUT[$track]} 2>/dev/null | cut -c1-64)
      ok=FAIL; [ \"\$sum\" = \"$expect\" ] && ok=PASS
      echo \"run $i rc=\$rc T=\$(( (e - s) / 1000000 )).\$(( (e - s) / 10000 % 100 ))ms M=\$((peak / 1024))KiB out=[\$files] \$ok\""
done
