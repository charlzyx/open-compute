#!/usr/bin/env bash
set -euo pipefail
evidence="$PWD/.temp/glibc-ocd/evidence"
for selector in tests::lock_source:: packaged_lock_matches_formal_release_pin; do
  log="$evidence/runtime-${selector//:/_}.log"
  mbx test --locked -p open-compute-runtime --lib "$selector" -- --nocapture 2>&1 | tee "$log"
  python - "$log" <<'PY'
import pathlib, re, sys
output = pathlib.Path(sys.argv[1]).read_text()
if not re.search(r'test result: ok\. [1-9][0-9]* passed; 0 failed;', output):
    raise SystemExit('Selected runtime regressions must execute at least one passing test')
PY
done
