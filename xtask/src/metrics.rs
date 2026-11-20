//! The charter's three measures as one read-only report: `cargo xtask metrics`.
//!
//! ADR-0110 names the numbers that decide whether the refactoring worked: the
//! median blocking gate rounds for a ≤300-line change, CI wall time (the
//! `Test` job's 6-minute target), and the `fix` commit share with every module
//! over 2,000 production lines. This command prints them from `gh` and the
//! local tree only — no service and no token beyond the `gh` login the repo
//! already uses — and it writes nothing.
//!
//! Sources: a gate round is a comment whose last non-blank line carries the
//! gate's `CODEX_REVIEW_VERDICT` marker (the same completeness rule as
//! `.github/workflows/codex-review.yml`'s `codex_verdict_of_line`), and a
//! `FAIL` is a blocking round. The PR's `<!-- gate-dossier -->` comment is the
//! human record that count should agree with; its free-form shape is not
//! parsed, and a dossier that quotes a verdict is not counted as a round. CI
//! times are the Actions API's per-job `started_at`/`completed_at`; commit
//! types come from `git log`; module sizes are the ratchet's own
//! production-line count, so this report and `check-structure` measure the same
//! thing.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::TYPES;
use crate::structure_guard;

/// The charter's diff size bound for the round metric, in changed lines.
const DIFF_CAP: u64 = 300;

/// How many merged PRs to fetch before ordering them by merge date. The report
/// samples `--prs` *after* that ordering, so a long-lived PR that merged late
/// is not skipped because newer-created PRs filled the API page. Hitting the
/// ceiling is an error, never a silently truncated window.
const MERGED_FETCH_LIMIT: usize = 2_000;

/// The `Test` matrix job and the step that proves its work actually ran: a
/// docs-only/draft/release run dispatches the job with every step skipped, so
/// a job's timestamps alone do not make it a code run.
const TEST_JOB_PREFIX: &str = "Test (";
const TEST_STEP: &str = "cargo test --workspace --locked";

/// A module over this many production lines is the growth metric's target.
const MODULE_CAP: usize = 2_000;

/// The marker every gate review comment ends with.
const VERDICT_MARKER: &str = "CODEX_REVIEW_VERDICT:";

/// The first line of the human-maintained record, which may quote a verdict
/// without being one.
const DOSSIER_MARKER: &str = "<!-- gate-dossier -->";

const USAGE: &str = "\
usage: cargo xtask metrics [--prs N] [--runs N] [--since DATE]

  --prs N      merged PRs to sample for gate rounds (default 30)
  --runs N     completed CI runs to sample for wall time (default 20)
  --since D    git date for the commit window (default \"90 days ago\")

Read-only: it reads `gh` and the local tree and writes nothing.
Documented in CONTRIBUTING.md, \"Measuring the charter\".";

struct Options {
    prs: usize,
    runs: usize,
    since: String,
}

/// The parsed one-line verdict of a gate review comment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    Pass,
    Fail,
}

/// One sampled PR's gate history.
struct GateSample {
    number: u64,
    title: String,
    lines: u64,
    rounds: usize,
    blocking_rounds: usize,
}

/// The gate-round metric over the sampled window.
struct GateReport {
    samples: Vec<GateSample>,
    oversized: usize,
    ungated: usize,
    skipped: usize,
    /// The merge dates of the sampled window, as `YYYY-MM-DD`.
    window: Option<(String, String)>,
}

/// One completed CI run, as the run list reports it.
#[derive(Deserialize)]
struct RunRow {
    id: u64,
    conclusion: Option<String>,
    /// Whole-run wall time, computed by `gh --jq` from `createdAt`/`updatedAt`.
    seconds: i64,
}

/// One job of a CI run; `steps` tells an executed job from a dispatched one
/// whose steps were all skipped.
#[derive(Deserialize)]
struct JobRow {
    name: String,
    conclusion: Option<String>,
    seconds: i64,
    #[serde(default)]
    steps: Vec<StepRow>,
}

#[derive(Deserialize)]
struct StepRow {
    name: String,
    conclusion: Option<String>,
}

/// The CI wall-time metric over the sampled window.
struct WallReport {
    runs: usize,
    overall: Vec<i64>,
    jobs: BTreeMap<String, Vec<i64>>,
    cancelled: usize,
    no_code: usize,
}

/// One merged PR as `gh pr list` reports it.
#[derive(Deserialize)]
struct PrRow {
    number: u64,
    title: String,
    additions: u64,
    deletions: u64,
    #[serde(rename = "headRefName")]
    head_ref: String,
    #[serde(rename = "mergedAt")]
    merged_at: String,
    author: PrAuthor,
}

#[derive(Deserialize)]
struct PrAuthor {
    login: String,
}

#[derive(Deserialize)]
struct PrComments {
    comments: Vec<CommentBody>,
}

#[derive(Deserialize)]
struct CommentBody {
    body: String,
}

pub(crate) fn cli() {
    let options = options_from_args();
    if let Err(error) = report(&options) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn options_from_args() -> Options {
    let mut options = Options {
        prs: 30,
        runs: 20,
        since: "90 days ago".to_string(),
    };
    let mut args = std::env::args().skip(2);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--prs" => options.prs = whole_number(args.next(), "--prs"),
            "--runs" => options.runs = whole_number(args.next(), "--runs"),
            "--since" => options.since = args.next().unwrap_or_else(|| usage("--since needs a date")),
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => usage(&format!("unknown argument: {other}")),
        }
    }
    options
}

fn whole_number(value: Option<String>, flag: &str) -> usize {
    value
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| usage(&format!("{flag} needs a whole number")))
}

fn usage(error: &str) -> ! {
    eprintln!("error: {error}\n\n{USAGE}");
    std::process::exit(1);
}

fn report(options: &Options) -> Result<(), String> {
    let root = crate::repo_root();
    let gates = gate_report(options.prs)?;
    let wall = ci_wall(options.runs)?;
    let (fix, feat, total) = commit_mix(&root, &options.since)?;
    let modules = oversized(structure_guard::module_sizes(&root)?);

    println!(
        "cola metrics — read-only, from `gh` and the local tree at {}",
        head_short(&root).unwrap_or_else(|| "HEAD".to_string())
    );
    print_gates(&gates, options.prs);
    print_wall(&wall, options.runs);
    print_growth(options, fix, feat, total, &modules);
    Ok(())
}

// ---------------------------------------------------------------------------
// 1. Gate blocking rounds
// ---------------------------------------------------------------------------

fn gate_report(limit: usize) -> Result<GateReport, String> {
    let args = [
        "pr".to_string(),
        "list".to_string(),
        "--state".to_string(),
        "merged".to_string(),
        "--limit".to_string(),
        MERGED_FETCH_LIMIT.to_string(),
        "--json".to_string(),
        "number,title,additions,deletions,headRefName,mergedAt,author".to_string(),
    ];
    let fetched: Vec<PrRow> = parse_json(&gh(&args)?, "the merged PR list")?;
    if fetched.len() >= MERGED_FETCH_LIMIT {
        return Err(format!(
            "error: the merged-PR fetch hit its {MERGED_FETCH_LIMIT}-PR ceiling — raise \
             MERGED_FETCH_LIMIT so the window is not silently truncated"
        ));
    }
    let merged = newest_merged(fetched, limit);
    let window = merged
        .first()
        .zip(merged.last())
        .map(|(newest, oldest)| (day(&newest.merged_at), day(&oldest.merged_at)));

    let mut report = GateReport {
        samples: Vec::new(),
        oversized: 0,
        ungated: 0,
        skipped: 0,
        window,
    };
    for pr in merged {
        // Release cuts skip the gate; bot PRs get no repository secrets.
        if pr.head_ref.starts_with("release/") || pr.author.login.contains("dependabot") {
            report.skipped += 1;
            continue;
        }
        let lines = pr.additions + pr.deletions;
        if lines > DIFF_CAP {
            report.oversized += 1;
            continue;
        }
        let bodies = pr_comment_bodies(pr.number)?;
        let mut rounds = 0;
        let mut blocking_rounds = 0;
        for body in bodies {
            let Some(verdict) = gate_round(&body) else {
                continue;
            };
            rounds += 1;
            if verdict == Verdict::Fail {
                blocking_rounds += 1;
            }
        }
        if rounds == 0 {
            report.ungated += 1;
            continue;
        }
        report.samples.push(GateSample {
            number: pr.number,
            title: pr.title,
            lines,
            rounds,
            blocking_rounds,
        });
    }
    Ok(report)
}

/// The most recently merged `limit` PRs, newest first.
fn newest_merged(mut prs: Vec<PrRow>, limit: usize) -> Vec<PrRow> {
    prs.sort_by(|left, right| right.merged_at.cmp(&left.merged_at));
    prs.truncate(limit);
    prs
}

fn pr_comment_bodies(number: u64) -> Result<Vec<String>, String> {
    let args = [
        "pr".to_string(),
        "view".to_string(),
        number.to_string(),
        "--json".to_string(),
        "comments".to_string(),
    ];
    let comments: PrComments = parse_json(&gh(&args)?, "the PR comments")?;
    Ok(comments.comments.into_iter().map(|c| c.body).collect())
}

/// A gate review round's verdict; the dossier is not a round even when it
/// quotes one.
fn gate_round(body: &str) -> Option<Verdict> {
    if body.trim_start().starts_with(DOSSIER_MARKER) {
        return None;
    }
    verdict(body)
}

/// The verdict a gate review comment posts, if it posts one.
///
/// The gate's own completeness rule: the **last non-blank line** must end with
/// the marker, markdown emphasis around the value is tolerated, and prose may
/// precede it on the same line — but nothing after the value does. This is
/// `codex_verdict_of_line` in `.github/workflows/codex-review.yml`; a looser
/// parse would count a quoted verdict or a human discussion comment as a round.
fn verdict(body: &str) -> Option<Verdict> {
    let line = body.lines().rev().find(|line| !line.trim().is_empty())?;
    for (index, _) in line.rmatch_indices(VERDICT_MARKER) {
        // The gate's sed spells `CODEX_REVIEW_VERDICT: (PASS|FAIL)` — the one
        // space is literal, so `CODEX_REVIEW_VERDICT:PASS` is not a verdict.
        let value = line[index + VERDICT_MARKER.len()..]
            .strip_prefix(' ')
            .unwrap_or_default()
            .trim_end_matches(|c: char| matches!(c, '*' | '`') || c.is_whitespace());
        match value {
            "PASS" => return Some(Verdict::Pass),
            "FAIL" => return Some(Verdict::Fail),
            _ => {}
        }
    }
    None
}

fn print_gates(report: &GateReport, requested: usize) {
    match &report.window {
        Some((newest, oldest)) => println!(
            "\nGate rounds — last {requested} merged PRs ({oldest} … {newest}), diff ≤ {DIFF_CAP} \
             lines"
        ),
        None => println!("\nGate rounds — last {requested} merged PRs, diff ≤ {DIFF_CAP} lines"),
    }
    if report.samples.is_empty() {
        println!("  no sampled PR carries a gate verdict");
    } else {
        println!("  {:<7} {:>6} {:>7} {:>10}", "PR", "lines", "rounds", "blocking");
        for sample in &report.samples {
            println!(
                "  {:<7} {:>6} {:>7} {:>10}   {}",
                format!("#{}", sample.number),
                sample.lines,
                sample.rounds,
                sample.blocking_rounds,
                sample.title
            );
        }
        let rounds: Vec<f64> = report
            .samples
            .iter()
            .map(|sample| sample.blocking_rounds as f64)
            .collect();
        if let Some(median) = median(&rounds) {
            println!(
                "  median blocking rounds: {median:.1} over {} PR(s) — target ≤ 1",
                report.samples.len()
            );
        }
    }
    println!(
        "  excluded: {} over the cap, {} without a gate verdict, {} release/dependabot",
        report.oversized, report.ungated, report.skipped
    );
}

// ---------------------------------------------------------------------------
// 2. CI wall time per job
// ---------------------------------------------------------------------------

fn ci_wall(limit: usize) -> Result<WallReport, String> {
    let jq = "[.[] | {id: .databaseId, conclusion, \
              seconds: ((.updatedAt|fromdateiso8601) - (.createdAt|fromdateiso8601))}]";
    let args = [
        "run".to_string(),
        "list".to_string(),
        "--workflow".to_string(),
        "CI".to_string(),
        "--status".to_string(),
        "completed".to_string(),
        "--limit".to_string(),
        limit.to_string(),
        "--json".to_string(),
        "databaseId,conclusion,createdAt,updatedAt".to_string(),
        "--jq".to_string(),
        jq.to_string(),
    ];
    let runs: Vec<RunRow> = parse_json(&gh(&args)?, "the CI run list")?;
    let mut report = WallReport {
        runs: 0,
        overall: Vec::new(),
        jobs: BTreeMap::new(),
        cancelled: 0,
        no_code: 0,
    };
    for run in runs {
        if run.conclusion.as_deref() == Some("cancelled") {
            report.cancelled += 1;
            continue;
        }
        let jobs = run_jobs(run.id)?;
        // A docs-only/draft/release run still dispatches the `Test` matrix with
        // every step skipped; only an executed test step makes it a code run
        // whose wall the charter's 6-minute target measures.
        if !executed_test_leg(&jobs) {
            report.no_code += 1;
            continue;
        }
        report.runs += 1;
        report.overall.push(run.seconds);
        for job in jobs {
            if job.conclusion.as_deref() == Some("cancelled") {
                continue;
            }
            report.jobs.entry(job.name).or_default().push(job.seconds);
        }
    }
    Ok(report)
}

fn executed_test_leg(jobs: &[JobRow]) -> bool {
    jobs.iter().any(|job| {
        job.name.starts_with(TEST_JOB_PREFIX)
            && job.steps.iter().any(|step| {
                step.name.strip_prefix("Run ") == Some(TEST_STEP)
                    && step.conclusion.as_deref() != Some("skipped")
            })
    })
}

fn run_jobs(run: u64) -> Result<Vec<JobRow>, String> {
    let jq = "[.jobs[] | select(.started_at != null and .completed_at != null \
              and .conclusion != \"skipped\") \
              | {name, conclusion, steps: [.steps[] | {name, conclusion}], \
                 seconds: ((.completed_at|fromdateiso8601) - (.started_at|fromdateiso8601))}]";
    let endpoint = format!("repos/{{owner}}/{{repo}}/actions/runs/{run}/jobs?per_page=100");
    let args = ["api".to_string(), endpoint, "--jq".to_string(), jq.to_string()];
    parse_json(&gh(&args)?, "the CI run's jobs")
}

fn print_wall(report: &WallReport, requested: usize) {
    println!("\nCI wall — last {requested} completed CI runs");
    if let Some(median) = median(&report.overall.iter().map(|s| *s as f64).collect::<Vec<_>>()) {
        let max = report.overall.iter().copied().max().unwrap_or_default();
        println!(
            "  overall wall (queue included): median {} · max {} over {} code run(s) — target ≤ 6m",
            duration(median),
            duration(max as f64),
            report.runs
        );
    } else {
        println!("  no sampled run executed a code job");
    }
    let test_legs: Vec<String> = report
        .jobs
        .iter()
        .filter(|(name, _)| name.starts_with(TEST_JOB_PREFIX))
        .filter_map(|(name, samples)| {
            median(&samples.iter().map(|s| *s as f64).collect::<Vec<_>>())
                .map(|median| format!("{name} {}", duration(median)))
        })
        .collect();
    if !test_legs.is_empty() {
        println!("  Test legs: {} — target ≤ 6m each", test_legs.join(" · "));
    }
    if !report.jobs.is_empty() {
        println!("  {:<30} {:>5} {:>10} {:>10}", "job", "runs", "median", "max");
        for (name, samples) in &report.jobs {
            let values: Vec<f64> = samples.iter().map(|s| *s as f64).collect();
            let Some(median) = median(&values) else {
                continue;
            };
            let max = samples.iter().copied().max().unwrap_or_default();
            println!(
                "  {:<30} {:>5} {:>10} {:>10}",
                name,
                samples.len(),
                duration(median),
                duration(max as f64)
            );
        }
    }
    println!(
        "  excluded: {} cancelled, {} without an executed Test leg",
        report.cancelled, report.no_code
    );
}

// ---------------------------------------------------------------------------
// 3. Commit mix and module growth
// ---------------------------------------------------------------------------

/// The tree the local measures describe, so a recorded baseline names its
/// commit. Best-effort: the report still runs outside a git checkout.
fn head_short(root: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn commit_mix(root: &Path, since: &str) -> Result<(usize, usize, usize), String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["log", "--since", since, "--pretty=format:%s"])
        .output()
        .map_err(|error| format!("error: cannot run git: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "error: `git log` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut fix = 0;
    let mut feat = 0;
    let mut total = 0;
    for subject in text.lines().filter(|line| !line.trim().is_empty()) {
        total += 1;
        match commit_type(subject) {
            Some("fix") => fix += 1,
            Some("feat") => feat += 1,
            _ => {}
        }
    }
    Ok((fix, feat, total))
}

/// The date half of an ISO timestamp (`2026-11-20T16:01:15Z` → `2026-11-20`).
fn day(timestamp: &str) -> String {
    timestamp
        .split_once('T')
        .map_or(timestamp, |(day, _)| day)
        .to_string()
}

/// A commit subject's Conventional Commit type, if it has one.
fn commit_type(subject: &str) -> Option<&str> {
    let end = subject.find(['(', '!', ':'])?;
    let kind = &subject[..end];
    TYPES.contains(&kind).then_some(kind)
}

/// The production modules over the cap, largest first.
fn oversized(mut sizes: Vec<structure_guard::FileSize>) -> Vec<structure_guard::FileSize> {
    sizes.retain(|file| file.lines > MODULE_CAP);
    sizes.sort_by(|left, right| {
        right
            .lines
            .cmp(&left.lines)
            .then_with(|| left.path.cmp(&right.path))
    });
    sizes
}

fn print_growth(
    options: &Options,
    fix: usize,
    feat: usize,
    total: usize,
    modules: &[structure_guard::FileSize],
) {
    println!("\nCommit mix — since \"{}\"", options.since);
    println!(
        "  fix {} ({} of {total}) · feat {} ({}) · other {}",
        fix,
        share(fix, total),
        feat,
        share(feat, total),
        total.saturating_sub(fix + feat)
    );
    println!("\nModules over {MODULE_CAP} production lines: {}", modules.len());
    for module in modules {
        println!("  {:>5}  {}", module.lines, module.path);
    }
}

fn share(part: usize, total: usize) -> String {
    if total == 0 {
        return "0.0%".to_string();
    }
    format!("{:.1}%", part as f64 * 100.0 / total as f64)
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// The median of a non-empty sample; an even sample averages its middle pair.
fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|left, right| left.total_cmp(right));
    let middle = sorted.len() / 2;
    Some(if sorted.len().is_multiple_of(2) {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    } else {
        sorted[middle]
    })
}

fn duration(seconds: f64) -> String {
    let seconds = seconds.round().max(0.0) as i64;
    format!("{}m {:02}s", seconds / 60, seconds % 60)
}

fn gh(args: &[String]) -> Result<String, String> {
    let output = Command::new("gh").args(args).output().map_err(|error| {
        format!("error: cannot run gh ({error}) — install the GitHub CLI and run `gh auth login`")
    })?;
    if !output.status.success() {
        return Err(format!(
            "error: `gh {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn parse_json<T: DeserializeOwned>(text: &str, what: &str) -> Result<T, String> {
    serde_json::from_str(text).map_err(|error| format!("error: cannot read {what}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_verdict_is_read_from_the_last_non_blank_line() {
        for (body, want) in [
            (
                "## Codex review\n\nCODEX_REVIEW_VERDICT: FAIL\n",
                Some(Verdict::Fail),
            ),
            (
                "## Codex review\n\nCODEX_REVIEW_VERDICT: PASS\n\n\n",
                Some(Verdict::Pass),
            ),
            ("body\r\nCODEX_REVIEW_VERDICT: PASS\r\n", Some(Verdict::Pass)),
            (
                "closing note. **CODEX_REVIEW_VERDICT: FAIL**",
                Some(Verdict::Fail),
            ),
            ("closing note. `CODEX_REVIEW_VERDICT: PASS`", Some(Verdict::Pass)),
        ] {
            assert_eq!(verdict(body), want, "body: {body:?}");
        }
    }

    #[test]
    fn a_verdict_is_taken_from_the_closing_line_not_an_earlier_quote() {
        assert_eq!(
            verdict("a quoted CODEX_REVIEW_VERDICT: FAIL above\nCODEX_REVIEW_VERDICT: PASS"),
            Some(Verdict::Pass)
        );
    }

    #[test]
    fn a_body_without_a_complete_verdict_line_is_not_a_round() {
        for body in [
            "## Codex review\n\nno marker here\n",
            "CODEX_REVIEW_VERDICT: MAYBE",
            "CODEX_REVIEW_VERDICT: PASS and then prose",
            "CODEX_REVIEW_VERDICT: PASS.",
            "CODEX_REVIEW_VERDICT:PASS",
            "",
        ] {
            assert_eq!(verdict(body), None, "body: {body:?}");
        }
    }

    #[test]
    fn a_dossier_quoting_a_verdict_is_not_a_gate_round() {
        assert_eq!(
            gate_round("<!-- gate-dossier -->\nCODEX_REVIEW_VERDICT: PASS"),
            None
        );
    }

    #[test]
    fn an_even_sample_median_averages_the_middle_pair() {
        assert_eq!(median(&[3.0, 0.0, 1.0, 2.0]), Some(1.5));
    }

    #[test]
    fn an_odd_sample_median_is_its_middle_value() {
        assert_eq!(median(&[3.0, 0.0, 1.0]), Some(1.0));
    }

    #[test]
    fn an_empty_sample_has_no_median() {
        assert_eq!(median(&[]), None);
    }

    #[test]
    fn a_commit_subject_yields_its_conventional_type() {
        assert_eq!(commit_type("fix(bridge): correct a bug"), Some("fix"));
        assert_eq!(commit_type("feat!: break the API"), Some("feat"));
        assert_eq!(commit_type("docs: reword"), Some("docs"));
        assert_eq!(commit_type("add a thing"), None);
    }

    #[test]
    fn the_code_run_detector_matches_the_ci_workflow() {
        let workflow = std::fs::read_to_string(crate::repo_root().join(".github/workflows/ci.yml"))
            .expect("ci.yml exists");
        assert!(
            workflow.contains("name: Test (${{ matrix.os }})")
                && workflow.contains(&format!("run: {TEST_STEP}")),
            "the code-run detector keys on the `Test (` job and its `{TEST_STEP}` step"
        );
    }

    #[test]
    fn a_dispatched_test_job_with_every_step_skipped_is_not_a_code_run() {
        let job = JobRow {
            name: "Test (macos-latest)".to_string(),
            conclusion: Some("success".to_string()),
            seconds: 5,
            steps: vec![
                StepRow {
                    name: "Set up job".to_string(),
                    conclusion: Some("success".to_string()),
                },
                StepRow {
                    name: format!("Run {TEST_STEP}"),
                    conclusion: Some("skipped".to_string()),
                },
            ],
        };
        assert!(!executed_test_leg(&[job]));
    }

    #[test]
    fn an_executed_test_step_makes_the_run_a_code_run() {
        let job = JobRow {
            name: "Test (macos-latest)".to_string(),
            conclusion: Some("failure".to_string()),
            seconds: 300,
            steps: vec![StepRow {
                name: format!("Run {TEST_STEP}"),
                conclusion: Some("failure".to_string()),
            }],
        };
        assert!(executed_test_leg(&[job]));
    }

    #[test]
    fn the_newest_merged_prs_are_selected_by_merge_date() {
        let prs = vec![
            pr(1, "2026-11-01T00:00:00Z"),
            pr(3, "2026-11-20T00:00:00Z"),
            pr(2, "2026-11-10T00:00:00Z"),
        ];
        let selected: Vec<u64> = newest_merged(prs, 2).iter().map(|pr| pr.number).collect();
        assert_eq!(selected, vec![3, 2]);
    }

    #[test]
    fn module_sizes_counts_production_lines_only() {
        let root = std::env::temp_dir().join(format!("cola-metrics-{}", std::process::id()));
        let src = root.join("src");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&src).expect("temp src");
        std::fs::write(
            src.join("main.rs"),
            "fn main() {}\nlet a = 1;\n#[cfg(test)]\nmod tests {\n    fn helper() {}\n}\n",
        )
        .expect("temp source");
        let sizes = structure_guard::module_sizes(&root).expect("scan");
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(sizes.iter().map(|file| file.lines).sum::<usize>(), 2, "{sizes:?}");
    }

    fn pr(number: u64, merged_at: &str) -> PrRow {
        PrRow {
            number,
            title: String::new(),
            additions: 0,
            deletions: 0,
            head_ref: String::new(),
            merged_at: merged_at.to_string(),
            author: PrAuthor { login: String::new() },
        }
    }

    #[test]
    fn modules_above_the_cap_are_listed_largest_first() {
        let sizes = vec![
            structure_guard::FileSize {
                path: "src/small.rs".to_string(),
                lines: MODULE_CAP,
            },
            structure_guard::FileSize {
                path: "src/large.rs".to_string(),
                lines: 4_821,
            },
            structure_guard::FileSize {
                path: "src/medium.rs".to_string(),
                lines: 2_500,
            },
        ];
        let listed: Vec<String> = oversized(sizes).into_iter().map(|file| file.path).collect();
        assert_eq!(listed, vec!["src/large.rs", "src/medium.rs"]);
    }
}
