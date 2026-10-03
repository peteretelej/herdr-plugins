use anyhow::{bail, Context as _, Result};
use clap::Parser;
use serde::{Deserialize, Serialize, Serializer};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;

const PLUGIN_ID: &str = "peteretelej.broadcast";

/// Send one prompt to N herdr agents and collect the answers when they settle.
#[derive(Parser)]
#[command(name = "broadcast", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Prompt N agents with one text; classify outcomes and collect answers.
    Run(RunOptions),
    /// Print the summary of the most recent run.
    Last {
        /// Run-state root override (default: herdr's plugin state dir)
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
}

#[derive(clap::Args)]
struct RunOptions {
    /// Prompt text sent to every target
    prompt: String,
    /// Agent names or pane ids (comma-separated, repeatable)
    #[arg(long, value_delimiter = ',')]
    agents: Vec<String>,
    /// Every promptable agent in a workspace (id or label)
    #[arg(long)]
    workspace: Option<String>,
    /// Every promptable agent on this server
    #[arg(long)]
    all: bool,
    /// Squad name from squads.toml in the plugin config dir
    #[arg(long)]
    squad: Option<String>,
    /// Per-agent settle timeout in milliseconds
    #[arg(long, default_value_t = 600_000)]
    timeout: u64,
    /// Answer lines collected per agent
    #[arg(long, default_value_t = 200)]
    lines: usize,
    /// Report outcomes only; skip transcript collection
    #[arg(long)]
    no_collect: bool,
    /// Print the run record as JSON instead of the human summary
    #[arg(long)]
    json: bool,
    /// Run-state root override (default: herdr's plugin state dir)
    #[arg(long)]
    state_dir: Option<PathBuf>,
}

#[derive(Deserialize, Clone)]
struct AgentInfo {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    agent_status: String,
    #[serde(default)]
    workspace_id: String,
    #[serde(default)]
    pane_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Completed,
    /// Needs attention: either the agent was already blocked (prompt not sent)
    /// or it settled blocked after the prompt was delivered (see detail).
    Blocked,
    /// No activity within 5s of submission; delivery unconfirmed.
    Stalled,
    Timeout,
    Failed,
    /// Not prompted: pre-flight state made prompting unsafe or pointless.
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

#[derive(Serialize, Clone)]
struct AgentResult {
    target: String,
    name: Option<String>,
    agent_kind: Option<String>,
    workspace_id: String,
    status_before: String,
    outcome: Outcome,
    detail: String,
    /// The prompt text was submitted to the agent (not merely attempted).
    submitted: bool,
    answer_lines: usize,
    answer_file: Option<String>,
}

#[derive(Serialize)]
struct RunRecord {
    timestamp: String,
    prompt: String,
    timeout_ms: u64,
    collect_lines: usize,
    results: Vec<AgentResult>,
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Run(mut opts) => {
            opts.agents.retain(|a| !a.is_empty());
            run(opts)
        }
        Cmd::Last { state_dir } => last(state_dir.as_deref()),
    }
}

fn run(opts: RunOptions) -> Result<()> {
    if opts.prompt.trim().is_empty() {
        bail!("prompt text is empty");
    }
    if opts.lines == 0 && !opts.no_collect {
        bail!("--lines must be at least 1 (or pass --no-collect)");
    }
    let mut wanted = opts.agents.clone();
    if let Some(squad) = &opts.squad {
        wanted.extend(squad_targets(squad)?);
    }
    if wanted.is_empty() && opts.workspace.is_none() && !opts.all {
        bail!("no targets selected; use --agents, --workspace, --squad or --all");
    }

    let agents = agent_list()?;
    let mut selected: BTreeMap<String, AgentInfo> = BTreeMap::new();
    if !wanted.is_empty() {
        let mut missing = Vec::new();
        for key in &wanted {
            match agents
                .iter()
                .find(|a| a.name.as_deref() == Some(key.as_str()) || a.pane_id == *key)
            {
                Some(agent) => {
                    selected.insert(agent.pane_id.clone(), agent.clone());
                }
                None => missing.push(key.clone()),
            }
        }
        if !missing.is_empty() {
            bail!(
                "unknown agent(s): {} - live agents: {}",
                missing.join(", "),
                describe_agents(&agents)
            );
        }
    }
    if let Some(workspace) = &opts.workspace {
        let ids = workspace_ids(workspace, &agents)?;
        for agent in agents.iter().filter(|a| ids.contains(a.workspace_id.as_str())) {
            selected.insert(agent.pane_id.clone(), agent.clone());
        }
    }
    if opts.all {
        for agent in &agents {
            selected.insert(agent.pane_id.clone(), agent.clone());
        }
    }
    if selected.is_empty() {
        bail!("no agents matched; live agents: {}", describe_agents(&agents));
    }

    let mut skipped = Vec::new();
    let mut promptable = Vec::new();
    for agent in selected.into_values() {
        let mut result = base_result(&agent);
        match skip_reason(&agent.agent_status) {
            Some(reason) => {
                result.outcome = Outcome::Skipped;
                result.detail = reason;
                skipped.push(result);
            }
            None => promptable.push(agent),
        }
    }

    let stamp = run_stamp();
    let dir = state_root(opts.state_dir.as_deref())?
        .join("runs")
        .join(&stamp);
    std::fs::create_dir_all(dir.join("answers"))
        .with_context(|| format!("creating {}", dir.display()))?;

    let mut results = skipped;
    results.extend(thread::scope(|scope| {
        let handles: Vec<_> = promptable
            .iter()
            .map(|agent| {
                let prompt = opts.prompt.clone();
                let timeout = opts.timeout.to_string();
                let lines = opts.lines;
                let no_collect = opts.no_collect;
                let answers = dir.join("answers");
                scope.spawn(move || {
                    prompt_one(agent, &prompt, &timeout, lines, no_collect, &answers)
                })
            })
            .collect();
        handles
            .into_iter()
            .zip(promptable.iter())
            .map(|(handle, agent)| {
                handle.join().unwrap_or_else(|_| AgentResult {
                    outcome: Outcome::Failed,
                    detail: "internal error: worker panicked".into(),
                    submitted: false,
                    ..base_result(agent)
                })
            })
            .collect::<Vec<AgentResult>>()
    }));
    results.sort_by(|a, b| a.target.cmp(&b.target));

    let record = RunRecord {
        timestamp: stamp,
        prompt: opts.prompt.clone(),
        timeout_ms: opts.timeout,
        collect_lines: if opts.no_collect { 0 } else { opts.lines },
        results,
    };

    let mut status = std::fs::File::create(dir.join("status.jsonl"))
        .with_context(|| format!("creating {}", dir.join("status.jsonl").display()))?;
    for result in &record.results {
        serde_json::to_writer(&mut status, result)?;
        status.write_all(b"\n")?;
    }
    let summary = summary_md(&record, &dir);
    std::fs::write(dir.join("summary.md"), summary)?;

    if opts.json {
        println!("{}", serde_json::to_string_pretty(&record)?);
    } else {
        print_run_summary(&record, &dir);
    }
    std::io::stdout().flush()?;
    if record.results.iter().any(|r| r.submitted) {
        Ok(())
    } else {
        bail!("no prompt reached an agent");
    }
}

fn prompt_one(
    agent: &AgentInfo,
    prompt: &str,
    timeout: &str,
    lines: usize,
    no_collect: bool,
    answers: &Path,
) -> AgentResult {
    let mut result = base_result(agent);
    let out = herdr()
        .args([
            "agent",
            "prompt",
            &agent.pane_id,
            prompt,
            "--wait",
            "--timeout",
            timeout,
        ])
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
    if result.outcome == Outcome::Completed && !no_collect {
        match collect(&agent.pane_id, lines, answers) {
            Ok((count, file)) => {
                result.answer_lines = count;
                result.answer_file = Some(file);
            }
            // Collection is best-effort: the agent may have started a new
            // turn between settling and the read (agent_not_idle).
            Err(err) => result.detail.push_str(&format!("; collection failed: {err}")),
        }
    }
    result
}

fn base_result(agent: &AgentInfo) -> AgentResult {
    AgentResult {
        target: agent.pane_id.clone(),
        name: agent.name.clone(),
        agent_kind: agent.agent.clone(),
        workspace_id: agent.workspace_id.clone(),
        status_before: agent.agent_status.clone(),
        outcome: Outcome::Failed,
        detail: String::new(),
        submitted: false,
        answer_lines: 0,
        answer_file: None,
    }
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
                "delivered; agent settled blocked and needs attention".into(),
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
                "not sent: agent is blocked; resolve the pane first".into(),
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

/// Read the answer transcript. Only safe right after a completed settle:
/// alternate-screen paging is idle-gated (`agent_not_idle` otherwise).
fn collect(target: &str, lines: usize, answers: &Path) -> Result<(usize, String)> {
    let lines_arg = lines.to_string();
    let out = herdr()
        .args([
            "agent",
            "read",
            target,
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
    let file = format!("{}.txt", file_stem_for(target));
    std::fs::write(answers.join(&file), text)?;
    Ok((text.lines().count(), file))
}

/// None when the agent can take a prompt now; Some(reason) to skip it.
/// Prompting a working agent queues the text and its wait can be satisfied
/// by the previous turn, so anything not settled is skipped.
fn skip_reason(status: &str) -> Option<String> {
    match status {
        "idle" | "done" => None,
        "working" => Some("working on another turn".into()),
        "blocked" => Some("waiting on a permission prompt".into()),
        "" => Some("no status reported".into()),
        other => Some(format!("status {other}")),
    }
}

fn agent_list() -> Result<Vec<AgentInfo>> {
    let response = herdr_json(&["agent", "list"])?;
    let agents = response
        .pointer("/result/agents")
        .cloned()
        .context("herdr agent list returned no agents array")?;
    Ok(serde_json::from_value(agents)?)
}

fn describe_agents(agents: &[AgentInfo]) -> String {
    agents
        .iter()
        .map(|a| {
            let label = a.name.clone().unwrap_or_else(|| a.pane_id.clone());
            format!("{label}({})", a.agent_status)
        })
        .collect::<Vec<_>>()
        .join(", ")
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

/// Resolve a --workspace value (id or label) to workspace ids.
fn workspace_ids(filter: &str, agents: &[AgentInfo]) -> Result<BTreeSet<String>> {
    if agents.iter().any(|a| a.workspace_id == filter) {
        return Ok(BTreeSet::from([filter.to_string()]));
    }
    let response = herdr_json(&["workspace", "list"])?;
    let found: Vec<String> = response
        .pointer("/result/workspaces")
        .and_then(|v| v.as_array())
        .map(|rows| {
            rows.iter()
                .filter(|w| w["label"].as_str() == Some(filter))
                .filter_map(|w| w["workspace_id"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if found.is_empty() {
        bail!("workspace '{filter}' not found (no matching id or label)");
    }
    Ok(found.into_iter().collect())
}

/// squads.toml lines of `name = "target-a, target-b"` - flat TOML, hand-parsed
/// like the rest of the suite (no toml dependency).
fn squad_targets(squad: &str) -> Result<Vec<String>> {
    let path = config_dir()?.join("squads.toml");
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    match lookup_squad(&text, squad).context(format!("in {}", path.display()))? {
        Some(targets) => Ok(targets),
        None => {
            let names = squad_names(&text);
            if names.is_empty() {
                bail!("squad '{squad}' not found in {} (no squads defined)", path.display());
            }
            bail!(
                "squad '{squad}' not found in {} (defined: {})",
                path.display(),
                names.join(", ")
            );
        }
    }
}

/// Ok(None) when the squad is missing; Err(message) for a malformed/empty hit.
fn lookup_squad(text: &str, squad: &str) -> Result<Option<Vec<String>>> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != squad {
            continue;
        }
        let targets: Vec<String> = value
            .trim()
            .trim_matches('"')
            .split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
        if targets.is_empty() {
            bail!("squad '{squad}' has no targets");
        }
        return Ok(Some(targets));
    }
    Ok(None)
}

fn squad_names(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            line.split_once('=').map(|(key, _)| key.trim().to_string())
        })
        .filter(|key| !key.is_empty())
        .collect()
}

/// Plugin config dir: env when manifest-invoked; the CLI's answer for bare
/// shell runs (which carry no HERDR_PLUGIN_CONFIG_DIR).
fn config_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("HERDR_PLUGIN_CONFIG_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let out = herdr()
        .args(["plugin", "config-dir", PLUGIN_ID])
        .output()
        .context("running herdr plugin config-dir")?;
    if !out.status.success() {
        bail!(
            "herdr plugin config-dir failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
    ))
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

fn file_stem_for(target: &str) -> String {
    target
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect()
}

/// Markdown table cells break on pipes and newlines.
fn md_cell(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}

fn print_run_summary(record: &RunRecord, dir: &Path) {
    println!(
        "broadcast to {} agent(s) - prompt: {:?}",
        record.results.len(),
        record.prompt
    );
    let width = record
        .results
        .iter()
        .map(|r| r.name.as_deref().unwrap_or(&r.target).len())
        .max()
        .unwrap_or(0)
        .max(1);
    for r in &record.results {
        let label = r.name.as_deref().unwrap_or(&r.target);
        let answer = r
            .answer_file
            .as_deref()
            .map(|f| format!("answers/{f} ({} lines)", r.answer_lines))
            .unwrap_or_default();
        println!(
            "  {label:<width$}  {:<9}  {}{}",
            r.outcome.as_str(),
            r.detail,
            if answer.is_empty() {
                String::new()
            } else {
                format!("  [{answer}]")
            }
        );
    }
    println!("run dir: {}", dir.join("summary.md").display());
}

fn summary_md(record: &RunRecord, dir: &Path) -> String {
    let mut s = format!("# Broadcast run {}\n\n", record.timestamp);
    s.push_str(&format!("- prompt: {}\n", md_cell(&record.prompt)));
    s.push_str(&format!(
        "- timeout: {} ms per agent, collection: {} lines\n\n",
        record.timeout_ms, record.collect_lines
    ));
    s.push_str("| target | name | before | outcome | detail | answer |\n");
    s.push_str("|---|---|---|---|---|---|\n");
    for r in &record.results {
        let answer = r
            .answer_file
            .as_deref()
            .map(|f| format!("answers/{f} ({} lines)", r.answer_lines))
            .unwrap_or_else(|| "-".into());
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            md_cell(&r.target),
            md_cell(r.name.as_deref().unwrap_or("-")),
            md_cell(&r.status_before),
            r.outcome.as_str(),
            md_cell(&r.detail),
            md_cell(&answer),
        ));
    }
    for r in &record.results {
        let Some(file) = &r.answer_file else { continue };
        s.push_str(&format!(
            "\n## {} ({})\n\n",
            md_cell(r.name.as_deref().unwrap_or(&r.target)),
            r.target
        ));
        if let Ok(text) = std::fs::read_to_string(dir.join("answers").join(file)) {
            s.push_str(text.trim_end());
            s.push('\n');
        }
    }
    s
}

fn last(state_dir: Option<&Path>) -> Result<()> {
    let runs = state_root(state_dir)?.join("runs");
    let latest = match std::fs::read_dir(&runs) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().join("summary.md").is_file())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .max(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err).context(format!("reading {}", runs.display())),
    }
    .context("no broadcast runs yet")?;
    let summary = std::fs::read_to_string(runs.join(&latest).join("summary.md"))?;
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
        let (outcome, detail, submitted) = classify_prompt(&output(
            1,
            "",
            r#"{"id":"cli:agent:prompt","error":{"code":"agent_blocked","message":"x"}}"#,
        ));
        assert_eq!(outcome, Outcome::Blocked);
        assert!(!submitted);
        assert!(detail.contains("not sent"));
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
    fn only_settled_agents_are_promptable() {
        assert!(skip_reason("idle").is_none());
        assert!(skip_reason("done").is_none());
        assert_eq!(skip_reason("working").as_deref(), Some("working on another turn"));
        assert_eq!(skip_reason("blocked").as_deref(), Some("waiting on a permission prompt"));
        assert!(skip_reason("unknown").is_some());
        assert!(skip_reason("").is_some());
    }

    #[test]
    fn squads_parse_from_flat_toml() {
        let text = "# squads\ndocs = \"docs-writer, api-docs\"\nreview = \"reviewer, auditor\"\n";
        assert_eq!(
            lookup_squad(text, "review").unwrap(),
            Some(vec!["reviewer".into(), "auditor".into()])
        );
        assert!(lookup_squad(text, "missing").unwrap().is_none());
        assert!(lookup_squad("empty = \"\"", "empty").is_err());
        assert_eq!(squad_names(text), vec!["docs", "review"]);
    }

    #[test]
    fn file_stems_are_filesystem_safe() {
        assert_eq!(file_stem_for("w1:p3"), "w1-p3");
        assert_eq!(file_stem_for("reviewer"), "reviewer");
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
