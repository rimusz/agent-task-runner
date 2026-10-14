//! Driver review: approve a worker diff or send comments back.

use crate::config::Provider;
use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

pub const REVIEW_SCHEMA: &str = r#"{"type":"object","properties":{"verdict":{"type":"string","enum":["approve","request_changes"]},"comments":{"type":"string"}},"required":["verdict"]}"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Approve,
    RequestChanges(String),
}

#[derive(Debug, Clone, Deserialize)]
struct RawVerdict {
    verdict: String,
    #[serde(default)]
    comments: String,
}

pub fn review_prompt(key: &str, diff: &str) -> String {
    format!(
        "/review\n\
         Review the worker's changes for task `{key}`.\n\
         Return JSON matching this schema:\n{REVIEW_SCHEMA}\n\
         Approve only if the change is complete and correct.\n\
         If you request changes, comments must tell the worker what to fix.\n\n\
         Diff:\n```\n{diff}\n```"
    )
}

pub fn apply_review_args(p: &mut Provider, max_turns: u32) {
    p.plugin = false;
    p.extra_args.extend([
        "--max-turns".into(),
        max_turns.max(15).to_string(),
        "--output-format".into(),
        "json".into(),
        "--json-schema".into(),
        REVIEW_SCHEMA.into(),
    ]);
}

pub fn parse_verdict(text: &str) -> Result<Verdict> {
    let v = crate::plan::first_json_value(text).or_else(|_| first_json(text))?;
    if let Some(verdict) = from_value(&v) {
        return Ok(verdict);
    }
    for key in ["structuredOutput", "structured_output", "result"] {
        if let Some(inner) = v.get(key) {
            if let Some(verdict) = from_value(inner) {
                return Ok(verdict);
            }
            if let Some(s) = inner.as_str() {
                if let Ok(inner) = first_json(s) {
                    if let Some(verdict) = from_value(&inner) {
                        return Ok(verdict);
                    }
                }
            }
        }
    }
    anyhow::bail!("review result was not approve/request_changes")
}

fn from_value(v: &Value) -> Option<Verdict> {
    if let Ok(raw) = serde_json::from_value::<RawVerdict>(v.clone()) {
        return Some(normalize(&raw.verdict, &raw.comments));
    }
    if let Some(s) = v.as_str() {
        return Some(normalize(s, ""));
    }
    None
}

fn normalize(verdict: &str, comments: &str) -> Verdict {
    let v = verdict.trim().to_ascii_lowercase().replace('-', "_");
    if matches!(v.as_str(), "approve" | "approved" | "lgtm" | "ok") {
        Verdict::Approve
    } else {
        Verdict::RequestChanges(comments.trim().to_string())
    }
}

fn first_json(text: &str) -> Result<Value> {
    let t = text.trim();
    if let Ok(v) = serde_json::from_str::<Value>(t) {
        return Ok(v);
    }
    if let Some(block) = crate::plan::fenced_json(t) {
        if let Ok(v) = serde_json::from_str::<Value>(block) {
            return Ok(v);
        }
    }
    anyhow::bail!("no JSON object in review output")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_approve_and_changes() {
        assert_eq!(
            parse_verdict(r#"{"verdict":"approve"}"#).unwrap(),
            Verdict::Approve
        );
        match parse_verdict(
            "```json\n{\"verdict\":\"request_changes\",\"comments\":\"fix tests\"}\n```",
        )
        .unwrap()
        {
            Verdict::RequestChanges(c) => assert!(c.contains("fix tests")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn grok_structured_output() {
        let raw = r#"{"structuredOutput":{"verdict":"approve","comments":""},"sessionId":"s"}"#;
        assert_eq!(parse_verdict(raw).unwrap(), Verdict::Approve);
    }
}
