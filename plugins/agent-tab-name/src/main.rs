use anyhow::{bail, Context as _, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::process::Command;

/// Name herdr tabs (and optionally panes) after a short project-feature slug
/// derived from the OpenCode session topic.
#[derive(Parser)]
#[command(name = "agent-tab-name", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Event hook entry. The event name is ignored: reconcile is idempotent.
    Run,
    /// Manual reconcile pass.
    Sync,
    /// Stop managing labels and forget state.
    Release {
        /// Return managed tabs to their switch number and clear pane labels.
        #[arg(long)]
        clear: bool,
    },
}

#[derive(Deserialize)]
#[serde(default)]
struct Config {
    rename_panes: bool,
    tab_format: String,
    max_len: usize,
    strip_prefixes: Vec<String>,
    max_topic_words: usize,
    project_slugs: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            rename_panes: false,
            tab_format: "{project}-{topic}".into(),
            max_len: 28,
            strip_prefixes: vec![],
            max_topic_words: 3,
            project_slugs: BTreeMap::new(),
        }
    }
}

impl Config {
    fn load() -> Self {
        let Some(dir) = std::env::var_os("HERDR_PLUGIN_CONFIG_DIR") else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(PathBuf::from(dir).join("config.toml")) else {
            return Self::default();
        };
        // Flat `key = value` TOML; enough for this config, no toml dependency.
        let mut cfg = Config::default();
        for line in text.lines() {
            let line = line.trim();
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            match key {
                "rename_panes" => cfg.rename_panes = value == "true",
                "tab_format" => cfg.tab_format = value.trim_matches('"').to_string(),
                "max_len" => cfg.max_len = value.parse().unwrap_or(cfg.max_len),
                "max_topic_words" => {
                    cfg.max_topic_words = value.parse().unwrap_or(cfg.max_topic_words)
                }
                "strip_prefixes" => {
                    cfg.strip_prefixes = parse_string_list(value);
                }
                "project_slugs" => {
                    cfg.project_slugs = parse_string_map(value);
                }
                _ => {}
            }
        }
        cfg
    }
}

fn parse_string_list(value: &str) -> Vec<String> {
    value
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn parse_string_map(value: &str) -> BTreeMap<String, String> {
    let inner = value.trim_start_matches('{').trim_end_matches('}');
    let mut map = BTreeMap::new();
    for pair in inner.split(',') {
        let Some((key, val)) = pair.split_once('=') else {
            continue;
        };
        let key = key.trim().trim_matches('"');
        let val = val.trim().trim_matches('"');
        if !key.is_empty() && !val.is_empty() {
            map.insert(key.to_string(), val.to_string());
        }
    }
    map
}

#[derive(Serialize, Deserialize, Default)]
struct State {
    /// pane_id -> last pane label written by us
    panes: BTreeMap<String, String>,
    /// tab_id -> last tab label written by us
    tabs: BTreeMap<String, String>,
}

impl State {
    fn path() -> PathBuf {
        let dir = std::env::var_os("HERDR_PLUGIN_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("peteretelej-agent-tab-name"));
        dir.join("state.json")
    }

    fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn save(&self) -> Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string(self)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

fn herdr() -> Command {
    Command::new(
        std::env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".into()),
    )
}

/// Run a herdr command and parse its JSON stdout.
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

/// Lowercase, collapse non-alphanumeric runs into dashes, trim edges.
fn slugify(text: &str) -> String {
    let mut slug = String::with_capacity(text.len());
    let mut dash = false;
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            dash = false;
        } else if !dash && !slug.is_empty() {
            slug.push('-');
            dash = true;
        }
    }
    slug.trim_matches('-').to_string()
}

/// Slugify a topic and keep only the first `max_words` words.
fn topic_slug(raw: &str, strip: &[String], max_words: usize) -> Option<String> {
    let mut topic = raw.trim().to_string();
    for prefix in strip {
        if let Some(stripped) = topic.strip_prefix(prefix.as_str()) {
            topic = stripped.trim_start().to_string();
            break;
        }
    }
    let slug = slugify(&topic);
    if slug.is_empty() {
        return None;
    }
    let capped: Vec<&str> = slug.split('-').take(max_words).collect();
    Some(capped.join("-"))
}

/// Project slug for a workspace: configured short form, else the slugified
/// label. Empty when there is nothing to show.
fn project_slug(workspace_label: Option<&str>, cfg: &Config) -> String {
    let Some(label) = workspace_label.map(str::trim).filter(|l| !l.is_empty()) else {
        return String::new();
    };
    match cfg.project_slugs.get(label) {
        Some(short) => short.clone(),
        None => slugify(label),
    }
}

fn render(template: &str, project: &str, topic: &str, agent: &str, n: &str, max_len: usize) -> String {
    let mut label = template
        .replace("{project}", project)
        .replace("{topic}", topic)
        .replace("{agent}", agent)
        .replace("{n}", n);
    if project.is_empty() {
        // Collapse the empty project slot: "{project}-{topic}" -> "{topic}".
        label = label.replace("{project}-", "").replace("{project}", "");
    }
    while label.contains("--") {
        label = label.replace("--", "-");
    }
    let label = label.trim_matches('-').to_string();
    if label.chars().count() > max_len {
        let trimmed: String = label.chars().take(max_len.saturating_sub(1)).collect();
        format!("{trimmed}\u{2026}")
    } else {
        label
    }
}

/// A label is ours to manage when it is unset/default, one we wrote earlier,
/// or exactly what we would write now. Anything else belongs to the user.
fn owned(current: Option<&str>, default: Option<&str>, ours: Option<&str>, desired: &str) -> bool {
    match current {
        None => true,
        Some(current) => {
            let current = current.trim();
            current.is_empty()
                || current == desired
                || default.is_some_and(|d| d == current)
                || ours.is_some_and(|o| o == current)
        }
    }
}

fn reconcile(rename_panes: bool, cfg: &Config) -> Result<()> {
    let mut state = State::load();

    let panes = herdr_json(&["pane", "list"])?;
    let tabs = herdr_json(&["tab", "list"])?;
    let workspaces = herdr_json(&["workspace", "list"])?;
    let empty = Vec::new();
    let pane_rows = panes
        .pointer("/result/panes")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);
    let tab_rows = tabs
        .pointer("/result/tabs")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);
    let ws_rows = workspaces
        .pointer("/result/workspaces")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);

    let ws_label: BTreeMap<&str, &str> = ws_rows
        .iter()
        .filter_map(|w| Some((w["workspace_id"].as_str()?, w["label"].as_str()?)))
        .collect();

    // tab_id -> (switch position, current label, workspace_id). The default
    // tab label is the 1-based switch position within the workspace (compact
    // as tabs open/close), NOT the persistent number - positions must be
    // computed from list order.
    let mut tab_info: BTreeMap<&str, (String, Option<&str>, &str)> = BTreeMap::new();
    let mut switch_counter: BTreeMap<&str, usize> = BTreeMap::new();
    for tab in tab_rows {
        let Some(id) = tab["tab_id"].as_str() else {
            continue;
        };
        let ws = tab["workspace_id"].as_str().unwrap_or_default();
        let pos = switch_counter.entry(ws).or_insert(0);
        *pos += 1;
        tab_info.insert(
            id,
            (
                pos.to_string(),
                tab["label"].as_str(),
                ws,
            ),
        );
    }

    // tab_id -> (topic slug, agent) from its first agent pane (list order
    // approximates reading order).
    let mut topic_by_tab: BTreeMap<&str, (String, String)> = BTreeMap::new();
    let mut seen_panes: HashSet<String> = HashSet::new();
    for pane in pane_rows {
        let Some(agent) = pane["agent"].as_str() else {
            continue;
        };
        let (Some(pane_id), Some(tab_id)) = (pane["pane_id"].as_str(), pane["tab_id"].as_str())
        else {
            continue;
        };
        seen_panes.insert(pane_id.to_string());
        if topic_by_tab.contains_key(tab_id) {
            continue;
        }
        let Some(topic) = topic_slug(
            pane["terminal_title_stripped"]
                .as_str()
                .or_else(|| pane["terminal_title"].as_str())
                .unwrap_or_default(),
            &cfg.strip_prefixes,
            cfg.max_topic_words,
        ) else {
            continue;
        };
        topic_by_tab.insert(tab_id, (topic, agent.to_string()));
    }

    for (tab_id, (topic, agent)) in &topic_by_tab {
        let Some((number, current_label, workspace_id)) = tab_info.get(tab_id) else {
            continue;
        };
        let project = project_slug(ws_label.get(*workspace_id).copied(), cfg);
        let desired = render(&cfg.tab_format, &project, topic, agent, number, cfg.max_len);
        if desired.is_empty() {
            continue;
        }
        if !owned(
            *current_label,
            Some(number.as_str()),
            state.tabs.get(*tab_id).map(String::as_str),
            &desired,
        ) {
            continue;
        }
        if Some(desired.as_str()) != *current_label {
            herdr_json(&["tab", "rename", tab_id, &desired])?;
        }
        state.tabs.insert(tab_id.to_string(), desired);
    }

    if rename_panes {
        for pane in pane_rows {
            let (Some(pane_id), Some(agent)) = (pane["pane_id"].as_str(), pane["agent"].as_str())
            else {
                continue;
            };
            let Some(topic) = topic_slug(
                pane["terminal_title_stripped"]
                    .as_str()
                    .or_else(|| pane["terminal_title"].as_str())
                    .unwrap_or_default(),
                &cfg.strip_prefixes,
                cfg.max_topic_words,
            ) else {
                continue;
            };
            let project = project_slug(
                ws_label.get(pane["workspace_id"].as_str().unwrap_or_default()).copied(),
                cfg,
            );
            let desired = render("{topic}", &project, &topic, agent, "", cfg.max_len);
            if desired.is_empty() {
                continue;
            }
            let get = herdr_json(&["pane", "get", pane_id])?;
            let current = get["result"]["label"].as_str();
            if !owned(
                current,
                None,
                state.panes.get(pane_id).map(String::as_str),
                &desired,
            ) {
                continue;
            }
            if Some(desired.as_str()) != current {
                herdr_json(&["pane", "rename", pane_id, &desired])?;
            }
            state.panes.insert(pane_id.to_string(), desired);
        }
    }

    // Forget entries for panes/tabs that no longer exist.
    let live_tabs: HashSet<&str> = tab_info.keys().copied().collect();
    state.tabs.retain(|id, _| live_tabs.contains(id.as_str()));
    state.panes.retain(|id, _| seen_panes.contains(id));

    state.save()?;
    Ok(())
}

fn release(clear: bool) -> Result<()> {
    let mut state = State::load();
    if clear {
        let tabs = herdr_json(&["tab", "list"])?;
        for tab_id in state.tabs.keys().cloned().collect::<Vec<_>>() {
            let number = tabs
                .pointer("/result/tabs")
                .and_then(|v| v.as_array())
                .and_then(|rows| {
                    rows.iter().find_map(|t| {
                        (t["tab_id"].as_str() == Some(tab_id.as_str()))
                            .then(|| t["number"].as_u64().map(|n| n.to_string()))
                            .flatten()
                    })
                });
            if let Some(number) = number {
                herdr_json(&["tab", "rename", &tab_id, &number]).ok();
            }
            state.tabs.remove(&tab_id);
        }
        for pane_id in state.panes.keys().cloned().collect::<Vec<_>>() {
            herdr_json(&["pane", "rename", &pane_id, "--clear"]).ok();
            state.panes.remove(&pane_id);
        }
    }
    state.save()?;
    println!("released");
    Ok(())
}

fn main() -> Result<()> {
    let cfg = Config::load();
    match Cli::parse().cmd {
        Cmd::Run | Cmd::Sync => reconcile(cfg.rename_panes, &cfg),
        Cmd::Release { clear } => release(clear),
    }
}
