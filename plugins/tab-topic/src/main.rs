use anyhow::{bail, Context as _, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::process::Command;

/// Name herdr tabs (and optionally panes) after the agent's live topic.
#[derive(Parser)]
#[command(name = "tab-topic", version)]
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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            rename_panes: false,
            tab_format: "{n}. {topic}".into(),
            max_len: 60,
            strip_prefixes: vec![],
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
                "strip_prefixes" => {
                    cfg.strip_prefixes = value
                        .trim_start_matches('[')
                        .trim_end_matches(']')
                        .split(',')
                        .map(|s| s.trim().trim_matches('"').to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
                _ => {}
            }
        }
        cfg
    }
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
            .unwrap_or_else(|| std::env::temp_dir().join("peteretelej-tab-topic"));
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

fn render(template: &str, topic: &str, agent: &str, n: &str, max_len: usize) -> String {
    let label = template
        .replace("{topic}", topic)
        .replace("{agent}", agent)
        .replace("{n}", n);
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

fn clean_topic(pane: &serde_json::Value, strip: &[String]) -> Option<String> {
    let raw = pane["terminal_title_stripped"]
        .as_str()
        .or_else(|| pane["terminal_title"].as_str())?
        .trim();
    let mut topic = raw.to_string();
    for prefix in strip {
        if let Some(stripped) = topic.strip_prefix(prefix.as_str()) {
            topic = stripped.trim_start().to_string();
            break;
        }
    }
    (!topic.is_empty()).then_some(topic)
}

fn reconcile(rename_panes: bool, cfg: &Config) -> Result<()> {
    let mut state = State::load();

    let panes = herdr_json(&["pane", "list"])?;
    let tabs = herdr_json(&["tab", "list"])?;
    let empty = Vec::new();
    let pane_rows = panes
        .pointer("/result/panes")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);
    let tab_rows = tabs
        .pointer("/result/tabs")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);

    // tab_id -> (switch number, current label)
    let mut tab_info: BTreeMap<&str, (String, Option<&str>)> = BTreeMap::new();
    for tab in tab_rows {
        let Some(id) = tab["tab_id"].as_str() else {
            continue;
        };
        tab_info.insert(
            id,
            (
                tab["number"].as_u64().map(|n| n.to_string()).unwrap_or_default(),
                tab["label"].as_str(),
            ),
        );
    }

    // tab_id -> first agent pane (list order approximates reading order)
    let mut topic_by_tab: BTreeMap<&str, (String, String)> = BTreeMap::new(); // (topic, agent)
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
        let Some(topic) = clean_topic(pane, &cfg.strip_prefixes) else {
            continue;
        };
        if topic_by_tab.contains_key(tab_id) {
            continue;
        }
        topic_by_tab.insert(tab_id, (topic, agent.to_string()));
    }

    for (tab_id, (topic, agent)) in &topic_by_tab {
        let Some((number, current_label)) = tab_info.get(*tab_id) else {
            continue;
        };
        let desired = render(&cfg.tab_format, topic, agent, number, cfg.max_len);
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
            let Some(topic) = clean_topic(pane, &cfg.strip_prefixes) else {
                continue;
            };
            let desired = render("{topic}", &topic, agent, "", cfg.max_len);
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
