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
# Runtime type generation invokes a native workerd through Miniflare. Record
# the npm dependency's actual startup result, then use only our verified pin.
node --input-type=module <<'JS' | tee "$evidence/npm-workerd-preflight.json"
import {spawnSync} from 'node:child_process';
import {existsSync, realpathSync} from 'node:fs';
import {createRequire} from 'node:module';
import {join, resolve} from 'node:path';
const cf = createRequire(join(realpathSync('node_modules/cf'), 'package.json'));
const candidate = cf.resolve.paths('@cloudflare/runtime-types')
  .map(directory => join(directory, '@cloudflare/runtime-types/package.json'))
  .find(existsSync);
if (!candidate) throw new Error('The locked cf runtime-types package is missing');
const runtime = createRequire(realpathSync(candidate))('workerd');
const result = spawnSync(runtime.default, ['--version'], {encoding:'utf8', timeout:20000});
console.log(JSON.stringify({version:runtime.version, exitCode:result.status,
  signal:result.signal, stdout:result.stdout, stderr:result.stderr,
  error:result.error?.message}));
JS
export MINIFLARE_WORKERD_PATH="$OPEN_COMPUTE_TEST_WORKERD"
"$MINIFLARE_WORKERD_PATH" --version
node --test --test-name-pattern='compatibility build profile' test/release-tools.test.mjs
# cf delegates this fixture to npx; run the same locked Vite directly so a
# native/tooling failure retains its complete output instead of a wrapper exit.
node --version
npm --version
npx --version
(
  cd test/applications/postgres-driver
  npx --no-install vite build --mode production
) 2>&1 | tee "$evidence/postgres-vite.log"
./scripts/package-release.sh --dest "$evidence/ocd-linux-x64-glibc228" \
  --archive "$OPEN_COMPUTE_BUILD_WORKERD_ARCHIVE" | tee "$evidence/package.json"
python test/glibc-compat/check-elf.py "$evidence/ocd-linux-x64-glibc228" > "$evidence/ocd-elf.json"
bun test/conformance/check.ts | tee "$evidence/conformance.json"
bash test/glibc-compat/check-runtime-tests.sh
sha256sum "$evidence/ocd-linux-x64-glibc228" > "$evidence/SHA256SUMS"
