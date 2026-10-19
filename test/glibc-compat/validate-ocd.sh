#!/usr/bin/env bash
set -euo pipefail
root="$PWD"
evidence="$root/.temp/glibc-ocd/evidence"
git config --global --add safe.directory "$root"
git config --global --add safe.directory "$root/third_party/workerd"
git config --global --add safe.directory "$root/third_party/gitserver"
test -f third_party/gitserver/crates/gitserver-core/Cargo.toml
export OPEN_COMPUTE_BUILD_PROFILE=linux-x64-glibc228
export CARGO_TARGET_DIR="$root/.temp/glibc-ocd/target" CARGO_BUILD_JOBS=4
export MBX_BUILD_SCRIPT_EXECUTION=0 MBX_TARGET_VIEWS=0 MBX_RESTORE_HARDLINK=0 MBX_GC_AUTO=false
export RUSTFLAGS='-L native=/opt/static-cxx' TMPDIR=/tmp
mbx fmt --all --check
bun install --frozen-lockfile --ignore-scripts
revision="$(python -c 'import json; print(json.load(open("packages/runtime/workerd.lock.json"))["workersTypes"]["gitHead"])')"
git -C third_party/workerd fetch --depth 1 origin "$revision"
bun scripts/prepare-workerd.ts --dest "$root/.temp/glibc-ocd/prepared-workerd" \
  --archive "$root/.temp/glibc-ocd/workerd/workerd-linux-64.gz" > "$evidence/workerd.env"
bun scripts/prepare-caddy.ts --dest "$root/.temp/glibc-ocd/prepared-caddy" --download > "$evidence/caddy.env"
set -a
source "$evidence/workerd.env"
source "$evidence/caddy.env"
set +a
export MINIFLARE_WORKERD_PATH="$OPEN_COMPUTE_TEST_WORKERD"
bun scripts/build-workerd.ts
bun run --filter @open-compute/sdk typecheck
bun run --filter @open-compute/sdk build
bun run --filter @open-compute/runtime build
bun run check:generated
producer="$(python -c 'import json; print(json.load(open("test/glibc-compat/ocd-artifact.json"))["producerCommit"])')"
bun scripts/verify-release-executable.ts "$evidence/ocd-linux-x64-glibc228" "$producer"
python test/glibc-compat/check-elf.py "$evidence/ocd-linux-x64-glibc228" > "$evidence/ocd-elf.json"
bash test/glibc-compat/check-runtime-tests.sh
