//! Read-only access to OpenCode's local SQLite store (`opencode.db`, v2 schema).
//!
//! Only exact-id lookups; the database is opened read-only so the plugin can
//! never corrupt a live OpenCode install.

use anyhow::Result;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::path::PathBuf;

pub struct OpenCodeDb {
    conn: Connection,
}

/// Session-level totals from the `session_v2` row.
/// `Ok(None)` = session unknown; `Err` = transient read failure, which
/// callers must treat as "skip this pane", never as "clear its tokens".
pub struct SessionTotals {
    pub cost: f64,
}

/// One completed assistant message, the unit for context % and tok/s.
pub struct AssistantSample {
    pub input: i64,
    pub output: i64,
    pub reasoning: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    /// v2 messages carry model identity as `model.id` (older builds: `modelID`).
    pub created: Option<i64>,
    pub completed: Option<i64>,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
}

impl AssistantSample {
    /// OpenCode's own context formula for a completed turn.
    pub fn context_tokens(&self) -> i64 {
        self.input + self.output + self.reasoning + self.cache_read + self.cache_write
    }

    /// Effective turn rate: output tokens over the turn's wall time (created
    /// to completed). `time.streamed` marks only the final flush in practice,
    /// so it is never a valid start anchor; require `created`.
    pub fn tokens_per_second(&self) -> Option<i64> {
        let completed = self.completed?;
        let start = self.created?;
        if start <= 0 || completed <= start || self.output <= 0 {
            return None;
        }
        Some((self.output as f64 / ((completed - start) as f64 / 1000.0)).round() as i64)
    }
}

fn db_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("OPENCODE_DB") {
        return Some(PathBuf::from(p));
    }
    let base = std::env::var("XDG_DATA_HOME")
        .ok()
        .or_else(|| std::env::var("HOME").ok().map(|h| format!("{h}/.local/share")))?;
    let p = PathBuf::from(base).join("opencode").join("opencode.db");
    p.exists().then_some(p)
}

impl OpenCodeDb {
    /// None when OpenCode's store is missing or cannot be opened read-only.
    pub fn open() -> Option<Self> {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(db_path()?, flags).ok()?;
        let _ = conn.busy_timeout(std::time::Duration::from_millis(500));
        Some(Self { conn })
    }

    /// Session-level totals from the `session_v2` row.
    /// `Ok(None)` = session unknown; `Err` = transient read failure, which
    /// callers must treat as "skip this pane", never as "clear its tokens".
    pub fn session_totals(&self, session_id: &str) -> rusqlite::Result<Option<SessionTotals>> {
        match self.conn.query_row(
            "SELECT cost FROM session_v2 WHERE id = ?1",
            [session_id],
            |row| Ok(SessionTotals { cost: row.get(0)? }),
        ) {
            Ok(totals) => Ok(Some(totals)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The newest completed assistant message, or `Ok(None)` when the session
    /// has none yet. `Err` is a transient read failure.
    pub fn last_completed_sample(&self, session_id: &str) -> rusqlite::Result<Option<AssistantSample>> {
        let mut stmt = self.conn.prepare(
            "SELECT data FROM session_message
             WHERE session_id = ?1 AND type = 'assistant'
             ORDER BY seq DESC LIMIT 8",
        )?;
        let rows: Vec<String> = stmt
            .query_map([session_id], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        let sample = rows
            .iter()
            .filter_map(|raw| parse_sample(raw))
            .find(|s| s.completed.is_some());
        Ok(sample)
    }

    /// Context window size for a model, from OpenCode's cached model catalog.
    pub fn context_limit(models: &Value, provider: &str, model: &str) -> Option<i64> {
        models
            .get(provider)?
            .get("models")?
            .get(model)?
            .get("limit")?
            .get("context")?
            .as_i64()
    }
}

fn parse_sample(raw: &str) -> Option<AssistantSample> {
    let v: Value = serde_json::from_str(raw).ok()?;
    let tokens = v.get("tokens")?;
    let num = |parent: Option<&Value>, key: &str| -> i64 {
        parent.and_then(|p| p.get(key)).and_then(Value::as_i64).unwrap_or(0)
    };
    let cache = tokens.get("cache");
    let time = v.get("time");
    Some(AssistantSample {
        input: num(Some(tokens), "input"),
        output: num(Some(tokens), "output"),
        reasoning: num(Some(tokens), "reasoning"),
        cache_read: num(cache, "read"),
        cache_write: num(cache, "write"),
        created: time.and_then(|t| t.get("created")).and_then(Value::as_i64),
        completed: time.and_then(|t| t.get("completed")).and_then(Value::as_i64),
        provider_id: v.pointer("/model/providerID").and_then(Value::as_str).map(String::from),
        model_id: v
            .pointer("/model/id")
            .or_else(|| v.pointer("/model/modelID"))
            .and_then(Value::as_str)
            .map(String::from),
    })
}

/// Load OpenCode's cached model catalog once per pass; None is fine and just
/// disables percentage context display.
pub fn load_models() -> Option<Value> {
    let path = std::env::var("OPENCODE_MODELS").ok().map(PathBuf::from).unwrap_or_else(|| {
        let base = std::env::var("XDG_CACHE_HOME").ok().unwrap_or_else(|| {
            format!("{}/.cache", std::env::var("HOME").unwrap_or_default())
        });
        PathBuf::from(base).join("opencode").join("models.json")
    });
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMPLETED: &str = r#"{
        "agent": "build", "cost": 0.0, "finish": "stop",
        "model": {"id": "glm-5.3-flash", "providerID": "zai-coding-plan", "variant": "max"},
        "snapshot": "abc",
        "time": {"created": 1000, "streamed": 2000, "completed": 11000},
        "tokens": {"input": 1000, "output": 900, "reasoning": 100, "cache": {"read": 50000, "write": 0}}
    }"#;

    #[test]
    fn parses_completed_sample() {
        let s = parse_sample(COMPLETED).expect("parses");
        assert_eq!(s.output, 900);
        assert_eq!(s.completed, Some(11000));
        assert_eq!(s.created, Some(1000));
        assert_eq!(s.provider_id.as_deref(), Some("zai-coding-plan"));
        assert_eq!(s.model_id.as_deref(), Some("glm-5.3-flash"));
        assert_eq!(s.context_tokens(), 1000 + 900 + 100 + 50000);
    }

    #[test]
    fn falls_back_to_legacy_modelid_key() {
        let raw = COMPLETED.replace(r#""id": "glm-5.3-flash", "providerID""#, r#""modelID": "glm-5.3-flash", "providerID""#);
        let s = parse_sample(&raw).expect("parses");
        assert_eq!(s.model_id.as_deref(), Some("glm-5.3-flash"));
    }

    #[test]
    fn skips_in_progress_sample() {
        // Verified live: in-progress rows have no tokens/finish keys at all.
        let raw = r#"{"agent":"build","content":[],"model":{"providerID":"p","modelID":"m"},"snapshot":"x","time":{"created":1000,"streamed":2000}}"#;
        assert!(parse_sample(raw).is_none());
    }

    #[test]
    fn rate_uses_effective_turn_window() {
        let s = parse_sample(COMPLETED).expect("parses");
        assert_eq!(s.tokens_per_second(), Some(90)); // 900 tokens over created->completed (10s)
    }

    #[test]
    fn rate_needs_completion() {
        let raw = COMPLETED.replace(r#""completed": 11000"#, r#""completed": null"#);
        let s = parse_sample(&raw).expect("parses");
        assert_eq!(s.tokens_per_second(), None);
    }


    #[test]
    fn context_limit_walks_the_catalog() {
        let catalog: Value = serde_json::from_str(
            r#"{"zai-coding-plan": {"models": {"glm-5.3-flash": {"limit": {"context": 1000000, "output": 131072}}}}}"#,
        )
        .unwrap();
        assert_eq!(
            OpenCodeDb::context_limit(&catalog, "zai-coding-plan", "glm-5.3-flash"),
            Some(1_000_000)
        );
        assert_eq!(OpenCodeDb::context_limit(&catalog, "nope", "glm-5.3-flash"), None);
    }
}
