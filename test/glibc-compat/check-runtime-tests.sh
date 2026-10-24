#!/usr/bin/env bash
set -euo pipefail
evidence="$PWD/.temp/glibc-ocd/evidence"
mode="${1:?Select build or run}"
case "$mode" in
  build) test "$(getconf GNU_LIBC_VERSION)" = 'glibc 2.28' ;;
  run)
    test "$(id -u)" != 0
    test -n "${OPEN_COMPUTE_TEST_BROWSER:?Prepare the pinned browser fixture}"
    set -a
    source "$evidence/workerd.env"
    source "$evidence/caddy.env"
    set +a
    getconf GNU_LIBC_VERSION > "$evidence/library-test-host-glibc.txt"
    ;;
  *) echo 'Select build or run' >&2; exit 1 ;;
esac
for crate in runtime storage; do
  manifest="$evidence/$crate-test-executable.txt"
  if [ "$mode" = build ]; then
    mbx test --locked -p "open-compute-$crate" --lib --no-run --message-format=json \
      > "$evidence/$crate-test-build.jsonl"
    python - "$evidence/$crate-test-build.jsonl" "$crate" > "$manifest" <<'PYTHON'
import json, os, pathlib, sys
artifacts = [json.loads(line) for line in pathlib.Path(sys.argv[1]).read_text().splitlines()]
executables = {item['executable'] for item in artifacts
               if item.get('reason') == 'compiler-artifact'
               and item.get('profile', {}).get('test') is True
               and item.get('target', {}).get('kind') == ['lib']
               and item['target']['name'] == 'open_compute_' + sys.argv[2]
               and item.get('executable')}
if len(executables) != 1:
    raise SystemExit('Expected exactly one complete library test executable')
binary = pathlib.Path(executables.pop()).resolve(strict=True)
binary.relative_to(pathlib.Path(os.environ['CARGO_TARGET_DIR']).resolve(strict=True))
print(binary)
PYTHON
    binary="$(cat "$manifest")"
    python test/glibc-compat/check-elf.py "$binary" > "$evidence/$crate-test-elf.json"
    sha256sum "$binary" > "$evidence/$crate-test-sha256.txt"
    continue
  fi
  sha256sum --check "$evidence/$crate-test-sha256.txt"
  binary="$(cat "$manifest")"
  log="$evidence/$crate-lib.log"
  "$binary" --test-threads=1 --nocapture 2>&1 | tee "$log"
  python3 - "$log" <<'PYTHON'
import pathlib, re, sys
output = pathlib.Path(sys.argv[1]).read_text()
if not re.search(r'test result: ok\. [1-9][0-9]* passed; 0 failed; 0 ignored;', output):
    raise SystemExit('The complete library suite must pass without ignored tests')
PYTHON
done
