#!/usr/bin/env bash
#
# ci_trust_gates_ci.sh — T03 trust-gate verification (CI 假成功链路封堵)
#
# ASCII-only messages: bash 3.2 glues bytes >=0x80 onto $var names inside
# double quotes. No LLM, no ion host; real (tiny) cargo for shim honesty.
#
#   Group A  workflow integrity (static)
#     A1 ci.yml has zero `continue-on-error:` directives
#     A2 pr-gate.yml `cargo test --tests` is bare (no `||` swallow)
#     A3 ci.yml artifact uploads follow the run-root pointer (no stale path)
#     A4 parallel runner contains no fabricated cargo output ("900 passed")
#   Group B  honest preflight + stamp + shim (fake git+cargo project)
#     B1 mid-run source break -> cargo check really runs and FAILS -> whole
#        matrix exits non-zero (fault propagates)
#     B2 preflight.log holds real cargo output
#     B3 cargo-stamp exists with tree fingerprint
#     B4 per-script verdicts: 3 pass / injector fail / summary FAIL
#     B5 `cargo test` output is REAL (1 passing test), never "900 passed"
#     B6 /tmp/ci-matrix-latest points at this run root
#     B7 source .ion/monitors untouched
#     B8 clean rerun exits 0
#     B9 stamp-cache no-op is used AND logged ("validated by preflight")
#   Group C  old runners (serial + rpc) structural honesty
#     C1 bash -n both
#     C2 no global `rm -rf /tmp/ci*` anywhere in the 3 runners
#     C3 both write manifest.txt + pass RESULTS_DIR to aggregator + exit AGG_RC
#     C4 rpc runner uses a private ION_HOST_SOCKET, never kills ~/.ion/host.sock
#     C5 (E2E of old runners: NOT_RUN — needs real LLM workers; banned here)
#   Group D  regression of T01/T02 suites
#     D1 aggregate_ci_fault_ci.sh exits 0
#     D2 ci_matrix_schedule_ci.sh exits 0
#
# Usage: bash tests/ci_trust_gates_ci.sh
#
set -u

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
SB="$(mktemp -d /tmp/ion-trust-gates.XXXXXX)"
FP="$SB/fakeproj"
RUN_ROOTS=
trap 'rm -rf "$SB" $RUN_ROOTS' EXIT

P=0
F=0
ok()  { echo "  [PASS] $1"; P=$((P+1)); }
bad() { echo "  [FAIL] $1"; F=$((F+1)); }
capture_root() { grep -o '/tmp/ci-matrix-[0-9-]*[0-9]' "$1" | head -1; }

# ─── Group A: workflow integrity (static) ─────────────────────────────────
echo "== Group A: workflow integrity =="
n=$(grep -cE '^[[:space:]]*continue-on-error:' "$PROJECT_DIR/.github/workflows/ci.yml" || true)
[ "${n:-0}" -eq 0 ] && ok "A1 ci.yml: no continue-on-error directive" \
                     || bad "A1 ci.yml still has ${n} continue-on-error directive(s)"
if grep -q 'run: cargo test --tests$' "$PROJECT_DIR/.github/workflows/pr-gate.yml" \
   && ! grep -q 'cargo test --tests.*||' "$PROJECT_DIR/.github/workflows/pr-gate.yml"; then
    ok "A2 pr-gate: cargo test --tests runs bare"
else
    bad "A2 pr-gate: integration test errors still swallowed"
fi
if ! grep -q 'path: /tmp/ci-results/' "$PROJECT_DIR/.github/workflows/ci.yml" \
   && grep -q 'CI_RUN_ROOT' "$PROJECT_DIR/.github/workflows/ci.yml"; then
    ok "A3 ci.yml: uploads follow run-root pointer"
else
    bad "A3 ci.yml: stale artifact upload path"
fi
if ! grep -q '900 passed' "$PROJECT_DIR/scripts/run_ci_matrix_parallel.sh"; then
    ok "A4 parallel runner: no fabricated cargo output"
else
    bad "A4 parallel runner: fabricated '900 passed' still present"
fi

# ─── Group B: honest preflight + stamp + shim (fake git+cargo project) ────
echo "== Group B: preflight/stamp/shim behavior =="
mkdir -p "$FP/scripts" "$FP/tests" "$FP/.ion/monitors" "$FP/docs" "$FP/src/bin"
cp "$PROJECT_DIR/scripts/run_ci_matrix_parallel.sh" "$FP/scripts/"
cp "$PROJECT_DIR/scripts/aggregate_ci_results.sh" "$FP/scripts/"
cat > "$FP/Cargo.toml" <<'EOF'
[package]
name = "fakeproj"
version = "0.1.0"
edition = "2018"

[[bin]]
name = "ion"
path = "src/bin/ion.rs"
EOF
printf 'fn main() {}\n' > "$FP/src/bin/ion.rs"
cat > "$FP/src/lib.rs" <<'EOF'
pub fn hello() -> &'static str {
    "hi"
}

#[cfg(test)]
mod tests {
    #[test]
    fn one_passing_test() {
        assert_eq!(super::hello(), "hi");
    }
}
EOF
printf '# user monitor config\n' > "$FP/.ion/monitors/SENTINEL.cfg"
MON_BEFORE=$(cksum "$FP/.ion/monitors/SENTINEL.cfg" | awk '{print $1}')
# git repo so the tree fingerprint is real (stamp-cache path is exercised)
(cd "$FP" && git init -q && git add -A && \
 git -c user.email=t@t -c user.name=t commit -qm init) >/dev/null 2>&1
# prebuild the ion bin (runner preflight check needs it to exist)
(cd "$FP" && cargo build --bin ion) > "$SB/setup-build.log" 2>&1 \
    || { echo "  [FAIL] setup build error:"; tail -5 "$SB/setup-build.log"; exit 1; }

cat > "$FP/tests/a_check_ci.sh" <<'EOF'
#!/usr/bin/env bash
exec cargo check
EOF
cat > "$FP/tests/b_test_ci.sh" <<'EOF'
#!/usr/bin/env bash
out=$(cargo test --lib 2>&1); rc=$?
echo "$out"
[ $rc -ne 0 ] && exit $rc
echo "$out" | grep -q "test result: ok" || exit 1
echo "$out" | grep -q "900 passed" && exit 1   # fabricated marker must never appear
exit 0
EOF
cat > "$FP/tests/c_build_ci.sh" <<'EOF'
#!/usr/bin/env bash
exec cargo build
EOF
cat > "$FP/tests/monitor_ci.sh" <<'EOF'
#!/usr/bin/env bash
# SERIAL member; doubles as the fault injector.
if [ "${INJECT_FAULT:-0}" = "1" ]; then
    printf '\nfn broken_( {}\n' >> src/lib.rs   # tracked file -> stamp mismatch
fi
cargo check 2>&1 | tail -20
exit ${PIPESTATUS[0]}
EOF
chmod +x "$FP"/tests/*.sh

# --- Run 1: fault injected mid-run ---
INJECT_FAULT=1 \
PREFLIGHT_CMDS="cargo build --bin ion;cargo test --lib" \
    bash "$FP/scripts/run_ci_matrix_parallel.sh" > "$SB/run1.out" 2>&1
RC1=$?
RUN1=$(capture_root "$SB/run1.out")
[ -n "$RUN1" ] && RUN_ROOTS="$RUN_ROOTS $RUN1"

[ "$RC1" -ne 0 ] && ok "B1 mid-run break -> matrix exit ${RC1} (non-zero)" \
                 || bad "B1 matrix exited 0 despite injected failure"
[ -s "${RUN1}/preflight.log" ] && grep -q "Finished" "${RUN1}/preflight.log" \
    && ok "B2 preflight.log holds real cargo output" \
    || bad "B2 preflight.log missing/empty/no cargo output"
[ -f "${RUN1}/cargo-stamp" ] && grep -q '^fingerprint=' "${RUN1}/cargo-stamp" \
    && ok "B3 stamp written with tree fingerprint" \
    || bad "B3 cargo-stamp missing"
python3 -c '
import json, sys
d = json.load(open(sys.argv[1])); c = d["counts"]
ok = (d["verdict"] == "FAIL" and c["fail"] == 1 and c["pass"] == 3
      and "tests/monitor_ci.sh" in d["failed_scripts"])
sys.exit(0 if ok else 1)' "${RUN1}/results/aggregate-summary.json" \
    && ok "B4 verdicts: 3 PASS / injector FAIL / summary FAIL" \
    || bad "B4 summary does not reflect injected failure"
BT="${RUN1}/out/b_test_ci.log"
if [ -f "$BT" ] && grep -q "one_passing_test" "$BT" && grep -q "test result: ok" "$BT" \
   && ! grep -q "900 passed" "$BT"; then
    ok "B5 cargo test output is REAL (no fabrication)"
else
    bad "B5 cargo test output not real or fabricated"
fi
[ "$(cat /tmp/ci-matrix-latest 2>/dev/null)" = "$RUN1" ] \
    && ok "B6 /tmp/ci-matrix-latest points at this run" \
    || bad "B6 run-root pointer wrong"
MON_AFTER=$(cksum "$FP/.ion/monitors/SENTINEL.cfg" 2>/dev/null | awk '{print $1}')
[ "$MON_BEFORE" = "$MON_AFTER" ] && ok "B7 source .ion/monitors untouched" \
                                  || bad "B7 source .ion/monitors modified"

# --- Run 2: clean tree, stamp-cache should be used and logged ---
(cd "$FP" && git checkout -q -- src/lib.rs)
PREFLIGHT_CMDS="cargo build --bin ion;cargo test --lib" \
    bash "$FP/scripts/run_ci_matrix_parallel.sh" > "$SB/run2.out" 2>&1
RC2=$?
RUN2=$(capture_root "$SB/run2.out")
[ -n "$RUN2" ] && RUN_ROOTS="$RUN_ROOTS $RUN2"

[ "$RC2" -eq 0 ] && ok "B8 clean rerun exits 0" || bad "B8 clean rerun exit ${RC2}"
grep -q "validated by preflight" "${RUN2}/out/a_check_ci.log" 2>/dev/null \
    && ok "B9 stamp-cache no-op used and logged" \
    || bad "B9 stamp-cache path not exercised/logged"
grep -q "test result: ok" "${RUN2}/out/b_test_ci.log" 2>/dev/null \
    && ok "B9b clean rerun still runs REAL cargo test" \
    || bad "B9b cargo test did not run for real"

# ─── Group C: old runners structural honesty ──────────────────────────────
echo "== Group C: old runners structural =="
bash -n "$PROJECT_DIR/scripts/run_ci_matrix.sh" \
    && bash -n "$PROJECT_DIR/scripts/run_ci_matrix_rpc.sh" \
    && ok "C1 serial+rpc runners: bash -n OK" || bad "C1 syntax error in old runners"
g=$(grep -h 'rm -rf /tmp/ci' "$PROJECT_DIR/scripts/run_ci_matrix.sh" \
    "$PROJECT_DIR/scripts/run_ci_matrix_rpc.sh" \
    "$PROJECT_DIR/scripts/run_ci_matrix_parallel.sh" || true)
[ -z "$g" ] && ok "C2 no global /tmp/ci cleanup in any runner" \
              || bad "C2 leftover: ${g}"
for r in run_ci_matrix.sh run_ci_matrix_rpc.sh; do
    if grep -q 'manifest.txt' "$PROJECT_DIR/scripts/$r" \
       && grep -q 'RESULTS_DIR=' "$PROJECT_DIR/scripts/$r" \
       && grep -q 'exit "$AGG_RC"' "$PROJECT_DIR/scripts/$r"; then
        ok "C3 ${r}: manifest + RESULTS_DIR + exit propagation"
    else
        bad "C3 ${r}: aggregator cooperation incomplete"
    fi
done
if ! grep -q 'lsof -ti' "$PROJECT_DIR/scripts/run_ci_matrix_rpc.sh" \
   && ! grep -q 'HOME/.ion/host.sock' "$PROJECT_DIR/scripts/run_ci_matrix_rpc.sh" \
   && grep -q 'ION_HOST_SOCKET' "$PROJECT_DIR/scripts/run_ci_matrix_rpc.sh"; then
    ok "C4 rpc runner: private socket, never kills user host"
else
    bad "C4 rpc runner still touches ~/.ion/host.sock"
fi
echo "  [SKIP] C5 old-runner E2E: NOT_RUN (needs real LLM workers; banned by no-paid-API rule)"

# ─── Group D: regression of T01/T02 suites ────────────────────────────────
echo "== Group D: regression =="
bash "$PROJECT_DIR/tests/aggregate_ci_fault_ci.sh" > "$SB/d1.log" 2>&1 \
    && ok "D1 T01 aggregate_ci_fault_ci still green" \
    || { bad "D1 T01 suite regressed"; tail -5 "$SB/d1.log"; }
bash "$PROJECT_DIR/tests/ci_matrix_schedule_ci.sh" > "$SB/d2.log" 2>&1 \
    && ok "D2 T02 ci_matrix_schedule_ci still green" \
    || { bad "D2 T02 suite regressed"; tail -5 "$SB/d2.log"; }

# ─── Summary ───────────────────────────────────────────────────────────────
echo ""
echo "========================================="
echo "  ci_trust_gates_ci: ${P} passed / ${F} failed (1 SKIP)"
echo "========================================="
[ "$F" -eq 0 ]
