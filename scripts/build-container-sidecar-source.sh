#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
sidecar_source_dir="$repository_root/.temp/container-sidecar-source"
mkdir -p "$sidecar_source_dir"
chmod 0700 "$sidecar_source_dir"

python3 - "$repository_root" "$sidecar_source_dir" <<'PY'
import hashlib
import json
import pathlib
import re
import subprocess
import sys

root, output = map(pathlib.Path, sys.argv[1:])
lock = json.loads((root / "apps/container-sidecar/upstream.lock.json").read_text())
revision = lock["revision"]
if not re.fullmatch(r"[0-9a-f]{40}", revision):
    raise SystemExit("Invalid Container sidecar source revision")
for name, expected in lock["files"].items():
    if name not in {"main.go", "dns.go", "sni.go", "tls.go", "dummyserver.go", "go.mod", "go.sum", "Dockerfile"}:
        raise SystemExit("Invalid Container sidecar source input")
    target = output / name
    subprocess.run(["curl", "-fsSL", "--max-time", "30", "--max-filesize", "2097152",
                    f"https://raw.githubusercontent.com/cloudflare/proxy-everything/{revision}/{name}",
                    "-o", str(target)], check=True)
    if hashlib.sha256(target.read_bytes()).hexdigest() != expected:
        raise SystemExit("Container sidecar source digest mismatch")
PY

cd "$sidecar_source_dir"
patch --batch --forward -p1 < "$repository_root/apps/container-sidecar/upstream.patch"
cp "$repository_root/apps/container-sidecar/"*.go .
go test -count=1 -coverprofile=auth-coverage.out auth.go auth_test.go callback.go callback_test.go
go tool cover -func=auth-coverage.out > auth-coverage.txt
python3 - <<'PY_COVERAGE'
import pathlib
import re
summary = pathlib.Path("auth-coverage.txt").read_text().splitlines()[-1]
match = re.fullmatch(r"total:\s+\(statements\)\s+([0-9.]+)%", summary)
if match is None or float(match[1]) < 90:
    raise SystemExit("Container sidecar authentication coverage below 90%")
PY_COVERAGE
case "$(uname -m)" in
  arm64 | aarch64) sidecar_build_arch=arm64 ;;
  x86_64) sidecar_build_arch=amd64 ;;
  *) echo 'Unsupported Container sidecar build architecture' >&2; exit 1 ;;
esac
if [[ "$(uname -s)" == Linux ]]; then
  go test -count=1 -run '^TestIngress' .
  exec_coverage_dir="$(mktemp -d "$sidecar_source_dir/exec-profiles.XXXXXX")"
  CGO_ENABLED=0 go test -c -cover -covermode=atomic -o exec-tests exec_supervisor*.go exec_http*.go
  GOCOVERDIR="$exec_coverage_dir" "$sidecar_source_dir/exec-tests" -test.count=1 -test.gocoverdir="$exec_coverage_dir"
  root_exec_coverage_dir="$(python3 "$repository_root/test/containers-fixture/exec-helper-capabilities.py" "$sidecar_source_dir/exec-tests" "$exec_coverage_dir")"
  go tool covdata textfmt -i="$exec_coverage_dir,$root_exec_coverage_dir" -o=exec-coverage.out
  go tool cover -func=exec-coverage.out > exec-coverage.txt
  python3 - <<'PY_EXEC_COVERAGE'
import pathlib
import re
summary = pathlib.Path("exec-coverage.txt").read_text().splitlines()[-1]
match = re.fullmatch(r"total:\s+\(statements\)\s+([0-9.]+)%", summary)
if match is None or float(match[1]) < 90:
    raise SystemExit("Container exec helper coverage below 90%")
PY_EXEC_COVERAGE
else
  GOOS=linux GOARCH="$sidecar_build_arch" CGO_ENABLED=0 go test -c -o ingress-tests .
fi
GOOS=linux GOARCH="$sidecar_build_arch" CGO_ENABLED=0 go build -trimpath -o container-sidecar .
