#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo 'Usage: bench.sh [--run] -- command [args...]' >&2
  echo 'Replace @LIBRARY@ in a command argument with each generated library path.' >&2
}
run=0
if [[ "${1:-}" == "--run" ]]; then
  run=1
  shift
fi
if [[ "${1:-}" != "--" || $# -lt 2 ]]; then
  usage
  exit 2
fi
shift
cmd=("$@")
if [[ "$run" -ne 1 ]]; then
  echo 'Benchmarks disabled. Pass --run to generate libraries and run the tool.'
  exit 0
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"
bench_root="$(dirname "$repo_root")/bench-libs"
mkdir -p "$bench_root"
printf '%-8s %-10s %s\n' images seconds command
for count in 1000 10000 100000; do
  library="$bench_root/library-$count"
  if [[ ! -f "$library/.expected.json" ]]; then
    python3 "$script_dir/gen-library.py" "$library" --images "$count" --seed 1401 --notes-ratio 0.2 --collections 10 --depth 4
  fi
  expanded=()
  for arg in "${cmd[@]}"; do
    expanded+=("${arg//@LIBRARY@/$library}")
  done
  start=$SECONDS
  "${expanded[@]}"
  elapsed=$((SECONDS - start))
  printf '%-8s %-10s %s\n' "$count" "$elapsed" "${expanded[*]}"
done
