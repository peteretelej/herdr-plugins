//! peteretelej.opencode-cost-tokens - live OpenCode session telemetry per
//! herdr agent pane.
//!
//! Attribution: `herdr pane list` maps panes to OpenCode session ids (herdr's
//! OpenCode integration). Data: read-only SQLite against OpenCode's local
//! store. Display: `herdr pane report-metadata` sidebar tokens. Liveness: a
//! detached watcher polls only while panes are working, plus a short settle
//! tail so final numbers catch DB lag after a turn ends.

use anyhow::{bail, Context as _, Result};
use clap::Parser;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod db;
use db::{load_models, OpenCodeDb};

const SOURCE_ID: &str = "peteretelej.opencode-cost-tokens";
const TTL_MS: &str = "86400000";
const POLL_SECS: u64 = 2;
const LOCK_FRESH_MS: u128 = 5_000;
const SETTLE_MS: u128 = 20_000;
const MAX_WATCH_MS: u128 = 30 * 60 * 1_000;

/// Sidebar tokens published per pane. Values are display-ready text.
/// Context uses three severity names (normal / warn / hot at 60% / 85%);
/// exactly one is reported per pass, and unreported token names simply
/// disappear from the sidebar row - herdr's severity pattern.
const TOK_CONTEXT: &str = "oct_cx";
const TOK_CONTEXT_WARN: &str = "oct_cx_warn";
const TOK_CONTEXT_HOT: &str = "oct_cx_hot";
const TOK_RATE: &str = "oct_tps";
const TOK_COST: &str = "oct_cost";

type Tokens = BTreeMap<String, String>;
type State = BTreeMap<String, Tokens>;

#[derive(Parser)]
#[command(name = "opencode-cost-tokens", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Event hook entry: refresh the pane named in the event, wake the watcher when working.
    Event,
    /// Refresh every OpenCode pane with a session (startup + manual action).
    Refresh,
    /// Poll working panes until they settle (spawned detached; not for hands).
    Watch,
    /// Stop the watcher.
    Stop,
}

#[derive(Clone)]
struct Pane {
    pane_id: String,
    status: Option<String>,
    session: Option<String>,
}

fn herdr() -> Command {
    let bin = std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into());
    Command::new(bin)
}

fn millis() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

fn state_dir() -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var("HERDR_PLUGIN_STATE_DIR").context("HERDR_PLUGIN_STATE_DIR not set")?,
    ))
}

fn panes() -> Result<Vec<Pane>> {
    let out = herdr().args(["pane", "list"]).output()?;
    let v: Value = serde_json::from_slice(&out.stdout)?;
    let mut result = Vec::new();
    let Some(list) = v.pointer("/result/panes").and_then(Value::as_array) else {
        return Ok(result);
    };
    for p in list {
        if p.get("agent").and_then(Value::as_str) != Some("opencode") {
            continue;
        }
        let session = p
            .get("agent_session")
            .filter(|s| s.get("kind").and_then(Value::as_str) == Some("id"))
            .and_then(|s| s.get("value"))
            .and_then(Value::as_str)
            .map(String::from);
        result.push(Pane {
            pane_id: p.get("pane_id").and_then(Value::as_str).unwrap_or_default().into(),
            status: p.get("agent_status").and_then(Value::as_str).map(String::from),
            session,
        });
    }
    Ok(result)
}

fn compact(n: i64) -> String {
    let n = n.max(0) as f64;
    if n >= 1_000_000.0 {
        format!("{:.1}M", n / 1_000_000.0)
    } else if n >= 1_000.0 {
        format!("{:.1}k", n / 1_000.0)
    } else {
        format!("{}", n as i64)
    }
}

/// Dollars with trailing zeros trimmed: $2.30 -> $2.3, $1.00 -> $1.
fn compact_dollars(cost: f64) -> String {
    let mut s = format!("${cost:.2}");
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    s
}

/// Sidebar token name for a context-used percentage. Severity buckets:
/// normal < 60% <= warn < 85% <= hot.
fn context_token_name(pct: i64) -> &'static str {
    if pct >= 85 {
        TOK_CONTEXT_HOT
    } else if pct >= 60 {
        TOK_CONTEXT_WARN
    } else {
        TOK_CONTEXT
    }
}

/// Sidebar token values for one session. Empty map when the session has no
/// completed turn yet; nothing is published for unknown sessions. Err is a
/// transient read failure: the caller must skip the pane and leave its
/// current tokens alone.
fn session_tokens(
    conn: &OpenCodeDb,
    models: &Option<Value>,
    session_id: &str,
) -> anyhow::Result<Tokens> {
    let mut tokens = Tokens::new();
    let Some(totals) = conn.session_totals(session_id)? else {
        return Ok(tokens);
    };
    let Some(sample) = conn.last_completed_sample(session_id)? else {
        return Ok(tokens);
    };

    let context = sample.context_tokens();
    let limit = sample
        .provider_id
        .as_deref()
        .zip(sample.model_id.as_deref())
        .and_then(|(p, m)| models.as_ref().and_then(|cat| OpenCodeDb::context_limit(cat, p, m)));
    match limit {
        Some(limit) if limit > 0 && context > 0 => {
            let pct = context * 100 / limit;
            tokens.insert(context_token_name(pct).into(), format!("{pct}%"));
        }
        _ => {
            tokens.insert(TOK_CONTEXT.into(), compact(context));
        }
    };

    if let Some(rate) = sample.tokens_per_second() {
        tokens.insert(TOK_RATE.into(), format!("{rate}t/s"));
    }
    // Cost only when it exists: OpenCode records $0 for subscription-billed
    // providers, and a permanent $0.00 in every row is noise.
    if totals.cost >= 0.005 {
        tokens.insert(TOK_COST.into(), compact_dollars(totals.cost));
    }
    Ok(tokens)
}

/// (tokens to write, token names to clear) between the previous and desired maps.
fn diff(prev: &Tokens, next: &Tokens) -> (Vec<(String, String)>, Vec<String>) {
    let mut puts = Vec::new();
    let mut clears = Vec::new();
    for (k, v) in next {
        if prev.get(k) != Some(v) {
            puts.push((k.clone(), v.clone()));
        }
    }
    for k in prev.keys() {
        if !next.contains_key(k) {
            clears.push(k.clone());
        }
    }
    (puts, clears)
}

fn load_state(path: &Path) -> State {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_state(path: &Path, state: &State) -> Result<()> {
    // Unique tmp per process: hooks and the watcher can save concurrently.
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp, serde_json::to_vec(state)?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// One reconcile pass over the given panes. Returns true when any metadata
/// was written (the no-op-write guard: unchanged panes cost zero CLI calls).
/// `force` republishes every token regardless of the diff gate - the
/// manual/startup path, so dropped metadata (TTL expiry, server restart) is
/// restored even when values are unchanged.
fn refresh_pass(
    list: &[Pane],
    only: Option<&str>,
    db: &OpenCodeDb,
    state: &mut State,
    force: bool,
) -> Result<bool> {
    let models = load_models();
    let mut wrote = false;
    for pane in list {
        if let Some(want) = only {
            if pane.pane_id != want {
                continue;
            }
        }
        let Some(session) = pane.session.as_deref() else { continue };
        // Transient DB failure: leave this pane's current tokens untouched.
        let Ok(tokens) = session_tokens(db, &models, session) else { continue };
        let empty = Tokens::new();
        let prev = state.get(&pane.pane_id).unwrap_or(&empty);
        let (mut puts, clears) = diff(prev, &tokens);
        if force {
            // Republish every desired token, but keep the diff's clears so
            // tokens dropped between versions (or states) still get removed.
            puts = tokens.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        }
        if puts.is_empty() && clears.is_empty() {
            continue;
        }
        let mut cmd = herdr();
        cmd.args([
            "pane",
            "report-metadata",
            &pane.pane_id,
            "--source",
            SOURCE_ID,
            "--seq",
            &millis().to_string(),
            "--ttl-ms",
            TTL_MS,
        ]);
        for (k, v) in &puts {
            cmd.arg("--token").arg(format!("{k}={v}"));
        }
        for k in &clears {
            cmd.arg("--clear-token").arg(k);
        }
        let out = cmd.output()?;
        if !out.status.success() {
            bail!("report-metadata failed for {}: {}", pane.pane_id, String::from_utf8_lossy(&out.stderr));
        }
        state.insert(pane.pane_id.clone(), tokens);
        wrote = true;
    }
    Ok(wrote)
}

fn cmd_refresh() -> Result<()> {
    let Some(db) = OpenCodeDb::open() else {
        return Ok(());
    };
    let list = panes()?;
    let dir = state_dir()?;
    fs::create_dir_all(&dir)?;
    let mut state = load_state(&dir.join("published.json"));

    // Panes that vanished: drop our bookkeeping; herdr discards their metadata with them.
    let live: BTreeSet<&str> = list.iter().map(|p| p.pane_id.as_str()).collect();
    state.retain(|pane_id, _| live.contains(pane_id.as_str()));

    refresh_pass(&list, None, &db, &mut state, true)?;
    save_state(&dir.join("published.json"), &state)?;
    if list.iter().any(|p| p.status.as_deref() == Some("working") && p.session.is_some()) {
        ensure_watcher(&dir);
    }
    Ok(())
}

fn cmd_event() -> Result<()> {
    let name = std::env::var("HERDR_PLUGIN_EVENT").unwrap_or_default();
    if name != "pane.agent_status_changed" && name != "pane.agent_detected" {
        return Ok(());
    }
    let raw = std::env::var("HERDR_PLUGIN_EVENT_JSON").unwrap_or_default();
    let event: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    let pane_id = event.pointer("/data/pane_id").and_then(Value::as_str).map(String::from);
    let status = event.pointer("/data/agent_status").and_then(Value::as_str).map(String::from);

    if let Some(pane_id) = pane_id {
        if let Some(db) = OpenCodeDb::open() {
            let dir = state_dir()?;
            fs::create_dir_all(&dir)?;
            let mut state = load_state(&dir.join("published.json"));
            let list = panes().unwrap_or_default();
            // One pane per event: state changes can burst; never scan the fleet per hook.
            refresh_pass(&list, Some(&pane_id), &db, &mut state, false)?;
            save_state(&dir.join("published.json"), &state)?;
        }
    }
    if status.as_deref() == Some("working") {
        if let Ok(dir) = state_dir() {
            ensure_watcher(&dir);
        }
    }
    Ok(())
}

fn lock_token(path: &Path) -> Option<String> {
    let age = fs::metadata(path).ok()?.modified().ok()?.elapsed().ok()?;
    if age.as_millis() > LOCK_FRESH_MS {
        return None;
    }
    fs::read_to_string(path).ok()
}

fn ensure_watcher(dir: &Path) {
    let lock = dir.join("watch.lock");
    if lock_token(&lock).is_some() {
        return;
    }
    // A stale stop file would kill the watcher we are about to spawn.
    let _ = fs::remove_file(dir.join("watch.stop"));
    // Exclusive claim: concurrent hooks (bursts are normal) race here, and
    // exactly one create_new wins; the loser sees a fresh lock and stands down.
    let token = format!("{}", millis());
    let claimed = fs::OpenOptions::new().write(true).create_new(true).open(&lock);
    let mut file = match claimed {
        Ok(file) => file,
        Err(_) => return,
    };
    use std::io::Write as _;
    if file.write_all(token.as_bytes()).is_err() {
        let _ = fs::remove_file(&lock);
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let _ = spawn_detached(&exe, &["watch"]);
}

fn spawn_detached(exe: &Path, args: &[&str]) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt as _;
    Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map(|_| ())
}

fn cmd_watch() -> Result<()> {
    let dir = state_dir()?;
    let lock = dir.join("watch.lock");
    let token = fs::read_to_string(&lock).unwrap_or_default();
    // Lost the claim race: a fresher watcher owns the lock.
    if token.is_empty() || lock_token(&lock).as_deref() != Some(token.trim()) {
        return Ok(());
    }
    let outcome = watch_loop(&dir, &lock, token.trim());
    // Cleanup on every exit path: stop, idle, lifetime cap, or error.
    let _ = fs::remove_file(&lock);
    outcome
}

fn watch_loop(dir: &Path, lock: &Path, token: &str) -> Result<()> {
    let stop = dir.join("watch.stop");
    let started = Instant::now();
    let mut seen_working: BTreeMap<String, Instant> = BTreeMap::new();
    let mut list_failures = 0u32;

    loop {
        let _ = fs::write(lock, token);
        if stop.exists() {
            let _ = fs::remove_file(&stop);
            break;
        }
        if started.elapsed().as_millis() > MAX_WATCH_MS {
            break;
        }
        let now = Instant::now();
        // A transient `pane list` failure must not end the watch mid-turn:
        // ride it out for a few cycles before giving up.
        let list = match panes() {
            Ok(list) => {
                list_failures = 0;
                list
            }
            Err(_) if list_failures < 5 => {
                list_failures += 1;
                thread::sleep(Duration::from_secs(POLL_SECS));
                continue;
            }
            Err(e) => return Err(e),
        };
        for pane in &list {
            if pane.status.as_deref() == Some("working") && pane.session.is_some() {
                seen_working.insert(pane.pane_id.clone(), now);
            }
        }
        seen_working.retain(|_, t| now.duration_since(*t).as_millis() <= SETTLE_MS);
        // Targets: working panes plus a settle tail so the last turn's final
        // numbers are picked up even if they landed in SQLite after the
        // done/idle status event.
        let targets: Vec<Pane> = list
            .iter()
            .filter(|p| {
                p.session.is_some()
                    && (p.status.as_deref() == Some("working") || seen_working.contains_key(&p.pane_id))
            })
            .cloned()
            .collect();
        if targets.is_empty() {
            break;
        }
        // One failing publish must not kill the loop, either.
        if let Some(db) = OpenCodeDb::open() {
            let mut state = load_state(&dir.join("published.json"));
            match refresh_pass(&targets, None, &db, &mut state, false) {
                Ok(true) => save_state(&dir.join("published.json"), &state)?,
                Ok(false) => {}
                Err(_) => {}
            }
        }
        thread::sleep(Duration::from_secs(POLL_SECS));
    }
    Ok(())
}

fn cmd_stop() -> Result<()> {
    let dir = state_dir()?;
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("watch.stop"), "stop")?;
    println!("stop requested");
    Ok(())
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Event => cmd_event(),
        Cmd::Refresh => cmd_refresh(),
        Cmd::Watch => cmd_watch(),
        Cmd::Stop => cmd_stop(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> Tokens {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn diff_writes_changes_and_clears_removals() {
        let prev = map(&[("oct_cx", "10%"), ("oct_cost", "$0.10")]);
        let next = map(&[("oct_cx", "12%"), ("oct_out", "900")]);
        let (puts, clears) = diff(&prev, &next);
        assert_eq!(puts, vec![("oct_cx".into(), "12%".into()), ("oct_out".into(), "900".into())]);
        assert_eq!(clears, vec!["oct_cost".to_string()]);
    }

    #[test]
    fn diff_no_change_costs_nothing() {
        let t = map(&[("oct_cx", "10%")]);
        assert!(diff(&t, &t).0.is_empty() && diff(&t, &t).1.is_empty());
    }

    #[test]
    fn context_severity_switches_token_names() {
        assert_eq!(context_token_name(13), TOK_CONTEXT);
        assert_eq!(context_token_name(59), TOK_CONTEXT);
        assert_eq!(context_token_name(60), TOK_CONTEXT_WARN);
        assert_eq!(context_token_name(84), TOK_CONTEXT_WARN);
        assert_eq!(context_token_name(85), TOK_CONTEXT_HOT);
    }

    #[test]
    fn compact_dollars_trims_zeros() {
        assert_eq!(compact_dollars(2.30), "$2.3");
        assert_eq!(compact_dollars(1.0), "$1");
        assert_eq!(compact_dollars(0.07), "$0.07");
        assert_eq!(compact_dollars(12.5), "$12.5");
    }

    #[test]
    fn compact_formats() {
        assert_eq!(compact(42), "42");
        assert_eq!(compact(45_300), "45.3k");
        assert_eq!(compact(1_234_000), "1.2M");
        assert_eq!(compact(-5), "0");
    }
}
