//! peteretelej.notify: Telegram notifications when herdr agents turn blocked
//! or done. The `event` hook decides and debounces, then hands delivery to a
//! detached `send` process so the hook never blocks on network (slow hooks
//! trigger events_lost on herdr 0.9.2+).

use anyhow::{bail, Context as _, Result};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Send Telegram notifications when agents turn blocked or done.
#[derive(Parser)]
#[command(name = "notify", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Event hook entry for pane.agent_status_changed. Always exits 0.
    Event,
    /// Internal: deliver one message. Spawned detached by `event`.
    Send {
        /// JSON message payload produced by `event`.
        #[arg(long)]
        payload: String,
    },
    /// Send a test message to verify credentials and routing.
    Test,
    /// Mute or resume notifications.
    Mute {
        /// `on` silences, `off` resumes.
        state: String,
    },
}

fn main() {
    let cli = Cli::parse();
    let is_event = matches!(cli.cmd, Cmd::Event);
    let result = match cli.cmd {
        Cmd::Event => on_event(),
        Cmd::Send { payload } => on_send(&payload),
        Cmd::Test => on_test(),
        Cmd::Mute { state } => on_mute(&state),
    };
    // Event hooks must never fail loudly: herdr only sees our stderr in the
    // plugin log, and a broken notify must never disturb herdr. User-invoked
    // subcommands (test, mute) fail loudly instead.
    if let Err(err) = result {
        eprintln!("notify: {err:#}");
        if !is_event {
            std::process::exit(1);
        }
    }
}

// ---------------------------------------------------------------- config

struct Config {
    notify: Vec<String>,
    debounce_secs: u64,
    include_workspaces: Vec<String>,
    exclude_workspaces: Vec<String>,
    quiet_hours: Vec<(u16, u16)>,
    telegram: Option<TelegramCfg>,
}

struct TelegramCfg {
    bot_token: String,
    chat_id: String,
    silent: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            notify: vec!["blocked".into(), "done".into()],
            debounce_secs: 30,
            include_workspaces: vec![],
            exclude_workspaces: vec![],
            quiet_hours: vec![],
            telegram: None,
        }
    }
}

impl Config {
    fn load() -> Self {
        let mut cfg = Config::default();
        let Some(dir) = std::env::var_os("HERDR_PLUGIN_CONFIG_DIR") else {
            return cfg;
        };
        let Ok(text) = std::fs::read_to_string(PathBuf::from(dir).join("notify.toml")) else {
            return cfg;
        };
        parse_config(&text, &mut cfg);
        cfg
    }

    fn telegram_ready(&self) -> bool {
        self.telegram
            .as_ref()
            .is_some_and(|t| !t.bot_token.is_empty() && !t.chat_id.is_empty())
    }
}

/// Minimal TOML subset parser: top-level keys, one-level `[table]` sections,
/// and string / integer / boolean / string-array values. Enough for
/// notify.toml without a toml dependency.
fn parse_config(text: &str, cfg: &mut Config) {
    let mut section = String::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Section names cannot contain '#'; strip a trailing comment so
        // `[telegram] # creds` still opens the section.
        if let Some(name) = line
            .split('#')
            .next()
            .unwrap_or("")
            .trim()
            .strip_prefix('[')
            .and_then(|l| l.strip_suffix(']'))
        {
            section = name.trim().to_string();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        // Multi-line arrays are valid TOML; buffer until the bracket closes.
        // Comments run to end of line, so strip each line before joining.
        let mut value = strip_line_comment(value).trim().to_string();
        if value.starts_with('[') && !value.contains(']') {
            for cont in lines.by_ref() {
                value.push(' ');
                value.push_str(strip_line_comment(cont.trim()));
                if cont.contains(']') {
                    break;
                }
            }
        }
        let value = value.trim();
        match section.as_str() {
            "" => match key {
                "notify" => cfg.notify = parse_string_array(value),
                "debounce_secs" => cfg.debounce_secs = parse_int(value).unwrap_or(30),
                "include_workspaces" => cfg.include_workspaces = parse_string_array(value),
                "exclude_workspaces" => cfg.exclude_workspaces = parse_string_array(value),
                "quiet_hours" => {
                    cfg.quiet_hours = parse_string_array(value)
                        .iter()
                        .filter_map(|v| parse_quiet_range(v))
                        .collect()
                }
                _ => {}
            },
            "telegram" => {
                let tg = cfg.telegram.get_or_insert_with(|| TelegramCfg {
                    bot_token: String::new(),
                    chat_id: String::new(),
                    silent: false,
                });
                match key {
                    "bot_token" => tg.bot_token = parse_string(value),
                    "chat_id" => tg.chat_id = parse_string(value),
                    "silent" => tg.silent = value == "true",
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

/// Parse a quoted string, bare word, or number; ignores trailing comments.
/// True-end-of-line comment strip that respects quoted `#` characters.
fn strip_line_comment(line: &str) -> &str {
    let mut in_quote = false;
    for (i, ch) in line.char_indices() {
        match ch {
            '"' => in_quote = !in_quote,
            '#' if !in_quote => return &line[..i],
            _ => {}
        }
    }
    line
}

fn parse_string(value: &str) -> String {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix('"') {
        if let Some(end) = rest.find('"') {
            return rest[..end].to_string();
        }
        return rest.to_string();
    }
    value.split('#').next().unwrap_or("").trim().to_string()
}

fn parse_int(value: &str) -> Option<u64> {
    parse_string(value).parse().ok()
}

/// Parse `["a", "b"]` (or a single bare word) into strings. Trailing
/// comments are tolerated: only the `[...]` span is read.
fn parse_string_array(value: &str) -> Vec<String> {
    let value = value.trim();
    let Some(open) = value.find('[') else {
        let word = parse_string(value);
        return if word.is_empty() { vec![] } else { vec![word] };
    };
    let Some(close_rel) = value[open..].find(']') else {
        return vec![];
    };
    value[open + 1..open + close_rel]
        .split(',')
        .map(parse_string)
        .filter(|s| !s.is_empty())
        .collect()
}

/// Parse `"HH:MM-HH:MM"` into (start_minutes, end_minutes) since midnight.
fn parse_quiet_range(value: &str) -> Option<(u16, u16)> {
    let (start, end) = value.trim().split_once('-')?;
    Some((parse_hhmm(start)?, parse_hhmm(end)?))
}

fn parse_hhmm(value: &str) -> Option<u16> {
    let (h, m) = value.trim().split_once(':')?;
    let hours: u16 = h.trim().parse().ok()?;
    let mins: u16 = m.trim().parse().ok()?;
    if hours > 23 || mins > 59 {
        return None;
    }
    Some(hours * 60 + mins)
}

/// True when `now_minutes` falls in the range; overnight ranges wrap.
fn in_quiet_range(now_minutes: u16, range: (u16, u16)) -> bool {
    let (start, end) = range;
    if start == end {
        return false;
    }
    if start < end {
        now_minutes >= start && now_minutes < end
    } else {
        now_minutes >= start || now_minutes < end
    }
}

// ---------------------------------------------------------------- state

#[derive(Serialize, Deserialize, Default)]
struct State {
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    panes: BTreeMap<String, PaneState>,
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct PaneState {
    #[serde(default)]
    last_status: String,
    #[serde(default)]
    last_sent: BTreeMap<String, u64>,
    #[serde(default)]
    seen: u64,
}

fn state_path() -> Result<PathBuf> {
    let dir = std::env::var("HERDR_PLUGIN_STATE_DIR")
        .context("HERDR_PLUGIN_STATE_DIR not set; run via herdr")?;
    Ok(PathBuf::from(dir).join("notify-state.json"))
}

fn load_state() -> State {
    std::fs::read(state_path().unwrap_or_default())
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .unwrap_or_default()
}

fn persist(state: &State) {
    let Ok(path) = state_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut pruned = State {
        disabled: state.disabled,
        panes: BTreeMap::new(),
    };
    let cutoff = now_ms().saturating_sub(7 * 24 * 3600 * 1000);
    for (id, pane) in &state.panes {
        if pane.seen >= cutoff {
            pruned.panes.insert(id.clone(), pane.clone());
        }
    }
    if let Ok(raw) = serde_json::to_vec(&pruned) {
        // Pid-unique temp name: concurrent writers must never share one tmp
        // file, or a rename can publish a torn state file that silently
        // resets mute flags and debounce history.
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        if std::fs::write(&tmp, &raw).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------- events

#[derive(Deserialize, Default)]
struct EventJson {
    #[serde(default)]
    event: EventInner,
}

#[derive(Deserialize, Default)]
struct EventInner {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    pane_id: String,
    #[serde(default)]
    agent_status: Option<String>,
}

#[derive(Deserialize, Default)]
struct ContextJson {
    #[serde(default)]
    workspace_label: Option<String>,
    #[serde(default)]
    workspace_cwd: Option<String>,
    #[serde(default)]
    tab_label: Option<String>,
    #[serde(default)]
    focused_pane_status: Option<String>,
}

/// Enrichment fields from `herdr pane get <pane_id>`. All optional: the CLI
/// call is best-effort and the message degrades gracefully without it.
#[derive(Deserialize, Default)]
struct PaneInfo {
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    display_agent: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    terminal_title_stripped: Option<String>,
    #[serde(default)]
    terminal_title: Option<String>,
}

/// Map a raw status plus the pane's previous status to a notification kind.
/// A watched pane reports a finished turn as `idle` (herdr only reports
/// `done` while the completion is unseen), so active -> idle counts as done.
fn notification_kind(raw: &str, last_status: &str) -> Option<String> {
    match raw {
        "blocked" => Some("blocked".into()),
        "done" => Some("done".into()),
        "idle" if matches!(last_status, "working" | "blocked") => Some("done".into()),
        _ => None,
    }
}

fn workspace_allowed(cfg: &Config, workspace: &str) -> bool {
    if !cfg.include_workspaces.is_empty() && !cfg.include_workspaces.iter().any(|w| w == workspace)
    {
        return false;
    }
    !cfg.exclude_workspaces.iter().any(|w| w == workspace)
}

fn minutes_now() -> u16 {
    let secs = now_ms() / 1000;
    minutes_of_day(secs, local_utc_offset_secs())
}

/// Local minute-of-day from UTC seconds and a signed UTC offset. Euclidean
/// remainder keeps negative offsets (the Americas) on the correct wall clock.
fn minutes_of_day(secs: u64, offset_secs: i64) -> u16 {
    let local = (secs as i64 + offset_secs).rem_euclid(86_400);
    (local / 60) as u16
}

/// Seconds east of UTC, read from the `TZ`-aware libc offset via `date`.
/// Shelling out keeps the dependency set lean; this runs at most once per
/// notification that reaches the quiet-hours gate.
fn local_utc_offset_secs() -> i64 {
    let offset = (|| -> Option<i64> {
        let out = std::process::Command::new("date")
            .arg("+%z")
            .output()
            .ok()?;
        let s = String::from_utf8(out.stdout).ok()?;
        parse_offset(&s)
    })();
    offset.unwrap_or(0)
}

/// Parse a `+HHMM` / `-HHMM` UTC offset.
fn parse_offset(s: &str) -> Option<i64> {
    let s = s.trim();
    let sign = if s.starts_with('-') { -1 } else { 1 };
    let digits = s.trim_start_matches(['+', '-']);
    let hours: i64 = digits.get(0..2)?.parse().ok()?;
    let mins: i64 = digits.get(2..4)?.parse().ok()?;
    Some(sign * (hours * 3600 + mins * 60))
}

fn on_event() -> Result<()> {
    let event: EventJson =
        serde_json::from_str(&std::env::var("HERDR_PLUGIN_EVENT_JSON").unwrap_or_default())
            .unwrap_or_default();
    if !event.event.kind.is_empty() && event.event.kind != "pane.agent_status_changed" {
        return Ok(());
    }
    let ctx: ContextJson =
        serde_json::from_str(&std::env::var("HERDR_PLUGIN_CONTEXT_JSON").unwrap_or_default())
            .unwrap_or_default();

    let pane_id = if event.event.pane_id.is_empty() {
        std::env::var("HERDR_PANE_ID").unwrap_or_default()
    } else {
        event.event.pane_id.clone()
    };
    if pane_id.is_empty() {
        return Ok(());
    }

    let mut state = load_state();
    if state.disabled {
        return Ok(());
    }

    let raw_status = event
        .event
        .agent_status
        .clone()
        .or(ctx.focused_pane_status.clone())
        .unwrap_or_default();
    let entry = state.panes.entry(pane_id.clone()).or_default();
    entry.seen = now_ms();
    let last_status = entry.last_status.clone();
    entry.last_status = raw_status.clone();
    let kind = match notification_kind(&raw_status, &last_status) {
        Some(kind) => kind,
        None => {
            persist(&state);
            return Ok(());
        }
    };
    persist(&state);

    let cfg = Config::load();
    if !cfg.notify.iter().any(|k| k == &kind) {
        return Ok(());
    }

    let workspace = ctx
        .workspace_label
        .clone()
        .or_else(|| std::env::var("HERDR_WORKSPACE_ID").ok())
        .unwrap_or_else(|| "unknown".into());
    if !workspace_allowed(&cfg, &workspace) {
        return Ok(());
    }

    // Debounce before the quiet-hours clock: flapping panes must not pay for
    // a `date` call per event.
    let window_ms = cfg.debounce_secs.saturating_mul(1000);
    let last_sent = state
        .panes
        .get(&pane_id)
        .and_then(|p| p.last_sent.get(&kind))
        .copied()
        .unwrap_or(0);
    if now_ms().saturating_sub(last_sent) < window_ms {
        return Ok(());
    }

    let minutes = minutes_now();
    if kind != "blocked" && cfg.quiet_hours.iter().any(|r| in_quiet_range(minutes, *r)) {
        return Ok(());
    }

    state
        .panes
        .entry(pane_id.clone())
        .or_default()
        .last_sent
        .insert(kind.clone(), now_ms());
    persist(&state);

    let info = pane_info(&pane_id);
    let msg = Message {
        agent: info
            .display_agent
            .clone()
            .or(info.agent.clone())
            .unwrap_or_else(|| "agent".into()),
        workspace: workspace.clone(),
        tab: ctx.tab_label.clone().unwrap_or_default(),
        topic: info
            .terminal_title_stripped
            .clone()
            .or(info.terminal_title.clone())
            .unwrap_or_default(),
        cwd: info
            .cwd
            .clone()
            .or(ctx.workspace_cwd.clone())
            .unwrap_or_default(),
        status: kind.clone(),
        raw_status,
    };

    spawn_sender(&msg)
        .with_context(|| format!("spawning detached sender for {kind} on {pane_id}"))?;
    Ok(())
}

/// One cached `herdr pane get` call per notification; failure degrades to an
/// empty PaneInfo rather than dropping the message.
fn pane_info(pane_id: &str) -> PaneInfo {
    let herdr = std::env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".into());
    let Ok(out) = Command::new(herdr).args(["pane", "get", pane_id]).output() else {
        return PaneInfo::default();
    };
    serde_json::from_slice(&out.stdout).unwrap_or_default()
}

fn spawn_sender(msg: &Message) -> Result<()> {
    let exe = std::env::current_exe().context("resolving current exe")?;
    let payload = serde_json::to_string(msg)?;
    Command::new(exe)
        .args(["send", "--payload", &payload])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("spawning send child")?;
    Ok(())
}

// ---------------------------------------------------------------- message

#[derive(Serialize, Deserialize, Clone)]
struct Message {
    agent: String,
    workspace: String,
    tab: String,
    topic: String,
    cwd: String,
    status: String,
    #[serde(default)]
    raw_status: String,
}

impl Message {
    fn icon(&self) -> &'static str {
        match self.status.as_str() {
            "blocked" => "⛔",
            "done" => "✅",
            _ => "•",
        }
    }

    /// Plain text only: no parse_mode means no escaping bugs, and Telegram
    /// shows receive time itself, so no timestamp in the text.
    fn render(&self) -> String {
        let mut text = format!("{} {} is {}", self.icon(), self.agent, self.status);
        if !self.raw_status.is_empty() && self.raw_status != self.status {
            text.push_str(&format!(" (reported {})", self.raw_status));
        }
        text.push('\n');
        if self.tab.is_empty() {
            text.push_str(&self.workspace);
        } else {
            text.push_str(&format!("{} / {}", self.workspace, self.tab));
        }
        if !self.topic.is_empty() {
            text.push_str(&format!(": {}", self.topic));
        }
        if !self.cwd.is_empty() {
            text.push('\n');
            text.push_str(&self.cwd);
        }
        text
    }
}

// ---------------------------------------------------------------- sinks

fn telegram_url(bot_token: &str) -> String {
    format!("https://api.telegram.org/bot{bot_token}/sendMessage")
}

/// One attempt with a hard timeout; on retryable failures (429 with
/// retry_after, or transient HTTP statuses) sleep briefly and retry once.
/// Bounded by design: a dropped notification is acceptable, a hung sender
/// is not.
fn send_telegram(cfg: &TelegramCfg, msg: &Message) -> Result<()> {
    let body = serde_json::json!({
        "chat_id": cfg.chat_id,
        "text": msg.render(),
        "disable_web_page_preview": true,
        "disable_notification": cfg.silent,
    });
    let mut attempt = 0;
    loop {
        attempt += 1;
        let outcome = ureq::post(&telegram_url(&cfg.bot_token))
            .timeout(Duration::from_secs(5))
            .send_json(body.clone());
        match outcome {
            Ok(_) => return Ok(()),
            Err(ureq::Error::Status(code, resp)) => {
                let detail = resp.into_string().unwrap_or_default();
                let retryable = matches!(code, 408 | 425 | 429 | 500..=599);
                if !retryable || attempt > 1 {
                    // The response body never echoes the token; redact anyway
                    // so no error path can ever carry it.
                    bail!(
                        "telegram returned {code}: {}",
                        redact_token(truncate(&detail, 200), &cfg.bot_token)
                    );
                }
                std::thread::sleep(backoff(code, &detail));
            }
            Err(err) => {
                if attempt > 1 {
                    // ureq's Display includes the request URL, which embeds
                    // the bot token; redact before the error leaves here.
                    bail!(
                        "telegram request failed: {}",
                        redact_token(&err.to_string(), &cfg.bot_token)
                    );
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

/// Honor Telegram's `parameters.retry_after` from a 429 body, capped so the
/// detached sender never sleeps long; other statuses get a fixed short wait.
fn backoff(status: u16, body: &str) -> Duration {
    if status == 429 {
        let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
        let retry_after = parsed
            .get("parameters")
            .and_then(|p| p.get("retry_after"))
            .and_then(|v| v.as_u64())
            .unwrap_or(1);
        return Duration::from_secs(retry_after.clamp(1, 5));
    }
    Duration::from_secs(1)
}

fn truncate(text: &str, max: usize) -> &str {
    match text.get(..max) {
        Some(head) => head,
        None => text,
    }
}

/// Scrub the bot token out of any error text before it reaches a log.
fn redact_token(text: &str, bot_token: &str) -> String {
    if bot_token.is_empty() {
        return text.to_string();
    }
    text.replace(bot_token, "[redacted]")
}

/// Delivery failures land in a small state-dir log: the sender's stderr is
/// null (that is the point of detaching), so this file is the only surface.
fn log_delivery(line: &str) {
    let Ok(dir) = std::env::var("HERDR_PLUGIN_STATE_DIR") else {
        return;
    };
    let path = PathBuf::from(dir).join("notify.log");
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
    // Cap the log: keep the newest lines when it grows past 64 KiB.
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > 64 * 1024 {
            if let Ok(old) = std::fs::read_to_string(&path) {
                let lines: Vec<&str> = old.lines().collect();
                let tail = lines.len().saturating_sub(40);
                let _ = std::fs::write(&path, lines[tail..].join("\n"));
            }
        }
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)
    {
        let _ = writeln!(file, "{} {line}", timestamp());
    }
}

fn timestamp() -> String {
    let secs = now_ms() / 1000;
    format!("{secs}")
}

// ---------------------------------------------------------------- commands

fn on_send(payload: &str) -> Result<()> {
    let msg: Message = serde_json::from_str(payload).context("parsing send payload")?;
    let cfg = Config::load();
    let Some(tg) = &cfg.telegram else {
        log_delivery(&format!(
            "drop {}: telegram not configured",
            serde_json::to_value(&msg).unwrap_or_default()
        ));
        return Ok(());
    };
    match send_telegram(tg, &msg) {
        Ok(()) => {
            log_delivery(&format!("sent {}: {}", msg.status, msg.agent));
            Ok(())
        }
        Err(err) => {
            log_delivery(&format!("failed {}: {err:#}", msg.status));
            Ok(())
        }
    }
}

fn on_test() -> Result<()> {
    let cfg = Config::load();
    if !cfg.telegram_ready() {
        bail!(
            "telegram is not configured; add [telegram] bot_token and chat_id to {}",
            std::env::var("HERDR_PLUGIN_CONFIG_DIR")
                .map(|d| format!("{d}/notify.toml"))
                .unwrap_or_else(|_| "notify.toml".into())
        );
    }
    let msg = Message {
        agent: "notify".into(),
        workspace: "test".into(),
        tab: String::new(),
        topic: "If you can read this, peteretelej.notify works".into(),
        cwd: String::new(),
        status: "done".into(),
        raw_status: String::new(),
    };
    send_telegram(cfg.telegram.as_ref().unwrap(), &msg)?;
    println!("test notification sent");
    Ok(())
}

fn on_mute(state: &str) -> Result<()> {
    let disabled = match state {
        "on" => true,
        "off" => false,
        other => bail!("expected `on` or `off`, got `{other}`"),
    };
    let mut state = load_state();
    state.disabled = disabled;
    persist(&state);
    println!("{}", if disabled { "muted" } else { "resumed" });
    Ok(())
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> Config {
        let mut cfg = Config::default();
        parse_config(text, &mut cfg);
        cfg
    }

    #[test]
    fn completion_on_watched_pane_arrives_as_idle() {
        assert_eq!(
            notification_kind("idle", "working").as_deref(),
            Some("done")
        );
        assert_eq!(
            notification_kind("idle", "blocked").as_deref(),
            Some("done")
        );
        assert_eq!(notification_kind("idle", "idle"), None);
        assert_eq!(notification_kind("idle", ""), None);
        assert_eq!(
            notification_kind("done", "working").as_deref(),
            Some("done")
        );
        assert_eq!(notification_kind("blocked", "").as_deref(), Some("blocked"));
        assert_eq!(notification_kind("working", "idle"), None);
    }

    #[test]
    fn parses_full_config() {
        let cfg = config(
            r#"
debounce_secs = 45
notify = ["blocked"]
include_workspaces = ["webshop", "api"]
exclude_workspaces = ["docs-site"]
quiet_hours = ["22:00-07:00"]  # nights

[telegram]
bot_token = "123:abc"
chat_id = "42"
silent = true
"#,
        );
        assert_eq!(cfg.debounce_secs, 45);
        assert_eq!(cfg.notify, vec!["blocked".to_string()]);
        assert_eq!(
            cfg.include_workspaces,
            vec!["webshop".to_string(), "api".to_string()]
        );
        assert_eq!(cfg.exclude_workspaces, vec!["docs-site".to_string()]);
        assert_eq!(cfg.quiet_hours, vec![(22 * 60, 7 * 60)]);
        assert!(cfg.telegram_ready());
        assert!(cfg.telegram.as_ref().unwrap().silent);
    }

    #[test]
    fn defaults_when_file_missing_keys() {
        let cfg = config("");
        assert_eq!(cfg.debounce_secs, 30);
        assert_eq!(cfg.notify.len(), 2);
        assert!(!cfg.telegram_ready());
    }

    #[test]
    fn quiet_ranges_wrap_midnight() {
        assert!(in_quiet_range(23 * 60, (22 * 60, 7 * 60)));
        assert!(in_quiet_range(3 * 60, (22 * 60, 7 * 60)));
        assert!(!in_quiet_range(12 * 60, (22 * 60, 7 * 60)));
        assert!(!in_quiet_range(7 * 60, (22 * 60, 7 * 60)));
        assert!(in_quiet_range(9 * 60, (9 * 60, 12 * 60)));
        assert!(!in_quiet_range(12 * 60, (9 * 60, 12 * 60)));
        assert!(!in_quiet_range(5 * 60, (7 * 60, 7 * 60)));
    }

    #[test]
    fn workspace_filters_match_label_or_id() {
        let cfg = config("include_workspaces = [\"webshop\"]");
        assert!(workspace_allowed(&cfg, "webshop"));
        assert!(!workspace_allowed(&cfg, "api"));
        let cfg = config("exclude_workspaces = [\"api\"]\ninclude_workspaces = []");
        assert!(workspace_allowed(&cfg, "webshop"));
        assert!(!workspace_allowed(&cfg, "api"));
    }

    #[test]
    fn message_renders_triage_fields() {
        let msg = Message {
            agent: "opencode".into(),
            workspace: "webshop".into(),
            tab: "api".into(),
            topic: "fix checkout redirect".into(),
            cwd: "/code/webshop".into(),
            status: "blocked".into(),
            raw_status: "blocked".into(),
        };
        let text = msg.render();
        assert!(text.starts_with("⛔ opencode is blocked"));
        assert!(text.contains("webshop / api: fix checkout redirect"));
        assert!(text.contains("/code/webshop"));
        let seen = Message {
            status: "done".into(),
            raw_status: "idle".into(),
            ..msg
        };
        assert!(seen
            .render()
            .starts_with("✅ opencode is done (reported idle)"));
    }

    #[test]
    fn backoff_honors_retry_after_cap() {
        assert_eq!(
            backoff(429, r#"{"parameters":{"retry_after":30}}"#),
            Duration::from_secs(5)
        );
        assert_eq!(
            backoff(429, r#"{"parameters":{"retry_after":2}}"#),
            Duration::from_secs(2)
        );
        assert_eq!(backoff(429, "not json"), Duration::from_secs(1));
        assert_eq!(backoff(502, "{}"), Duration::from_secs(1));
    }

    #[test]
    fn minutes_of_day_handles_negative_offsets() {
        // UTC 02:00 at UTC-8 must be 18:00 local, not a wrapped garbage value.
        assert_eq!(minutes_of_day(2 * 3600, -8 * 3600), 18 * 60);
        // UTC 22:00 at UTC-5 is 17:00 local.
        assert_eq!(minutes_of_day(22 * 3600, -5 * 3600), 17 * 60);
        // Positive offsets and zero stay exact.
        assert_eq!(minutes_of_day(21 * 3600, 3 * 3600), 0);
        assert_eq!(minutes_of_day(9 * 3600 + 30 * 60, 0), 9 * 60 + 30);
    }

    #[test]
    fn parse_offset_reads_sign_and_digits() {
        assert_eq!(parse_offset("+0300"), Some(3 * 3600));
        assert_eq!(parse_offset("-0530"), Some(-(5 * 3600 + 30 * 60)));
        assert_eq!(parse_offset("+0000"), Some(0));
        assert_eq!(parse_offset("garbage"), None);
        assert_eq!(parse_offset(""), None);
    }

    #[test]
    fn redact_token_scrubs_every_occurrence() {
        assert_eq!(
            redact_token(
                "https://api.telegram.org/bot123:abc/sendMessage: Dns Failed",
                "123:abc"
            ),
            "https://api.telegram.org/bot[redacted]/sendMessage: Dns Failed"
        );
        assert_eq!(redact_token("no secret here", "123:abc"), "no secret here");
        assert_eq!(redact_token("anything", ""), "anything");
    }

    #[test]
    fn parses_multiline_arrays_and_section_comments() {
        let cfg = config(
            r#"
include_workspaces = [
    "webshop",   # main project
    "api",
]

[telegram]  # credentials
bot_token = "123:abc"
chat_id = "42"
"#,
        );
        assert_eq!(
            cfg.include_workspaces,
            vec!["webshop".to_string(), "api".to_string()]
        );
        assert!(cfg.telegram_ready());
    }
}
