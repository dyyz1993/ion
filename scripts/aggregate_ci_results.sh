#!/usr/bin/env bash
#
# aggregate_ci_results.sh — Aggregate CI matrix results and gate on failures.
#
# Input:  $RESULTS_DIR/*.jsonl (default /tmp/ci-results), one JSON object per
#         line, written by the CI matrix runners:
#           {"script":"tests/xxx_ci.sh","status":"PASS","exit_code":0,"duration_s":12,"log_path":"/tmp/..."}
#           {"script":"tests/yyy_ci.sh","status":"FAIL","exit_code":1,"duration_s":5,"log_path":"/tmp/..."}
#           {"script":"tests/zzz_ci.sh","status":"SKIP","reason":"env-dependent","exit_code":-1,"duration_s":0}
#
# Manifest (optional, recommended): $RESULTS_DIR/manifest.txt — one script
# path per line (# comments allowed), listing every script the runner
# scheduled (run or skipped). When present, every manifest entry must have
# at least one result, and every result must belong to a manifest entry.
# Runners that don't write a manifest simply skip this check.
#
# Output: $REPORT_PATH (default docs/testing/CI_MATRIX_REPORT.md)
#         $RESULTS_DIR/aggregate-summary.json (machine-readable verdict)
#
# Exit codes (ANY non-zero means the matrix did NOT pass):
#   0  all scheduled scripts PASS/SKIP, all records well-formed
#   1  at least one FAIL attempt — a later PASS does NOT erase an earlier FAIL
#   2  manifest mismatch (scheduled script without result, or result for an
#      unscheduled script)
#   3  malformed records (unparseable line / missing 'script' / bad 'status'),
#      or internal aggregator error
#   4  no usable input (results dir missing, or no records at all)
#
# Every attempt per script is retained (retry history appears in the report);
# "dedup by overwriting earlier attempts" is intentionally gone.
#
set -o pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
RESULTS_DIR="${RESULTS_DIR:-/tmp/ci-results}"
MANIFEST_FILE="${MANIFEST_FILE:-$RESULTS_DIR/manifest.txt}"
REPORT_PATH="${REPORT_PATH:-$PROJECT_DIR/docs/testing/CI_MATRIX_REPORT.md}"
SUMMARY_JSON="$RESULTS_DIR/aggregate-summary.json"

if [ ! -d "$RESULTS_DIR" ]; then
    echo "❌ Results dir not found: $RESULTS_DIR"
    exit 4
fi

mkdir -p "$(dirname "$REPORT_PATH")"

python3 - "$RESULTS_DIR" "$MANIFEST_FILE" "$REPORT_PATH" "$SUMMARY_JSON" <<'PYEOF'
import datetime
import glob
import json
import os
import sys

results_dir, manifest_path, report_path, summary_path = sys.argv[1:5]
VALID_STATUS = ("PASS", "FAIL", "SKIP")


def md(s):
    return str(s).replace("|", "\\|").replace("\n", " ")


# ── Parse every *.jsonl (skip all.jsonl merged copies: reading one would
#    double-count every record already present in the per-script files).
files = [f for f in sorted(glob.glob(os.path.join(results_dir, "*.jsonl")))
         if os.path.basename(f) != "all.jsonl"]

attempts = []   # parsed records, arrival order, with _file/_line attached
malformed = []  # (file, line_no, reason, excerpt)
for path in files:
    try:
        with open(path, encoding="utf-8", errors="replace") as fh:
            lines = fh.read().splitlines()
    except OSError as exc:
        malformed.append((os.path.basename(path), 0, "unreadable file: %s" % exc, ""))
        continue
    for no, raw in enumerate(lines, 1):
        line = raw.strip()
        if not line:
            continue
        try:
            rec = json.loads(line)
            if not isinstance(rec, dict):
                raise ValueError("record is not a JSON object")
            if not isinstance(rec.get("script"), str) or not rec["script"]:
                raise ValueError("missing or empty 'script'")
            if rec.get("status") not in VALID_STATUS:
                raise ValueError("bad 'status': %r" % (rec.get("status"),))
        except Exception as exc:
            malformed.append((os.path.basename(path), no, str(exc), line[:120]))
            continue
        rec["_file"] = os.path.basename(path)
        rec["_line"] = no
        attempts.append(rec)

# ── Group by script, preserving arrival order; keep ALL attempts.
by_script = {}
order = []
for rec in attempts:
    by_script.setdefault(rec["script"], []).append(rec)
    if rec["script"] not in order:
        order.append(rec["script"])

# ── Manifest check (only when the runner wrote one).
manifest = None
missing = []
unexpected = []
if manifest_path and os.path.isfile(manifest_path):
    with open(manifest_path, encoding="utf-8") as fh:
        manifest = [l.strip() for l in fh
                    if l.strip() and not l.lstrip().startswith("#")]
    missing = [s for s in manifest if s not in by_script]
    unexpected = [s for s in order if s not in manifest]

failed = [(s, [a for a in by_script[s] if a["status"] == "FAIL"])
          for s in order if any(a["status"] == "FAIL" for a in by_script[s])]
retried = [(s, by_script[s]) for s in order if len(by_script[s]) > 1]

pass_n = sum(1 for a in attempts if a["status"] == "PASS")
fail_n = sum(1 for a in attempts if a["status"] == "FAIL")
skip_n = sum(1 for a in attempts if a["status"] == "SKIP")

if not attempts and not malformed:
    code = 4
elif malformed:
    code = 3
elif missing or unexpected:
    code = 2
elif fail_n:
    code = 1
else:
    code = 0

now = datetime.datetime.utcnow().strftime("%Y-%m-%dT%H:%M:%SZ")

rep = []
rep.append("# CI Matrix Report")
rep.append("")
rep.append("> Auto-generated by `scripts/aggregate_ci_results.sh`")
rep.append("> Run: `bash scripts/run_ci_matrix_parallel.sh` (or run_ci_matrix.sh / run_ci_matrix_rpc.sh)")
rep.append("> Date: %s" % now)
rep.append("> Results dir: `%s`" % results_dir)
if manifest is not None:
    rep.append("> Manifest: `%s` (%d entries)" % (manifest_path, len(manifest)))
else:
    rep.append("> Manifest: *(none — missing-result check disabled)*")
rep.append("")
rep.append("## Verdict: %s (exit %d)" % ("✅ PASS" if code == 0 else "❌ FAIL", code))
rep.append("")

problems = []
if fail_n:
    problems.append("%d FAIL attempt(s) across %d script(s)" % (fail_n, len(failed)))
if missing:
    problems.append("%d scheduled script(s) missing results" % len(missing))
if unexpected:
    problems.append("%d result(s) for unscheduled script(s)" % len(unexpected))
if malformed:
    problems.append("%d malformed record(s)" % len(malformed))
if problems:
    rep.append("Problems:")
    for p in problems:
        rep.append("- %s" % p)
    rep.append("")

rep.append("## Summary")
rep.append("")
rep.append("| Metric | Count |")
rep.append("|--------|-------|")
rep.append("| Unique scripts | %d |" % len(by_script))
rep.append("| Total attempts | %d |" % len(attempts))
rep.append("| ✅ PASS attempts | %d |" % pass_n)
rep.append("| ❌ FAIL attempts | %d |" % fail_n)
rep.append("| ⏭️ SKIP attempts | %d |" % skip_n)
rep.append("| Retried scripts | %d |" % len(retried))
if manifest is not None:
    rep.append("| Missing results | %d |" % len(missing))
    rep.append("| Unscheduled results | %d |" % len(unexpected))
rep.append("| Malformed records | %d |" % len(malformed))
rep.append("")

if failed:
    rep.append("## ❌ Failed Scripts")
    rep.append("")
    rep.append("| Script | Attempt | Exit | Duration | Log |")
    rep.append("|--------|---------|------|----------|-----|")
    for s, fattempts in failed:
        all_attempts = by_script[s]
        for a in fattempts:
            idx = all_attempts.index(a) + 1
            log = a.get("log_path", "-")
            base = os.path.basename(str(s))
            if base.endswith(".sh"):
                base = base[:-3]
            rep.append("| `%s` | %d/%d | %s | %ss | [%s](%s) |" % (
                md(s), idx, len(all_attempts), md(a.get("exit_code", "?")),
                md(a.get("duration_s", "?")), md(base), md(log)))
    rep.append("")

    rep.append("## Error Excerpts")
    rep.append("")
    rep.append("<details><summary>Click to expand failure logs (last 20 lines per failed attempt)</summary>")
    rep.append("")
    for s, fattempts in failed:
        for a in fattempts:
            log = a.get("log_path", "")
            rep.append("### %s (attempt at %s:%s)" % (s, a.get("_file"), a.get("_line")))
            rep.append("")
            if log and os.path.isfile(log):
                rep.append("```")
                try:
                    with open(log, encoding="utf-8", errors="replace") as fh:
                        tail = fh.read().splitlines()[-20:]
                    rep.extend(tail)
                except OSError as exc:
                    rep.append("(log unreadable: %s)" % exc)
                rep.append("```")
            else:
                rep.append("(log file not found: %s)" % log)
            rep.append("")
    rep.append("</details>")
    rep.append("")

if retried:
    rep.append("## 🔁 Retry History (later PASS does not erase an earlier FAIL)")
    rep.append("")
    rep.append("| Script | Attempt sequence | Verdict contribution |")
    rep.append("|--------|------------------|----------------------|")
    for s, atts in retried:
        seq = "→".join(a["status"] for a in atts)
        worst = "FAIL" if any(a["status"] == "FAIL" for a in atts) else atts[-1]["status"]
        rep.append("| `%s` | %s | %s |" % (md(s), seq, worst))
    rep.append("")

skip_scripts = [(s, by_script[s]) for s in order
                if all(a["status"] == "SKIP" for a in by_script[s]) and by_script[s]]
if skip_scripts:
    rep.append("## ⏭️ Skipped Scripts")
    rep.append("")
    rep.append("| Script | Reason |")
    rep.append("|--------|--------|")
    for s, atts in skip_scripts:
        reasons = {str(a.get("reason", "unknown")) for a in atts}
        rep.append("| `%s` | %s |" % (md(s), md(", ".join(sorted(reasons)))))
    rep.append("")

if missing:
    rep.append("## ⚠️ Scheduled but Missing Results")
    rep.append("")
    for s in missing:
        rep.append("- `%s`" % md(s))
    rep.append("")

if unexpected:
    rep.append("## ⚠️ Results for Unscheduled Scripts")
    rep.append("")
    for s in unexpected:
        rep.append("- `%s`" % md(s))
    rep.append("")

if malformed:
    rep.append("## ⚠️ Malformed Records")
    rep.append("")
    rep.append("| Location | Reason | Line excerpt |")
    rep.append("|----------|--------|--------------|")
    for fname, no, reason, excerpt in malformed:
        rep.append("| `%s:%s` | %s | `%s` |" % (md(fname), md(no), md(reason), md(excerpt)))
    rep.append("")

passed = [s for s in order if by_script[s][-1]["status"] == "PASS"
          and not any(a["status"] == "FAIL" for a in by_script[s])]
if passed:
    rep.append("## ✅ Passed Scripts")
    rep.append("")
    rep.append("<details><summary>%d scripts passed — click to expand</summary>" % len(passed))
    rep.append("")
    for s in passed:
        atts = by_script[s]
        dur = atts[-1].get("duration_s", "?")
        extra = " (+%d earlier attempt(s))" % (len(atts) - 1) if len(atts) > 1 else ""
        rep.append("- `%s` (%ss)%s" % (s, dur, extra))
    rep.append("")
    rep.append("</details>")
    rep.append("")

rep.append("---")
rep.append("")
rep.append("## Next Steps")
rep.append("")
if code == 0:
    rep.append("All scheduled scripts passed or were skipped with a reason. ✅")
else:
    rep.append("This matrix round FAILED (exit %d). Per problem above:" % code)
    rep.append("")
    rep.append("1. **FAIL** — read the error excerpt; a script bug gets fixed in a worktree,")
    rep.append("   an environment-dependent failure gets a `SKIP` with reason and a re-run.")
    rep.append("2. **Missing / unscheduled** — runner scheduling bug; check manifest vs results.")
    rep.append("3. **Malformed** — a runner wrote a broken record; fix the writer, don't hand-edit results.")
rep.append("")

with open(report_path, "w", encoding="utf-8") as fh:
    fh.write("\n".join(rep) + "\n")

summary = {
    "verdict": "PASS" if code == 0 else "FAIL",
    "exit_code": code,
    "generated_at": now,
    "results_dir": results_dir,
    "counts": {
        "unique_scripts": len(by_script),
        "attempts": len(attempts),
        "pass": pass_n,
        "fail": fail_n,
        "skip": skip_n,
        "retried_scripts": len(retried),
        "malformed": len(malformed),
        "missing": len(missing),
        "unexpected": len(unexpected),
    },
    "failed_scripts": [s for s, _ in failed],
    "missing_scripts": missing,
    "unexpected_scripts": unexpected,
}
with open(summary_path, "w", encoding="utf-8") as fh:
    json.dump(summary, fh, indent=2, sort_keys=True)
    fh.write("\n")

print("Report: %s" % report_path)
print("Summary: %s" % summary_path)
print("Verdict: %s (exit %d)" % (summary["verdict"], code))
print("Unique scripts: %d | attempts: %d | PASS: %d / FAIL: %d / SKIP: %d | "
      "retried: %d | malformed: %d | missing: %d | unscheduled: %d"
      % (len(by_script), len(attempts), pass_n, fail_n, skip_n,
         len(retried), len(malformed), len(missing), len(unexpected)))

sys.exit(code)
PYEOF
rc=$?

if [ ! -f "$SUMMARY_JSON" ]; then
    echo "❌ aggregator internal error (python exited $rc, no summary written)"
    exit 3
fi
exit $rc
