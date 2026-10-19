#!/usr/bin/env bash
set -euo pipefail
root="$PWD"
evidence="$root/.temp/glibc-ocd/evidence"
mkdir -p "$evidence"
test "$(getconf GNU_LIBC_VERSION)" = 'glibc 2.28'
getconf GNU_LIBC_VERSION > "$evidence/build-glibc.txt"
git config --global --add safe.directory "$root"
git config --global --add safe.directory "$root/third_party/workerd"
git config --global --add safe.directory "$root/third_party/gitserver"
export OPEN_COMPUTE_BUILD_PROFILE=linux-x64-glibc228
export CARGO_TARGET_DIR="$root/.temp/glibc-ocd/target"
export CARGO_BUILD_JOBS=4
export MBX_BUILD_SCRIPT_EXECUTION=0 MBX_TARGET_VIEWS=0 MBX_RESTORE_HARDLINK=0 MBX_GC_AUTO=false
export RUSTFLAGS='-L native=/opt/static-cxx'
export TMPDIR=/tmp
mbx fmt --all --check
bun install --frozen-lockfile --ignore-scripts
while IFS= read -r revision; do
  if ! git -C third_party/workerd cat-file -e "${revision}^{commit}" 2>/dev/null; then
    git -C third_party/workerd fetch --depth 1 origin "$revision"
  fi
done < <(python -c 'import json; p=json.load(open("packages/runtime/workerd.lock.json")); print(p["revision"]); print(p["workersTypes"]["gitHead"])')
bun scripts/prepare-workerd.ts --dest "$root/.temp/glibc-ocd/prepared-workerd" \
  --archive "$root/.temp/glibc-ocd/workerd/workerd-linux-64.gz" > "$evidence/workerd.env"
bun scripts/prepare-caddy.ts --dest "$root/.temp/glibc-ocd/prepared-caddy" --download > "$evidence/caddy.env"
set -a
source "$evidence/workerd.env"
source "$evidence/caddy.env"
set +a
python test/glibc-compat/check-elf.py "$OPEN_COMPUTE_TEST_WORKERD" > "$evidence/workerd-elf.json"
python test/glibc-compat/check-elf.py "$OPEN_COMPUTE_BUILD_CADDY" > "$evidence/caddy-elf.json"
node --test --test-name-pattern='compatibility build profile' test/release-tools.test.mjs
./scripts/package-release.sh --dest "$evidence/ocd-linux-x64-glibc228" \
  --archive "$OPEN_COMPUTE_BUILD_WORKERD_ARCHIVE" | tee "$evidence/package.json"
python test/glibc-compat/check-elf.py "$evidence/ocd-linux-x64-glibc228" > "$evidence/ocd-elf.json"
mbx test --locked -p open-compute-runtime --lib lock_source_tests
mbx test --locked -p open-compute-runtime --lib packaged_lock_matches_formal_release_pin
sha256sum "$evidence/ocd-linux-x64-glibc228" > "$evidence/SHA256SUMS"
