#!/usr/bin/env bash
#
# h2spec conformance harness for the native HTTP/2 backend.
#
# Builds and starts `examples/h2spec_server.rs`, waits until it accepts
# connections on 127.0.0.1:8080, runs h2spec with `--strict`, writes a JUnit
# report, and propagates h2spec's exit code so CI can gate on it.
#
# Env overrides: H2SPEC_HOST, H2SPEC_PORT, H2SPEC_SPEC, H2SPEC_JUNIT,
# H2SPEC_BIN (path to a prebuilt server binary).

set -u
set -o pipefail

HOST="${H2SPEC_HOST:-127.0.0.1}"
PORT="${H2SPEC_PORT:-8080}"
SPEC="${H2SPEC_SPEC:-http2 hpack generic}"
JUNIT="${H2SPEC_JUNIT:-target/h2spec-report.xml}"
BIN="${H2SPEC_BIN:-target/debug/examples/h2spec_server}"

here="$(cd "$(dirname "$0")/.." && pwd)"
cd "$here"

if [ ! -x "$BIN" ]; then
    echo "==> building h2spec_server example"
    cargo build --example h2spec_server --features h2
fi

echo "==> starting h2spec_server on ${HOST}:${PORT}"
"$BIN" &
SERVER_PID=$!

cleanup() {
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
}
trap cleanup EXIT

echo "==> waiting for server readiness on ${HOST}:${PORT}"
ready=0
for _ in $(seq 1 100); do
    if (exec 3<>"/dev/tcp/${HOST}/${PORT}") 2>/dev/null; then
        exec 3>&- 3<&-
        ready=1
        break
    fi
    sleep 0.1
done
if [ "$ready" -ne 1 ]; then
    echo "error: h2spec_server did not become ready" >&2
    exit 1
fi

mkdir -p "$(dirname "$JUNIT")"
echo "==> running h2spec --strict ${SPEC}"
if [ -x ./h2spec/h2spec ]; then
  ./h2spec/h2spec -h "$HOST" -p "$PORT" -S -j "$JUNIT" $SPEC
else
  ./h2spec -h "$HOST" -p "$PORT" -S -j "$JUNIT" $SPEC
fi
status=$?
echo "==> h2spec exited with code ${status} (report: ${JUNIT})"
exit "$status"
