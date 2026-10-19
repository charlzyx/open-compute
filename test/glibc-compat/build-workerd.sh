#!/usr/bin/env bash
set -euo pipefail

root="$PWD"
allocator="${1:-tcmalloc}"
allocator_options=()
case "$allocator" in
  tcmalloc) evidence="$root/.temp/glibc-compat/evidence" ;;
  system)
    evidence="$root/.temp/glibc-compat-system-malloc/evidence"
    allocator_options=(--//src/workerd/server:use_tcmalloc=false)
    ;;
  *) echo 'Unsupported diagnostic allocator selection' >&2; exit 2 ;;
esac
mkdir -p "$evidence"
printf '%s\n' "$allocator" > "$evidence/allocator.txt"
printf '%s\n' "${allocator_options[@]}" > "$evidence/allocator-bazel-option.txt"
uname -sr > "$evidence/build-kernel.txt"
sha256sum test/glibc-compat/Dockerfile > "$evidence/builder.sha256"
test "$(getconf GNU_LIBC_VERSION)" = 'glibc 2.28'
getconf GNU_LIBC_VERSION > "$evidence/build-glibc.txt"
clang --version > "$evidence/clang.txt"
bazel --version > "$evidence/bazel.txt"
git config --global --add safe.directory "$root"
git config --global --add safe.directory "$root/third_party/workerd"
git rev-parse HEAD > "$evidence/producer-commit.txt"
revision="$(python -c 'import json; print(json.load(open("packages/runtime/workerd.lock.json"))["revision"])')"
test "$(git -C third_party/workerd rev-parse HEAD)" = "$revision"
test -z "$(git -C third_party/workerd status --porcelain)"
printf '%s\n' "$revision" > "$evidence/source-revision.txt"
cd third_party/workerd
mkdir -p .local
cat > .local/bazelrc <<'CONFIG'
build:linux --repo_env=CC=/opt/llvm/bin/clang
build:linux --repo_env=AR=/opt/llvm/bin/llvm-ar
build:linux --copt=--gcc-toolchain=/opt/rh/gcc-toolset-14/root/usr
build:linux --host_copt=--gcc-toolchain=/opt/rh/gcc-toolset-14/root/usr
build:linux --cxxopt=-isystem/opt/llvm/include/c++/v1
build:linux --host_cxxopt=-isystem/opt/llvm/include/c++/v1
build:linux --linkopt=-L/opt/llvm/lib
build:linux --host_linkopt=-L/opt/llvm/lib
build:linux --linkopt=--ld-path=/opt/llvm/bin/ld.lld
build:linux --host_linkopt=--ld-path=/opt/llvm/bin/ld.lld
CONFIG
cp .local/bazelrc "$evidence/compat.bazelrc"
if [ "$allocator" = system ]; then
  bazel --output_user_root=/var/tmp/open-compute-glibc228/bazel cquery \
    'deps(//src/workerd/server:workerd)' --config=release_linux --strip=always \
    --incompatible_enable_proto_toolchain_resolution=false \
    --@rules_rust//:extra_exec_rustc_flags=-Cstrip=none \
    "${allocator_options[@]}" --output=label > "$evidence/configured-dependencies.txt"
  python - "$evidence/configured-dependencies.txt" <<'PYQUERY'
from pathlib import Path
import sys
labels = [line.split()[0] for line in Path(sys.argv[1]).read_text().splitlines() if line.strip()]
if any(label.partition('//')[2] == 'tcmalloc:tcmalloc' for label in labels):
    raise SystemExit('System malloc diagnostic must not depend on the tcmalloc allocator target')
PYQUERY
fi
bazel --output_user_root=/var/tmp/open-compute-glibc228/bazel build \
  //src/workerd/server:workerd --config=release_linux --strip=always \
  --disk_cache=/var/tmp/open-compute-glibc228/action-cache \
  --repository_cache=/var/tmp/open-compute-glibc228/repository-cache \
  --experimental_disk_cache_gc_max_size=6G \
  --incompatible_enable_proto_toolchain_resolution=false \
  --@rules_rust//:extra_exec_rustc_flags=-Cstrip=none \
  "${allocator_options[@]}" \
  --jobs=4 --local_resources=memory=10240 2>&1 | tee "$evidence/build.log"
cp bazel-bin/src/workerd/server/workerd "$evidence/workerd"
cd "$root"
python test/glibc-compat/check-elf.py "$evidence/workerd" > "$evidence/elf.json"
if [ "$allocator" = system ]; then
  python test/glibc-compat/check-allocator.py "$evidence/workerd" > "$evidence/allocator-symbols.json"
fi
"$evidence/workerd" --version > "$evidence/version.txt"
test "$(cat "$evidence/version.txt")" = 'workerd 2026-09-30'
"$evidence/workerd" container-protocol-version > "$evidence/protocol.txt"
test "$(cat "$evidence/protocol.txt")" = 'open-compute-container-protocol-v3'
"$evidence/workerd" compatibility-catalog > "$evidence/catalog.json"
python - "$evidence/catalog.json" <<'PY'
import hashlib, json, pathlib, sys
lock = json.loads(pathlib.Path('packages/runtime/workerd.lock.json').read_text())
assert hashlib.sha256(pathlib.Path(sys.argv[1]).read_bytes()).hexdigest() == lock['compatibilityCatalog']['sha256']
PY
"$evidence/workerd" serve test/glibc-compat/worker.capnp --experimental > "$evidence/worker.stdout" 2> "$evidence/worker.stderr" &
worker_pid=$!
trap 'kill "$worker_pid" 2>/dev/null || true; wait "$worker_pid" 2>/dev/null || true' EXIT
for attempt in $(seq 1 100); do
  if curl -fsS http://127.0.0.1:8080 > "$evidence/worker-response.txt" 2>/dev/null; then
    test "$(cat "$evidence/worker-response.txt")" = 'glibc228-worker-ok'
    gzip -9n -c "$evidence/workerd" > "$evidence/workerd-linux-64.gz"
    sha256sum "$evidence/workerd" "$evidence/workerd-linux-64.gz" > "$evidence/SHA256SUMS"
    exit 0
  fi
  kill -0 "$worker_pid"
  sleep 0.1
done
cat "$evidence/worker.stderr" >&2
exit 1
