#!/bin/sh
set -eu
directory=/tmp/open-compute-fixture-www
mkdir -p "$directory"
printf '%s\n' "$IMAGE_REVISION" > "$directory/index.html"
exec python3 -m http.server 8080 --bind 0.0.0.0 --directory "$directory"
