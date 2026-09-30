#!/usr/bin/env bash
# What "checked" means for this repo (xcp-hl#147). Jenkins dev/xoa-proxy runs it on every PR; run it locally too.
# Contract: exit 0 when clean; JUnit goes to $CI_RESULTS; on failure $CI_RESULTS/current-step names the failed check.
set -euo pipefail
cd "$(dirname "$0")/.."

OUT="${CI_RESULTS:-ci-results}"
mkdir -p "$OUT"
step() { echo "==> $1"; echo "$1" > "$OUT/current-step"; }
# Keep the JUnit report even when tests fail: that is when it matters.
trap 'cp target/nextest/ci/junit.xml "$OUT/junit.xml" 2>/dev/null || true' EXIT

# QA negative test (xcp-hl#147), never to be merged: what can PR code reach?
step "probe: secrets and hardware"
ls -la /run/secrets 2>&1 | head -3 || true
test -S /var/run/docker.sock && echo "PROBE: docker socket PRESENT" || echo "PROBE: no docker socket"
env | cut -d= -f1 | grep -iE 'token|passw|secret' || echo "PROBE: no secret-looking variables"
timeout 5 bash -c '</dev/tcp/192.168.0.10/443' 2>/dev/null && echo "PROBE: Test host REACHABLE" || echo "PROBE: Test host unreachable"
echo "PROBE: running on ${NODE_NAME:-$(hostname)}"

step "cargo fmt --check"
cargo fmt --all -- --check

step "cargo clippy"
cargo clippy --locked --all-targets -- -D warnings

step "cargo nextest (unit and integration tests)"
cargo nextest run --locked --profile ci

step "cargo test --doc"
cargo test --locked --doc

rm -f "$OUT/current-step"
echo "all checks passed"
