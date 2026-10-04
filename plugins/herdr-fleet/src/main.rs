use anyhow::{bail, Context as _, Result};
use clap::Parser;
use serde::{Deserialize, Serialize, Serializer};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::Duration;

const PLUGIN_ID: &str = "peteretelej.herdr-fleet";
const MAX_WORKERS: usize = 8;

/// Democratized subagents for herdr: dispatch kind-agnostic consultation
/// workers as herdr panes and collect their answers.
#[derive(Parser)]
#[command(name = "herdr-fleet", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Spawn N workers with one brief; collect each answer as a file.
    Dispatch {
        /// The brief every worker receives
        #[arg(long)]
        brief: String,
        /// Worker agent kind (a herdr agent kind, e.g. opencode, claude, codex)
        #[arg(long, default_value = "opencode")]
        kind: String,
        /// How many workers to spawn (1-8)
        #[arg(long, default_value_t = 3)]
        count: usize,
        /// Working directory for the workers (default: the current directory)
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Per-worker settle timeout in milliseconds
        #[arg(long, default_value_t = 600_000)]
        timeout: u64,
        /// Fallback transcript lines for workers that wrote no result file
        #[arg(long, default_value_t = 200)]
        lines: usize,
        /// Outcomes only; skip result collection
        #[arg(long)]
        no_collect: bool,
        /// Print the run record as JSON instead of the human summary
        #[arg(long)]
        json: bool,
        /// Run-state root override (default: herdr's plugin state dir)
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Show per-worker state of a run (default: the latest).
    Status {
        /// Run stamp (the runs/ directory name); default: latest
        #[arg(long)]
        run: Option<String>,
        /// Run-state root override (default: herdr's plugin state dir)
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Print the summary of the most recent run.
    Last {
        /// Run-state root override (default: herdr's plugin state dir)
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
}

#[derive(Deserialize, Clone)]
struct WorkspaceCreated {
    #[serde(default)]
    workspace: WorkspaceRef,
    #[serde(default)]
    root_pane: PaneRef,
}

#[derive(Deserialize, Default, Clone)]
struct WorkspaceRef {
    #[serde(default)]
    workspace_id: String,
}

#[derive(Deserialize, Default, Clone)]
struct PaneRef {
    #[serde(default)]
    pane_id: String,
}

#[derive(Deserialize, Clone)]
struct TabCreated {
    #[serde(default)]
    root_pane: PaneRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Completed,
    /// Needs attention: the worker settled blocked after the brief was
    /// delivered (the pre-start equivalents surface as Failed/Skipped).
    Blocked,
    /// No activity within 5s of submission; delivery unconfirmed.
    Stalled,
    Timeout,
    Failed,
    /// Not started: the worker pane never became ready.
    Skipped,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Blocked => "blocked",
            Self::Stalled => "stalled",
            Self::Timeout => "timeout",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

impl Serialize for Outcome {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Outcome {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "completed" => Ok(Self::Completed),
            "blocked" => Ok(Self::Blocked),
            "stalled" => Ok(Self::Stalled),
            "timeout" => Ok(Self::Timeout),
            "failed" => Ok(Self::Failed),
            "skipped" => Ok(Self::Skipped),
            other => Err(serde::de::Error::unknown_variant(
                other,
                &["completed", "blocked", "stalled", "timeout", "failed", "skipped"],
            )),
        }
    }
}

#[derive(Serialize, Clone, Deserialize)]
struct WorkerResult {
    /// Worker agent name
    target: String,
    name: Option<String>,
    agent_kind: Option<String>,
    workspace_id: String,
    status_before: String,
    outcome: Outcome,
    detail: String,
    /// The brief was submitted to the worker (not merely attempted).
    submitted: bool,
    answer_lines: usize,
    answer_file: Option<String>,
}

#[derive(Serialize)]
struct RunRecord {
    timestamp: String,
    kind: String,
    brief: String,
    workspace: String,
    timeout_ms: u64,
    collect_lines: usize,
    results: Vec<WorkerResult>,
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Dispatch {
            brief,
            kind,
            count,
            cwd,
            timeout,
            lines,
            no_collect,
            json,
            state_dir,
        } => dispatch(DispatchArgs {
            brief,
            kind,
            count,
            cwd,
            timeout,
            lines,
            no_collect,
            json,
            state_dir,
        }),
        Cmd::Status { run, state_dir } => status(run.as_deref(), state_dir.as_deref()),
        Cmd::Last { state_dir } => last(state_dir.as_deref()),
    }
}

struct DispatchArgs {
    brief: String,
    kind: String,
    count: usize,
    cwd: Option<PathBuf>,
    timeout: u64,
    lines: usize,
    no_collect: bool,
    json: bool,
    state_dir: Option<PathBuf>,
}

fn dispatch(args: DispatchArgs) -> Result<()> {
    let brief = args.brief.trim().to_string();
    if brief.is_empty() {
        bail!("brief is empty");
    }
    if args.count == 0 || args.count > MAX_WORKERS {
        bail!("--count must be between 1 and {MAX_WORKERS}");
    }
    if args.lines == 0 && !args.no_collect {
        bail!("--lines must be at least 1 (or pass --no-collect)");
    }
    let cwd = match &args.cwd {
        Some(dir) => std::fs::canonicalize(dir)
            .with_context(|| format!("--cwd {} does not exist", dir.display()))?,
        None => std::env::current_dir().context("no current directory; pass --cwd")?,
    };

    let stamp = run_stamp();
    let run_dir = state_root(args.state_dir.as_deref())?
        .join("runs")
        .join(&stamp);
    let answers = run_dir.join("answers");
    std::fs::create_dir_all(&answers).with_context(|| format!("creating {}", answers.display()))?;
    let answers = std::fs::canonicalize(&answers)?;

    // One workspace per run; worker 1 lives in its root pane, workers 2..N in
    // their own tabs. Herdr owns everything from here on - no teardown.
    let label = format!("fleet-{}", &stamp[9..]);
    let cwd_arg = cwd.to_string_lossy().to_string();
    let label_arg = label.clone();
    let ws_response = herdr_json(&[
        "workspace", "create", "--cwd", &cwd_arg, "--label", &label_arg, "--no-focus",
    ])?;
    let ws: WorkspaceCreated = serde_json::from_value(ws_response["result"].clone())?;
    let workspace_id = ws.workspace.workspace_id.clone();
    let mut panes = vec![ws.root_pane.pane_id.clone()];
    for _ in 1..args.count {
        let tab_response = herdr_json(&[
            "tab", "create", "--workspace", &workspace_id, "--cwd", &cwd_arg, "--label",
            &label_arg, "--no-focus",
        ])?;
        let tab: TabCreated = serde_json::from_value(tab_response["result"].clone())?;
        panes.push(tab.root_pane.pane_id.clone());
    }

    // One thread per worker: start (waits for readiness) -> prompt -> collect.
    let mut results: Vec<WorkerResult> = thread::scope(|scope| {
        let handles: Vec<_> = panes
            .iter()
            .enumerate()
            .map(|(i, pane_id)| {
                let job = WorkerJob {
                    worker: worker_name(i + 1, &stamp),
                    kind: args.kind.clone(),
                    pane_id: pane_id.clone(),
                    workspace_id: workspace_id.clone(),
                    brief: brief.clone(),
                    timeout: args.timeout.to_string(),
                    lines: args.lines,
                    no_collect: args.no_collect,
                    answers: answers.clone(),
                };
                scope.spawn(move || run_worker(&job))
            })
            .collect();
        handles
            .into_iter()
            .zip(panes.iter())
            .map(|(handle, pane_id)| {
                handle.join().unwrap_or_else(|_| WorkerResult {
                    target: pane_id.clone(),
                    name: None,
                    agent_kind: None,
                    workspace_id: workspace_id.clone(),
                    status_before: "unknown".into(),
                    outcome: Outcome::Failed,
                    detail: "internal error: worker thread panicked".into(),
                    submitted: false,
                    answer_lines: 0,
                    answer_file: None,
                })
            })
            .collect::<Vec<WorkerResult>>()
    });
    results.sort_by(|a, b| a.target.cmp(&b.target));

    let record = RunRecord {
        timestamp: stamp,
        kind: args.kind.clone(),
        brief,
        workspace: label,
        timeout_ms: args.timeout,
        collect_lines: if args.no_collect { 0 } else { args.lines },
        results,
    };

    let mut status = std::fs::File::create(run_dir.join("status.jsonl"))
        .with_context(|| format!("creating {}", run_dir.join("status.jsonl").display()))?;
    for result in &record.results {
        serde_json::to_writer(&mut status, result)?;
        status.write_all(b"\n")?;
    }
    std::fs::write(run_dir.join("summary.md"), summary_md(&record, &run_dir))?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&record)?);
    } else {
        print_run_summary(&record, &run_dir);
    }
    std::io::stdout().flush()?;
    if record.results.iter().any(|r| r.submitted) {
        Ok(())
    } else {
        bail!("no brief reached a worker");
    }
}

/// One fleet worker = one thread of run_worker: start (waits for readiness),
/// deliver the brief, wait, collect.
#[derive(Clone)]
struct WorkerJob {
    worker: String,
    kind: String,
    pane_id: String,
    workspace_id: String,
    brief: String,
    timeout: String,
    lines: usize,
    no_collect: bool,
    answers: PathBuf,
}

fn run_worker(job: &WorkerJob) -> WorkerResult {
    let WorkerJob {
        worker,
        kind,
        pane_id,
        workspace_id,
        brief,
        timeout,
        lines,
        no_collect,
        answers,
    } = job;
    let worker = worker.as_str();
    let kind = kind.as_str();
    let pane_id = pane_id.as_str();
    let workspace_id = workspace_id.as_str();
    let brief = brief.as_str();
    let timeout = timeout.as_str();
    let mut result = base_result(worker, kind);
    result.workspace_id = workspace_id.to_string();

    let start = herdr()
        .args(["agent", "start", worker, "--kind", kind, "--pane", pane_id])
        .output();
    let start = match start {
        Ok(out) => out,
        Err(err) => {
            result.detail = format!("failed to run herdr: {err}");
            return result;
        }
    };
    if !start.status.success() {
        let code = json_field(&start.stderr, "/error/code").unwrap_or_default();
        result.outcome = Outcome::Failed;
        result.detail = match code.as_str() {
            "" => format!("agent start failed ({})", start.status),
            other => format!("agent start failed: {other}"),
        };
        return result;
    }
    result.status_before = "idle".into();

    let prompt = worker_brief(brief, worker, answers);
    let out = herdr()
        .args(["agent", "prompt", worker, &prompt, "--wait", "--timeout", timeout])
        .output();
    let out = match out {
        Ok(out) => out,
        Err(err) => {
            result.detail = format!("failed to run herdr: {err}");
            return result;
        }
    };
    let (outcome, detail, submitted) = classify_prompt(&out);
    result.outcome = outcome;
    result.detail = detail;
    result.submitted = submitted;

    if !no_collect && result.outcome == Outcome::Completed {
        // Primary contract: the worker writes its own answer file.
        let result_file = answers.join(format!("{worker}.md"));
        match poll_file(&result_file, 15) {
            Some(()) => {
                let text = std::fs::read_to_string(&result_file).unwrap_or_default();
                result.answer_lines = text.lines().count();
                result.answer_file = Some(format!("answers/{}.md", worker));
            }
            // Fallback: the worker never wrote the file; page its transcript
            // instead (only safe while idle).
            None => match collect_transcript(worker, *lines, answers) {
                Ok((count, file)) => {
                    result.answer_lines = count;
                    result.answer_file = Some(file);
                    result
                        .detail
                        .push_str("; no result file, collected transcript tail");
                }
                Err(err) => result.detail.push_str(&format!("; collection failed: {err}")),
            },
        }
    }
    result
}

fn base_result(worker: &str, kind: &str) -> WorkerResult {
    WorkerResult {
        target: worker.to_string(),
        name: Some(worker.to_string()),
        agent_kind: Some(kind.to_string()),
        workspace_id: String::new(),
        status_before: "unstarted".into(),
        outcome: Outcome::Failed,
        detail: String::new(),
        submitted: false,
        answer_lines: 0,
        answer_file: None,
    }
}

/// Worker names must be unique among live agents and match
/// `[a-z][a-z0-9_-]{0,31}`; the stamp's millisecond digits (lowercase, fixed
/// width) keep concurrent runs apart.
fn worker_name(index: usize, stamp: &str) -> String {
    let digits = stamp.trim_end_matches('Z');
    let suffix = &digits[digits.len() - 5..];
    format!("fleet-{}-{}", index, suffix)
}

/// The brief plus the worker protocol: independent work, one result file.
fn worker_brief(brief: &str, worker: &str, answers: &Path) -> String {
    format!(
        "{brief}\n\n---\n\
        You are worker {worker} in a fleet of consultation workers. Work \
        independently; do not read other workers' files.\n\
        When your answer is complete, write it to exactly this path, then stop:\n\
        {}\n",
        answers.join(format!("{worker}.md")).display()
    )
}

/// Wait up to `seconds` for the worker's result file to appear.
fn poll_file(path: &Path, seconds: u64) -> Option<()> {
    for _ in 0..=seconds {
        if path.is_file() {
            return Some(());
        }
        thread::sleep(Duration::from_secs(1));
    }
    None
}

/// (outcome, detail, submitted) from one `agent prompt --wait` invocation.
/// The CLI prints a JSON error envelope to stderr with exit 1; success prints
/// the response with `.result.agent`.
fn classify_prompt(out: &Output) -> (Outcome, String, bool) {
    if out.status.success() {
        let status = json_field(&out.stdout, "/result/agent/agent_status")
            .unwrap_or_else(|| "unknown".into());
        if status == "blocked" {
            (
                Outcome::Blocked,
                "delivered; worker settled blocked and needs attention".into(),
                true,
            )
        } else {
            (Outcome::Completed, format!("settled {status}"), true)
        }
    } else {
        let code = json_field(&out.stderr, "/error/code").unwrap_or_default();
        match code.as_str() {
            "agent_blocked" => (
                Outcome::Blocked,
                "not sent: worker is blocked; resolve the pane first".into(),
                false,
            ),
            "agent_prompt_stalled" => (
                Outcome::Stalled,
                "no activity within 5s of submission; delivery unconfirmed - check the pane before re-sending".into(),
                true,
            ),
            "timeout" => (
                Outcome::Timeout,
                "no settled state within the timeout".into(),
                true,
            ),
            "" => (
                Outcome::Failed,
                format!("prompt failed ({})", out.status),
                false,
            ),
            other => (Outcome::Failed, format!("error code {other}"), false),
        }
    }
}

fn json_field(bytes: &[u8], pointer: &str) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()?
        .pointer(pointer)?
        .as_str()
        .map(String::from)
}

/// Fallback collection: page the worker's transcript tail. Only safe while
/// the worker is idle (alternate-screen paging is idle-gated).
fn collect_transcript(worker: &str, lines: usize, answers: &Path) -> Result<(usize, String)> {
    let lines_arg = lines.to_string();
    let out = herdr()
        .args([
            "agent",
            "read",
            worker,
            "--source",
            "recent-unwrapped",
            "--lines",
            &lines_arg,
        ])
        .output()
        .context("running herdr agent read")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let text = text.trim_end();
    let file = format!("{worker}-transcript.txt");
    std::fs::write(answers.join(&file), text)?;
    Ok((text.lines().count(), file))
}

fn herdr_json(args: &[&str]) -> Result<serde_json::Value> {
    let out = herdr()
        .args(args)
        .output()
        .with_context(|| format!("running herdr {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "herdr {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}

fn herdr() -> Command {
    Command::new(std::env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".into()))
}

/// Run-state root: --state-dir, else the manifest-provided state dir, else
/// herdr 0.9.3's own state location for bare shell runs
/// ($XDG_STATE_HOME/herdr or ~/.local/state/herdr, then plugins/<id>).
fn state_root(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(dir) = explicit {
        return Ok(dir.to_path_buf());
    }
    if let Some(dir) = std::env::var_os("HERDR_PLUGIN_STATE_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let base = match std::env::var_os("XDG_STATE_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = std::env::var_os("HOME").context("no HOME set; pass --state-dir")?;
            PathBuf::from(home).join(".local/state")
        }
    };
    Ok(base.join("herdr").join("plugins").join(PLUGIN_ID))
}

fn run_stamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let (year, month, day) = civil_from_days((now.as_secs() / 86_400) as i64);
    let rem = now.as_secs() % 86_400;
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}{:03}Z",
        year,
        month,
        day,
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60,
        now.subsec_millis()
    )
}

/// Days since 1970-01-01 to (year, month, day) in the proleptic Gregorian
/// calendar (Howard Hinnant's civil_from_days).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Markdown table cells break on pipes and newlines.
fn md_cell(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}

fn print_run_summary(record: &RunRecord, run_dir: &Path) {
    println!(
        "dispatched {} {} worker(s) in workspace '{}' - brief: {:?}",
        record.results.len(),
        record.kind,
        record.workspace,
        record.brief
    );
    let width = record
        .results
        .iter()
        .map(|r| r.target.len())
        .max()
        .unwrap_or(0)
        .max(1);
    for r in &record.results {
        let answer = r
            .answer_file
            .as_deref()
            .map(|f| format!("{} ({} lines)", f, r.answer_lines))
            .unwrap_or_default();
        println!(
            "  {t:<width$}  {:<9}  {}{}",
            r.outcome.as_str(),
            r.detail,
            if answer.is_empty() {
                String::new()
            } else {
                format!("  [{answer}]")
            },
            t = r.target
        );
    }
    println!("run dir: {}", run_dir.join("summary.md").display());
}

fn summary_md(record: &RunRecord, run_dir: &Path) -> String {
    let mut s = format!("# Fleet run {}\n\n", record.timestamp);
    s.push_str(&format!("- brief: {}\n", md_cell(&record.brief)));
    s.push_str(&format!(
        "- {} x {} workers in workspace '{}', timeout {} ms per worker, collection: {} lines\n\n",
        record.results.len(),
        record.kind,
        md_cell(&record.workspace),
        record.timeout_ms,
        record.collect_lines
    ));
    s.push_str("| worker | before | outcome | detail | answer |\n");
    s.push_str("|---|---|---|---|---|\n");
    for r in &record.results {
        let answer = r
            .answer_file
            .as_deref()
            .map(|f| format!("{} ({} lines)", f, r.answer_lines))
            .unwrap_or_else(|| "-".into());
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            md_cell(&r.target),
            md_cell(&r.status_before),
            r.outcome.as_str(),
            md_cell(&r.detail),
            md_cell(&answer),
        ));
    }
    for r in &record.results {
        let Some(file) = &r.answer_file else { continue };
        s.push_str(&format!("\n## {}\n\n", md_cell(&r.target)));
        if let Ok(text) = std::fs::read_to_string(run_dir.join(file)) {
            s.push_str(text.trim_end());
            s.push('\n');
        }
    }
    s
}

fn resolve_run_dir(runs: &Path, run: Option<&str>) -> Result<PathBuf> {
    match run {
        Some(stamp) => {
            let dir = runs.join(stamp);
            if !dir.is_dir() {
                bail!("no run '{}' under {}", stamp, runs.display());
            }
            Ok(dir)
        }
        None => match std::fs::read_dir(runs) {
            Ok(entries) => entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path().is_dir())
                .map(|entry| entry.file_name().to_string_lossy().to_string())
                .max()
                .map(|stamp| runs.join(stamp))
                .context("no fleet runs yet"),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Err(err).context("no fleet runs yet")
            }
            Err(err) => Err(err).context(format!("reading {}", runs.display())),
        },
    }
}

fn status(run: Option<&str>, state_dir: Option<&Path>) -> Result<()> {
    let run_dir = resolve_run_dir(&state_root(state_dir)?.join("runs"), run)?;
    let record_path = run_dir.join("status.jsonl");
    let text = std::fs::read_to_string(&record_path)
        .with_context(|| format!("reading {}", record_path.display()))?;
    println!("run: {}", run_dir.display());
    for line in text.lines() {
        let Ok(r) = serde_json::from_str::<WorkerResult>(line) else {
            continue;
        };
        let result_file = run_dir.join(format!("answers/{}.md", r.target));
        let result_state = if result_file.is_file() {
            "result file present"
        } else if r.answer_file.is_some() {
            "collected"
        } else {
            "-"
        };
        println!(
            "  {:<20} {:<9} {:<12} {}",
            r.target,
            r.outcome.as_str(),
            result_state,
            r.detail
        );
    }
    Ok(())
}

fn last(state_dir: Option<&Path>) -> Result<()> {
    let run_dir = resolve_run_dir(&state_root(state_dir)?.join("runs"), None)?;
    let summary = std::fs::read_to_string(run_dir.join("summary.md"))
        .with_context(|| format!("reading {}", run_dir.join("summary.md").display()))?;
    print!("{summary}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    fn output(exit: i32, stdout: &str, stderr: &str) -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(exit),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn stalled_means_submitted_but_unconfirmed() {
        let (outcome, detail, submitted) = classify_prompt(&output(
            1,
            "",
            r#"{"id":"cli:agent:prompt","error":{"code":"agent_prompt_stalled","message":"x"}}"#,
        ));
        assert_eq!(outcome, Outcome::Stalled);
        assert!(submitted);
        assert!(detail.contains("unconfirmed"));
    }

    #[test]
    fn blocked_error_was_not_sent() {
        let (outcome, _detail, submitted) = classify_prompt(&output(
            1,
            "",
            r#"{"id":"cli:agent:prompt","error":{"code":"agent_blocked","message":"x"}}"#,
        ));
        assert_eq!(outcome, Outcome::Blocked);
        assert!(!submitted);
    }

    #[test]
    fn timeout_counts_as_submitted() {
        let (outcome, _, submitted) = classify_prompt(&output(
            1,
            "",
            r#"{"id":"cli:agent:prompt","error":{"code":"timeout","message":"x"}}"#,
        ));
        assert_eq!(outcome, Outcome::Timeout);
        assert!(submitted);
    }

    #[test]
    fn settled_block_needs_attention_but_was_delivered() {
        let (outcome, detail, submitted) = classify_prompt(&output(
            0,
            r#"{"id":"cli:agent:prompt","result":{"agent":{"agent_status":"blocked"}}}"#,
            "",
        ));
        assert_eq!(outcome, Outcome::Blocked);
        assert!(submitted);
        assert!(detail.contains("delivered"));
    }

    #[test]
    fn settled_idle_completes() {
        let (outcome, detail, submitted) = classify_prompt(&output(
            0,
            r#"{"id":"cli:agent:prompt","result":{"agent":{"agent_status":"idle"}}}"#,
            "",
        ));
        assert_eq!(outcome, Outcome::Completed);
        assert!(submitted);
        assert_eq!(detail, "settled idle");
    }

    #[test]
    fn worker_names_fit_herdr_rules_and_differ_by_index() {
        let stamp = "20261004T095811882Z";
        let a = worker_name(1, stamp);
        let b = worker_name(12, stamp);
        assert_eq!(a, "fleet-1-11882");
        assert_eq!(b, "fleet-12-11882");
        assert_eq!(a, a.to_lowercase());
        assert!(a.len() <= 32);
        assert!(a.chars().next().is_some_and(|c| c.is_ascii_lowercase()));
        assert!(a != b);
    }

    #[test]
    fn brief_carries_brief_plus_protocol() {
        let answers = Path::new("/tmp/run/answers");
        let text = worker_brief("Review the auth middleware", "fleet-1-958118", answers);
        assert!(text.starts_with("Review the auth middleware"));
        assert!(text.contains("do not read other workers' files"));
        assert!(text.contains("/tmp/run/answers/fleet-1-958118.md"));
    }

    #[test]
    fn stamp_matches_known_instants() {
        // 2026-10-03T00:00:00Z
        assert_eq!(civil_from_days(20_729), (2026, 10, 3));
        // 1970-01-01T00:00:00Z
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2000-02-29T00:00:00Z
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }

    #[test]
    fn md_cells_escape_table_breakers() {
        assert_eq!(md_cell("a|b"), "a\\|b");
        assert_eq!(md_cell("a\nb"), "a b");
    }
}
