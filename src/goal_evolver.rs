//! Goal Evolver — analyzes goal-run logs and plans improvement Issues.
//!
//! Reads iterations.jsonl + final-report.json from a goal-runs directory,
//! analyzes across 3 dimensions (deadloop / model / context), and produces
//! Issue plans that a human (or A→B self-evolution) can act on.
//!
//! Design: docs/design/GOAL_SUPERVISOR.md section 8 (Evolution System).
//! Fixtures: tests/fixtures/goal-runs/ (10 scenarios).

use std::path::{Path, PathBuf};

// ===========================================================================
// Data structures
// ===========================================================================

/// One parsed iteration record (from a line of iterations.jsonl).
#[derive(Debug, Clone)]
pub struct IterationRecord {
    pub iter: u32,
    pub goal_id: String,
    pub objective: String,
    pub guards_hit: GuardsHit,
    pub similarity_to_prev: Option<f64>,
    pub llm_calls: Vec<LlmCallRecord>,
    pub context_snapshot: ContextSnapshot,
    pub checks_run: Vec<CheckRunRecord>,
    pub all_passed: bool,
    pub failed_checks: Vec<String>,
    pub total_cost_usd: f64,
}

#[derive(Debug, Clone, Default)]
pub struct GuardsHit {
    pub repetitive: bool,
    pub max_iter: bool,
    pub max_duration: bool,
    pub max_cost: bool,
    pub low_confidence: bool,
}

#[derive(Debug, Clone)]
pub struct LlmCallRecord {
    pub purpose: String,   // "generate_checks" | "analyze_failure"
    pub model: String,
    pub checks_generated: Option<u32>,
    pub checks_quality: Option<String>,
    pub analysis_used: Option<bool>,
    pub led_to_fix: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct ContextSnapshot {
    pub recent_messages: u32,
    pub file_changes: Vec<String>,
    pub git_diff_lines: u64,
    pub test_results_included: bool,
    pub skill_loaded: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CheckRunRecord {
    pub name: String,
    pub status: String,   // "pass" | "fail" | "error" | "skip"
    pub evidence_exit_code: Option<i64>,
}

/// One complete goal run (all iterations + final report).
#[derive(Debug, Clone)]
pub struct GoalRun {
    pub goal_id: String,
    pub iterations: Vec<IterationRecord>,
    pub final_status: String,
    pub stopped_reason: String,
    pub total_iterations: u32,
    pub outcome: String,           // "fixed" | "abandoned"
    pub diagnosis_hint: Option<String>,
}

// ===========================================================================
// Findings & Issue plans
// ===========================================================================

/// A problem found during analysis, mapped to one of the 3 dimensions.
#[derive(Debug, Clone)]
pub struct Finding {
    pub dimension: Dimension,
    pub severity: Severity,
    pub title: String,
    pub evidence: String,
    pub suggestion: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Dimension {
    Deadloop,
    Model,
    Context,
    Boundary,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Severity {
    High,
    Medium,
    Low,
}

/// A planned GitHub Issue (what the evolver would submit).
#[derive(Debug, Clone)]
pub struct IssuePlan {
    pub title: String,
    pub dimension: Dimension,
    pub severity: Severity,
    pub body: String,
}

/// The full report from analyzing one or more goal runs.
#[derive(Debug, Clone)]
pub struct EvolverReport {
    pub analyzed_goals: usize,
    pub total_iterations: u32,
    pub issues_planned: Vec<IssuePlan>,
}

// ===========================================================================
// Parsing — read fixture files into GoalRun
// ===========================================================================

/// Parse one goal-run directory (iterations.jsonl + final-report.json).
pub fn parse_goal_run(dir: &Path) -> Result<GoalRun, String> {
    let iters_path = dir.join("iterations.jsonl");
    let report_path = dir.join("final-report.json");

    // Parse iterations.jsonl (one JSON per line).
    let mut iterations = Vec::new();
    if iters_path.exists() {
        let content = std::fs::read_to_string(&iters_path)
            .map_err(|e| format!("read iterations.jsonl: {e}"))?;
        for (lineno, line) in content.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let v: serde_json::Value = serde_json::from_str(line)
                .map_err(|e| format!("iterations.jsonl:{}: {}", lineno + 1, e))?;
            iterations.push(parse_iteration(&v));
        }
    }

    // Parse final-report.json.
    let (final_status, stopped_reason, total_iterations, outcome, diagnosis_hint, goal_id) = {
        let report: serde_json::Value = std::fs::read_to_string(&report_path)
            .map_err(|e| format!("read final-report.json: {e}"))
            .and_then(|s| serde_json::from_str(&s).map_err(|e| format!("parse final-report.json: {e}")))?;
        (
            report["final_status"].as_str().unwrap_or("unknown").to_string(),
            report["stopped_reason"].as_str().unwrap_or("unknown").to_string(),
            report["total_iterations"].as_u64().unwrap_or(0) as u32,
            report["outcome"].as_str().unwrap_or("unknown").to_string(),
            report["outcome_detail"]["diagnosis_hint"].as_str().map(|s| s.to_string()),
            report["goal_id"].as_str().unwrap_or("unknown").to_string(),
        )
    };

    // goal_id from iterations if report didn't have it.
    let goal_id = if goal_id == "unknown" {
        iterations.first().map(|i| i.goal_id.clone()).unwrap_or_default()
    } else {
        goal_id
    };

    Ok(GoalRun {
        goal_id,
        iterations,
        final_status,
        stopped_reason,
        total_iterations,
        outcome,
        diagnosis_hint,
    })
}

fn parse_iteration(v: &serde_json::Value) -> IterationRecord {
    let guards = v["guards_hit"].as_object();
    let guards_hit = GuardsHit {
        repetitive: guards.and_then(|g| g.get("repetitive").and_then(|b| b.as_bool())).unwrap_or(false),
        max_iter: guards.and_then(|g| g.get("max_iter").and_then(|b| b.as_bool())).unwrap_or(false),
        max_duration: guards.and_then(|g| g.get("max_duration").and_then(|b| b.as_bool())).unwrap_or(false),
        max_cost: guards.and_then(|g| g.get("max_cost").and_then(|b| b.as_bool())).unwrap_or(false),
        low_confidence: guards.and_then(|g| g.get("low_confidence").and_then(|b| b.as_bool())).unwrap_or(false),
    };

    let llm_calls = v["llm_calls"].as_array().map(|arr| {
        arr.iter().map(|c| LlmCallRecord {
            purpose: c["purpose"].as_str().unwrap_or("").to_string(),
            model: c["model"].as_str().unwrap_or("").to_string(),
            checks_generated: c["checks_generated"].as_u64().map(|n| n as u32),
            checks_quality: c["checks_quality"].as_str().map(|s| s.to_string()),
            analysis_used: c["analysis_used"].as_bool(),
            led_to_fix: c["led_to_fix"].as_bool(),
        }).collect()
    }).unwrap_or_default();

    let ctx = &v["context_snapshot"];
    let context_snapshot = ContextSnapshot {
        recent_messages: ctx["recent_messages"].as_u64().unwrap_or(0) as u32,
        file_changes: ctx["file_changes"].as_array()
            .map(|a| a.iter().filter_map(|f| f.as_str().map(String::from)).collect())
            .unwrap_or_default(),
        git_diff_lines: ctx["git_diff_lines"].as_u64().unwrap_or(0),
        test_results_included: ctx["test_results_included"].as_bool().unwrap_or(false),
        skill_loaded: ctx["skill_loaded"].as_str().map(String::from),
    };

    let checks_run = v["checks_run"].as_array().map(|arr| {
        arr.iter().map(|c| CheckRunRecord {
            name: c["name"].as_str().unwrap_or("").to_string(),
            status: c["status"].as_str().unwrap_or("").to_string(),
            evidence_exit_code: c["evidence"]["exit_code"].as_i64(),
        }).collect()
    }).unwrap_or_default();

    let failed_checks = v["failed_checks"].as_array()
        .map(|a| a.iter().filter_map(|f| f.as_str().map(String::from)).collect())
        .unwrap_or_default();

    IterationRecord {
        iter: v["iter"].as_u64().unwrap_or(0) as u32,
        goal_id: v["goal_id"].as_str().unwrap_or("").to_string(),
        objective: v["objective"].as_str().unwrap_or("").to_string(),
        guards_hit,
        similarity_to_prev: v["similarity_to_prev"].as_f64(),
        llm_calls,
        context_snapshot,
        checks_run,
        all_passed: v["all_passed"].as_bool().unwrap_or(false),
        failed_checks,
        total_cost_usd: v["total_cost_usd"].as_f64().unwrap_or(0.0),
    }
}

// ===========================================================================
// Analysis — 3 dimensions
// ===========================================================================

/// Analyze one goal run across all 3 dimensions, return findings.
pub fn analyze_goal_run(run: &GoalRun) -> Vec<Finding> {
    let mut findings = Vec::new();

    // ── Q1: Deadloop risk ──
    findings.extend(analyze_deadloop(run));

    // ── Q2: Model selection ──
    findings.extend(analyze_model(run));

    // ── Q3: Context sufficiency ──
    findings.extend(analyze_context(run));

    findings
}

/// Q1: Detect deadloop patterns.
///
/// Signals:
/// - repetitive guard fired + outcome=abandoned → deadloop confirmed
/// - Same failed check across 2+ iterations → stuck on one check
/// - Low similarity but still exhausted → agent thrashing (different errors each time)
fn analyze_deadloop(run: &GoalRun) -> Vec<Finding> {
    let mut findings = Vec::new();

    let repetitive_count = run.iterations.iter().filter(|i| i.guards_hit.repetitive).count();
    let exhausted = run.outcome == "abandoned";

    // Pattern A: repetitive + abandoned = classic deadloop.
    if repetitive_count > 0 && exhausted {
        // Find which check(s) failed repeatedly.
        let mut fail_counts: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
        for it in &run.iterations {
            for fc in &it.failed_checks {
                *fail_counts.entry(fc.as_str()).or_insert(0) += 1;
            }
        }
        let stuck_check = fail_counts.iter().max_by_key(|(_, c)| **c).map(|(n, c)| (*n, *c));

        let check_detail = match stuck_check {
            Some((name, count)) if count >= 2 => format!(" Check '{}' failed in {} iterations.", name, count),
            _ => String::new(),
        };

        findings.push(Finding {
            dimension: Dimension::Deadloop,
            severity: Severity::High,
            title: format!("deadloop: repetitive guard fired, goal abandoned{}", check_detail),
            evidence: format!(
                "goal_id={}, repetitive_hits={}, outcome={}, stopped_reason={}",
                run.goal_id, repetitive_count, run.outcome, run.stopped_reason
            ),
            suggestion: run.diagnosis_hint.clone().unwrap_or_else(||
                "Investigate why the agent couldn't break out of the loop. Check if the skill is missing a step or if a stronger model is needed.".into()
            ),
        });
    }

    // Pattern B: exhausted without repetitive (max_iter hit) + different errors → thrashing.
    let max_iter_hit = run.stopped_reason == "max_iterations";
    if max_iter_hit && repetitive_count == 0 && exhausted && run.iterations.len() >= 3 {
        findings.push(Finding {
            dimension: Dimension::Deadloop,
            severity: Severity::Medium,
            title: format!("thrashing: exhausted via max_iter without repetition (different errors each time)"),
            evidence: format!(
                "goal_id={}, iters={}, no repetitive guard fired → agent hitting different errors",
                run.goal_id, run.total_iterations
            ),
            suggestion: "Agent may lack the skill to solve this class of problem. Consider a stronger model or more specific skill guidance.".into(),
        });
    }

    findings
}

/// Q2: Detect wrong model selection.
///
/// Signals:
/// - generate_checks used a weak model (fast tier) + checks_quality=poor
/// - analyze_failure model's analysis_used=false consistently → model output not actionable
fn analyze_model(run: &GoalRun) -> Vec<Finding> {
    let mut findings = Vec::new();

    // Check generate_checks model quality.
    let gen_checks: Vec<&LlmCallRecord> = run.iterations.iter()
        .flat_map(|i| i.llm_calls.iter().filter(|c| c.purpose == "generate_checks"))
        .collect();

    if let Some(gc) = gen_checks.first() {
        let is_weak = gc.model.contains("flash") || gc.model.contains("fast");
        let poor_quality = gc.checks_quality.as_deref() == Some("poor");
        let few_checks = gc.checks_generated.unwrap_or(10) < 3;

        if is_weak && (poor_quality || few_checks) {
            findings.push(Finding {
                dimension: Dimension::Model,
                severity: Severity::High,
                title: format!("generate_checks used weak model '{}' producing {} checks (quality: {})",
                    gc.model, gc.checks_generated.unwrap_or(0), gc.checks_quality.as_deref().unwrap_or("?")),
                evidence: format!("goal_id={}, model={}, checks={:?}, quality={:?}",
                    run.goal_id, gc.model, gc.checks_generated, gc.checks_quality),
                suggestion: "Switch generate_checks to a max-tier model (zai/glm-5.2). Weak models miss critical checks (e.g., security, edge cases).".into(),
            });
        }
    }

    // Check analyze_failure model effectiveness.
    let analysis_calls: Vec<&LlmCallRecord> = run.iterations.iter()
        .flat_map(|i| i.llm_calls.iter().filter(|c| c.purpose == "analyze_failure"))
        .collect();
    let never_used = analysis_calls.iter().filter(|c| c.analysis_used == Some(false)).count();
    let total_analysis = analysis_calls.len();

    if total_analysis >= 2 && never_used == total_analysis {
        let model = analysis_calls.first().map(|c| c.model.as_str()).unwrap_or("?");
        findings.push(Finding {
            dimension: Dimension::Model,
            severity: Severity::Medium,
            title: format!("analyze_failure model '{}' output never used ({} calls, 0 adopted)", model, never_used),
            evidence: format!("goal_id={}, analysis_used=false in all {} iterations", run.goal_id, never_used),
            suggestion: format!("The failure analysis from '{}' was never actionable. Switch to a stronger model that can read error output and produce concrete fix instructions.", model),
        });
    }

    findings
}

/// Q3: Detect insufficient context.
///
/// Signals:
/// - test_results_included=false across iterations
/// - git_diff_lines=0 across iterations
/// - low_confidence guard fired
fn analyze_context(run: &GoalRun) -> Vec<Finding> {
    let mut findings = Vec::new();

    // Missing test results.
    let no_tests = run.iterations.iter().filter(|i| !i.context_snapshot.test_results_included).count();
    let low_conf = run.iterations.iter().filter(|i| i.guards_hit.low_confidence).count();
    if no_tests == run.iterations.len() && !run.iterations.is_empty() && (low_conf > 0 || run.outcome == "abandoned") {
        findings.push(Finding {
            dimension: Dimension::Context,
            severity: Severity::High,
            title: "context missing: test_results_included=false across all iterations".into(),
            evidence: format!("goal_id={}, test_results_included=false in {} iterations, low_confidence_hits={}", run.goal_id, no_tests, low_conf),
            suggestion: "The skill must inject test stdout into context_snapshot before generate_checks/analyze_failure. Without test output, the agent cannot diagnose assertion failures.".into(),
        });
    }

    // Missing git diff.
    let no_diff = run.iterations.iter().filter(|i| i.context_snapshot.git_diff_lines == 0).count();
    if no_diff == run.iterations.len() && !run.iterations.is_empty() && run.outcome == "abandoned" {
        findings.push(Finding {
            dimension: Dimension::Context,
            severity: Severity::High,
            title: "context missing: git_diff_lines=0 across all iterations".into(),
            evidence: format!("goal_id={}, git_diff_lines=0 in {} iterations → agent had no visibility into changes", run.goal_id, no_diff),
            suggestion: "context_snapshot MUST include git diff (or uncommitted changes) before generate_checks. Without seeing what changed, checks cannot cover the right files.".into(),
        });
    }

    findings
}

// ===========================================================================
// Issue planning
// ===========================================================================

/// Convert findings into Issue plans (one Issue per High/Medium finding).
pub fn plan_issues(findings: &[Finding]) -> Vec<IssuePlan> {
    findings.iter()
        .filter(|f| f.severity == Severity::High || f.severity == Severity::Medium)
        .map(|f| IssuePlan {
            title: format!("[goal-evolver] {} {}", dimension_label(&f.dimension), f.title),
            dimension: f.dimension.clone(),
            severity: f.severity.clone(),
            body: format!(
                "## Dimension\n{}\n\n## Severity\n{}\n\n## Evidence\n```\n{}\n```\n\n## Suggestion\n{}\n",
                dimension_label(&f.dimension),
                match f.severity { Severity::High => "high", Severity::Medium => "medium", Severity::Low => "low" },
                f.evidence,
                f.suggestion,
            ),
        })
        .collect()
}

fn dimension_label(d: &Dimension) -> &'static str {
    match d {
        Dimension::Deadloop => "[deadloop]",
        Dimension::Model => "[model]",
        Dimension::Context => "[context]",
        Dimension::Boundary => "[boundary]",
    }
}

// ===========================================================================
// Entry point — run_once
// ===========================================================================

/// Analyze all goal runs under a directory, return a report.
///
/// - `data_dir`: path to a directory containing case subdirs (each with
///   iterations.jsonl + final-report.json), OR a single case dir directly.
/// - Returns EvolverReport with planned issues.
pub fn run_once(data_dir: &str) -> Result<EvolverReport, String> {
    let root = PathBuf::from(data_dir);
    if !root.exists() {
        return Err(format!("data_dir does not exist: {}", data_dir));
    }

    // Collect goal-run directories. If the root itself has iterations.jsonl,
    // treat it as a single run. Otherwise scan subdirs.
    let mut run_dirs: Vec<PathBuf> = Vec::new();
    if root.join("iterations.jsonl").exists() {
        run_dirs.push(root.clone());
    } else {
        let entries = std::fs::read_dir(&root).map_err(|e| format!("read_dir: {e}"))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("dir entry: {e}"))?;
            let path = entry.path();
            if path.is_dir() && path.join("iterations.jsonl").exists() {
                run_dirs.push(path);
            }
        }
    }

    run_dirs.sort();

    let mut all_findings = Vec::new();
    let mut total_iterations = 0u32;
    let analyzed = run_dirs.len();

    for dir in &run_dirs {
        match parse_goal_run(dir) {
            Ok(run) => {
                total_iterations += run.total_iterations;
                let findings = analyze_goal_run(&run);
                all_findings.extend(findings);
            }
            Err(e) => {
                // Skip unparseable runs but note in report (don't fail the whole thing).
                tracing::warn!("[goal-evolver] failed to parse {}: {}", dir.display(), e);
            }
        }
    }

    let issues = plan_issues(&all_findings);

    Ok(EvolverReport {
        analyzed_goals: analyzed,
        total_iterations,
        issues_planned: issues,
    })
}

// ===========================================================================
// Unit tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_dir(name: &str) -> PathBuf {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        PathBuf::from(manifest_dir)
            .join("tests/fixtures/goal-runs")
            .join(name)
    }

    // ── Q1 deadloop ──

    #[test]
    fn test_case_02_strict_check_finds_deadloop() {
        let run = parse_goal_run(&fixture_dir("case_02_deadloop_strict_check")).unwrap();
        let findings = analyze_goal_run(&run);
        let deadloop_findings: Vec<_> = findings.iter().filter(|f| f.dimension == Dimension::Deadloop).collect();
        assert!(!deadloop_findings.is_empty(), "must find deadloop finding for strict-check case");
        assert!(deadloop_findings.iter().any(|f| f.severity == Severity::High), "should be high severity");
    }

    #[test]
    fn test_case_03_weak_agent_finds_deadloop() {
        let run = parse_goal_run(&fixture_dir("case_03_deadloop_weak_agent")).unwrap();
        let findings = analyze_goal_run(&run);
        assert!(findings.iter().any(|f| f.dimension == Dimension::Deadloop), "must find deadloop");
    }

    // ── Q2 model ──

    #[test]
    fn test_case_04_weak_generate_checks_model() {
        let run = parse_goal_run(&fixture_dir("case_04_model_wrong_for_checks")).unwrap();
        let findings = analyze_goal_run(&run);
        let model_findings: Vec<_> = findings.iter().filter(|f| f.dimension == Dimension::Model).collect();
        assert!(!model_findings.is_empty(), "must flag weak generate_checks model");
        assert!(model_findings.iter().any(|f| f.evidence.contains("deepseek-v4-flash")));
    }

    #[test]
    fn test_case_05_weak_analyze_failure_model() {
        let run = parse_goal_run(&fixture_dir("case_05_model_wrong_for_analysis")).unwrap();
        let findings = analyze_goal_run(&run);
        assert!(
            findings.iter().any(|f| f.dimension == Dimension::Model && f.evidence.contains("analysis_used=false")),
            "must flag analyze_failure model never used"
        );
    }

    // ── Q3 context ──

    #[test]
    fn test_case_06_missing_test_results() {
        let run = parse_goal_run(&fixture_dir("case_06_missing_context_tests")).unwrap();
        let findings = analyze_goal_run(&run);
        assert!(
            findings.iter().any(|f| f.dimension == Dimension::Context && f.evidence.contains("test_results_included=false")),
            "must flag missing test results"
        );
    }

    #[test]
    fn test_case_07_missing_git_diff() {
        let run = parse_goal_run(&fixture_dir("case_07_missing_context_diff")).unwrap();
        let findings = analyze_goal_run(&run);
        assert!(
            findings.iter().any(|f| f.dimension == Dimension::Context && f.evidence.contains("git_diff_lines=0")),
            "must flag missing git diff"
        );
    }

    // ── Healthy cases (no false positives) ──

    #[test]
    fn test_case_01_healthy_no_findings() {
        let run = parse_goal_run(&fixture_dir("case_01_healthy")).unwrap();
        let findings = analyze_goal_run(&run);
        let actionable: Vec<_> = findings.iter().filter(|f| f.severity != Severity::Low).collect();
        assert!(actionable.is_empty(), "healthy case should produce no High/Medium findings, got: {:?}", actionable);
    }

    #[test]
    fn test_case_10_hard_won_no_false_deadloop() {
        // case_10 had a repetitive hit but SUCCEEDED — should not be flagged as deadloop.
        let run = parse_goal_run(&fixture_dir("case_10_hard_won_success")).unwrap();
        let findings = analyze_goal_run(&run);
        let deadloop: Vec<_> = findings.iter().filter(|f| f.dimension == Dimension::Deadloop).collect();
        assert!(deadloop.is_empty(), "hard-won success should NOT be flagged as deadloop (repetitive guard worked correctly)");
    }

    // ── run_once (directory scan) ──

    #[test]
    fn test_run_once_single_dir() {
        let dir = fixture_dir("case_02_deadloop_strict_check");
        let report = run_once(dir.to_str().unwrap()).unwrap();
        assert_eq!(report.analyzed_goals, 1);
        assert!(!report.issues_planned.is_empty());
    }

    #[test]
    fn test_run_once_all_fixtures() {
        let fixtures_root = env!("CARGO_MANIFEST_DIR").to_string() + "/tests/fixtures/goal-runs";
        let report = run_once(&fixtures_root).unwrap();
        assert_eq!(report.analyzed_goals, 10, "should find all 10 fixture cases");
        assert!(!report.issues_planned.is_empty(), "should have planned issues from problem cases");
        // Healthy cases (01, 10) should not contribute issues.
        // 8 problem cases → at least 8 issues (one each minimum).
        assert!(report.issues_planned.len() >= 6, "should plan at least 6 issues from 8 problem cases, got {}", report.issues_planned.len());
    }

    // ── parse robustness ──

    #[test]
    fn test_parse_handles_missing_fields_gracefully() {
        let json = serde_json::json!({"iter": 1, "goal_id": "g", "objective": "o", "all_passed": true});
        let rec = parse_iteration(&json);
        assert_eq!(rec.iter, 1);
        assert!(!rec.guards_hit.repetitive);
        assert!(rec.llm_calls.is_empty());
    }

    #[test]
    fn test_parse_goal_run_missing_dir_errors() {
        let result = parse_goal_run(Path::new("/nonexistent/goal-run-xyz"));
        assert!(result.is_err());
    }
}
