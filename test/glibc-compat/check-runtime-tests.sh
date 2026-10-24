#!/usr/bin/env bash
set -euo pipefail
evidence="$PWD/.temp/glibc-ocd/evidence"
for crate in runtime storage; do
  log="$evidence/$crate-lib.log"
  mbx test --locked -p "open-compute-$crate" --lib -- --test-threads=1 --nocapture 2>&1 | tee "$log"
  python - "$log" <<'PYTHON'
import pathlib, re, sys
output = pathlib.Path(sys.argv[1]).read_text()
if not re.search(r'test result: ok\. [1-9][0-9]* passed; 0 failed; 0 ignored;', output):
    raise SystemExit('The complete library suite must pass without ignored tests')
PYTHON
done
