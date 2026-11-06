#!/bin/bash
# Measures every store for the README, writing the logs that report.py reads.
#
# Usage: measure.sh mem|whole <pbf_eval binary> <out dir>
#   mem:   build, verify and bench of each store with the store in memory, twice, alternating
#          stores: the small extract, and the large one's nodes up to ID $MAX_ID
#          -> <out>/mem/<region>-<store>-<run>.txt
#   whole: load of the whole large extract and the planet, for their sizes and load times, and
#          resolving the planet's ways with a store larger than the free memory
#          -> <out>/whole/<region>-<store>.txt
#
# The files, and the stores the whole phase measures, come from the environment:
#   SMALL, SMALL_NAME     a small extract (default: District of Columbia)
#   LARGE, LARGE_NAME     a large extract, such as a continent
#   PLANET                the planet
#   MAX_ID                the large extract's nodes in the mem phase go up to this ID (3000000000)
#   MEM_STORES            the stores of the mem phase ("cache redb dense mem")
#   WHOLE_STORES          the stores of the whole phase ("cache redb dense"); a dense store takes
#                         8 bytes times the highest node ID, about 110 GB
#   WHOLE_RESOLVE         the stores whose ways the whole phase resolves on the planet ("cache")
#   WHOLE_FLAGS           pbf_eval flags of the whole phase ("--direct --preload": read the PBF file
#                         with direct I/O, and the store into memory before resolving)
set -euo pipefail
PHASE=$1
B=$2
OUT=$3
: "${SMALL:?}" "${LARGE:?}"
SMALL_NAME=${SMALL_NAME:-District of Columbia}
LARGE_NAME=${LARGE_NAME:-Large extract}
MAX_ID=${MAX_ID:-3000000000}
MEM_STORES=${MEM_STORES:-cache redb dense mem}
WHOLE_STORES=${WHOLE_STORES:-cache redb dense}
WHOLE_RESOLVE=${WHOLE_RESOLVE:-cache}
WHOLE_FLAGS=${WHOLE_FLAGS---direct --preload}
mkdir -p "$OUT/mem" "$OUT/whole"
# what report.py needs to name the regions
printf 'small=%s\nlarge=%s\nslice=%s\n' "$SMALL_NAME" "$LARGE_NAME" "$MAX_ID" > "$OUT/regions.txt"

t() { echo "== $*"; /usr/bin/time -f "  wall %e s, peak RSS %M KB, %F major faults" "$@" 2>&1; }
# the bytes the store's files take on disk, and their sizes; a store has one to three files, and a
# memory store none
disk() {
  local files=()
  for f in "$1" "$1.dir" "$1.undo"; do if [ -e "$f" ]; then files+=("$f"); fi; done
  if [ ${#files[@]} -eq 0 ]; then return; fi
  echo "== disk"; du -cB1 "${files[@]}" | tail -1
  echo "== apparent"; du -cB1 --apparent-size "${files[@]}" | tail -1
}
clean() { rm -f "$1" "$1.dir" "$1.undo"; }
# drops a store's files from the page cache, so that resolving starts with none of it in memory
uncache() {
  for f in "$1" "$1.dir"; do
    if [ -e "$f" ]; then python3 -c 'import os,sys; fd=os.open(sys.argv[1], os.O_RDONLY); os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)' "$f"; fi
  done
}

if [ "$PHASE" = mem ]; then
  for run in 1 2; do
    for spec in "small $SMALL 1 68719476735 cache redb dense mem" "slice $LARGE 10 $MAX_ID cache redb dense mem"; do
      set -- $spec; region=$1; pbf=$2; every=$3; max=$4; shift 4
      for store in "$@"; do
        if [[ " $MEM_STORES " != *" $store "* ]]; then continue; fi
        S=$OUT/mem/$region.$store
        clean "$S"
        {
          free -g | head -2
          t "$B" --max-id "$max" build "$pbf" "$S" 0.97
          disk "$S"
          t "$B" --max-id "$max" verify "$pbf" "$S"
          t "$B" --max-id "$max" bench "$pbf" "$S" "$every" 300000 2000
          echo DONE
        } > "$OUT/mem/$region-$store-$run.txt" 2>&1
        clean "$S"
      done
    done
  done
elif [ "$PHASE" = whole ]; then
  : "${PLANET:?}"
  for spec in "large $LARGE" "planet $PLANET"; do
    set -- $spec; region=$1; pbf=$2
    for store in $WHOLE_STORES; do
      S=$OUT/whole/$region.$store
      clean "$S"
      {
        free -g | head -2
        t "$B" $WHOLE_FLAGS load "$pbf" "$S" 0.97
        disk "$S"
        if [ "$region" = planet ] && [[ " $WHOLE_RESOLVE " == *" $store "* ]]; then
          uncache "$S"
          t "$B" $WHOLE_FLAGS resolve "$pbf" "$S"
        fi
        echo DONE
      } > "$OUT/whole/$region-$store.txt" 2>&1
      clean "$S"
    done
  done
else
  echo "the phase is mem or whole" >&2
  exit 1
fi
echo ALL DONE
