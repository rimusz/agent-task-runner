//! Shared HTTP/MCP intake: idempotent create and worktree file attach.
//!
//! These are additive on the existing `add_task` / `POST /tasks` path. atr
//! still does not hold vendor API keys; callers sign into CLIs themselves.

use crate::app::{Shared, SubmitOpts, SubmitOutcome};
use crate::task::{self, OutputKind, WorkspaceKind};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Max files on one create. Small on purpose — this is a prompt companion,
/// not a blob store.
pub const MAX_FILES: usize = 16;
const MAX_IDEM_KEY: usize = 256;
const MAX_NAME: usize = 255;
const ABSOLUTE_FILE_CAP: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct AttachFile {
    pub name: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Deserialize, Default)]
pub struct FileIn {
    #[serde(default, alias = "filename")]
    pub name: Option<String>,
    #[serde(default, alias = "content", alias = "contentBase64")]
    pub content_base64: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct CreateIn {
    pub body: String,
    pub title: Option<String>,
    pub key: Option<String>,
    pub repo: Option<String>,
    pub dir: Option<String>,
    pub workspace: Option<String>,
    pub output: Option<String>,
    pub webhook: Option<String>,
    pub patch: Option<String>,
    pub provider: Option<String>,
    pub needs: Vec<String>,
    pub source: String,
    pub source_kind: String,
    pub issue: Option<i64>,
    pub shadow: bool,
    pub tests_ok: bool,
    pub files: Vec<AttachFile>,
    pub idempotency_key: Option<String>,
    /// MCP uses the watch-repo name as the allocated-key prefix when that
    /// repo is not the shorthand `repo_path` target.
    pub repo_named_prefix: bool,
}

#[derive(Debug, Clone)]
pub struct CreateOut {
    pub key: String,
    pub title: String,
    pub workspace: String,
    pub output: String,
    pub status: &'static str,
    pub replayed: bool,
}

impl CreateOut {
    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "ok": true,
            "key": self.key,
            "status": self.status,
            "title": self.title,
            "workspace": self.workspace,
            "output": self.output,
        });
        if self.replayed {
            v["replayed"] = json!(true);
        }
        v
    }
}

#[derive(Debug)]
pub enum CreateError {
    BadRequest(String),
    Conflict(String),
    Internal(anyhow::Error),
}

impl CreateError {
    pub fn as_str(&self) -> String {
        match self {
            Self::BadRequest(s) | Self::Conflict(s) => s.clone(),
            Self::Internal(e) => e.to_string(),
        }
    }
}

impl From<anyhow::Error> for CreateError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e)
    }
}

/// Header `Idempotency-Key` wins; otherwise the JSON/MCP field.
pub fn pick_idempotency_key(header: Option<&str>, field: Option<&str>) -> Option<String> {
    header
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            field
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
}

pub fn validate_idempotency_key(raw: &str) -> Result<(), CreateError> {
    let t = raw.trim();
    if t.is_empty() {
        return Err(CreateError::BadRequest("idempotency key is empty".into()));
    }
    if t.len() > MAX_IDEM_KEY {
        return Err(CreateError::BadRequest(format!(
            "idempotency key is {} bytes; max is {MAX_IDEM_KEY}",
            t.len()
        )));
    }
    if t.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err(CreateError::BadRequest(
            "idempotency key must be a single line of printable text".into(),
        ));
    }
    Ok(())
}

pub fn parse_files(files: Vec<FileIn>, max_prompt_bytes: u64) -> Result<Vec<AttachFile>, String> {
    if files.len() > MAX_FILES {
        return Err(format!(
            "too many files ({}); max is {MAX_FILES}",
            files.len()
        ));
    }
    let per = file_byte_cap(max_prompt_bytes);
    let total_cap = if max_prompt_bytes == 0 {
        ABSOLUTE_FILE_CAP
    } else {
        max_prompt_bytes.saturating_mul(4).min(ABSOLUTE_FILE_CAP)
    };
    let mut out = Vec::with_capacity(files.len());
    let mut total = 0u64;
    let mut seen = std::collections::BTreeSet::new();
    for f in files {
        let name = sanitize_filename(f.name.as_deref().unwrap_or(""))?;
        if !seen.insert(name.clone()) {
            return Err(format!("duplicate file name {name}"));
        }
        let has_b64 = f
            .content_base64
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty());
        let has_text = f.text.is_some();
        if has_b64 && has_text {
            return Err(format!(
                "file {name} must set content_base64 or text, not both"
            ));
        }
        let bytes = if has_text {
            f.text.unwrap_or_default().into_bytes()
        } else if has_b64 {
            decode_base64(f.content_base64.as_deref().unwrap_or(""))
                .map_err(|e| format!("file {name}: {e}"))?
        } else {
            return Err(format!("file {name} missing content_base64 or text"));
        };
        let n = bytes.len() as u64;
        if n > per {
            return Err(format!("file {name} is {n} bytes; max is {per}"));
        }
        total = total.saturating_add(n);
        if total > total_cap {
            return Err(format!("files total {total} bytes; max is {total_cap}"));
        }
        out.push(AttachFile { name, bytes });
    }
    Ok(out)
}

pub fn files_from_args(args: &Value, max_prompt_bytes: u64) -> Result<Vec<AttachFile>, String> {
    let v = match args.get("files").or_else(|| args.get("attachments")) {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(v) => v,
    };
    let parsed: Vec<FileIn> = serde_json::from_value(v.clone())
        .map_err(|e| format!("files must be an array of {{name, content_base64|text}}: {e}"))?;
    parse_files(parsed, max_prompt_bytes)
}

pub fn sanitize_filename(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("file name is empty".into());
    }
    if name.len() > MAX_NAME {
        return Err(format!(
            "file name is {} bytes; max is {MAX_NAME}",
            name.len()
        ));
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return Err("file name must be a single path component (no directories)".into());
    }
    if name == "." || name == ".." {
        return Err("file name must be a single path component (no directories)".into());
    }
    if name.eq_ignore_ascii_case(".git") || name.eq_ignore_ascii_case(".atr") {
        return Err(format!("refusing reserved file name {name}"));
    }
    Ok(name.to_string())
}

pub fn attachments_dir(state_db: &Path, key: &str) -> PathBuf {
    state_db
        .parent()
        .unwrap_or(state_db)
        .join("attachments")
        .join(key)
}

pub fn store_attachments(state_db: &Path, key: &str, files: &[AttachFile]) -> Result<()> {
    let dir = attachments_dir(state_db, key);
    if dir.exists() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    if files.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    for f in files {
        let dest = dir.join(&f.name);
        std::fs::write(&dest, &f.bytes)
            .with_context(|| format!("write attachment {}", dest.display()))?;
    }
    Ok(())
}

pub fn list_attachment_names(state_db: &Path, key: &str) -> Vec<String> {
    let dir = attachments_dir(state_db, key);
    let rd = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return Vec::new(),
    };
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

/// Copy stored files into the ready worktree, each under its filename.
pub fn materialize_attachments(state_db: &Path, key: &str, worktree: &Path) -> Result<()> {
    let src = attachments_dir(state_db, key);
    if !src.is_dir() {
        return Ok(());
    }
    for ent in std::fs::read_dir(&src).with_context(|| format!("read {}", src.display()))? {
        let ent = ent?;
        if !ent.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let name = ent.file_name();
        let name = name.to_string_lossy();
        let name = sanitize_filename(&name).map_err(|e| anyhow::anyhow!("{e}"))?;
        let dest = worktree.join(&name);
        if dest.exists() && dest.is_dir() {
            bail!("cannot attach {name}: {} is a directory", dest.display());
        }
        std::fs::copy(ent.path(), &dest)
            .with_context(|| format!("copy attachment {name} into {}", dest.display()))?;
    }
    Ok(())
}

pub fn remove_attachments(state_db: &Path, key: &str) {
    let dir = attachments_dir(state_db, key);
    let _ = std::fs::remove_dir_all(dir);
}

pub fn create(shared: &Shared, spec: CreateIn) -> Result<CreateOut, CreateError> {
    let body = spec.body.trim();
    if body.is_empty() {
        return Err(CreateError::BadRequest("missing body".into()));
    }
    let cfg = shared.config.read().unwrap().clone();
    task::check_prompt_bytes(body, cfg.max_prompt_bytes)
        .map_err(|e| CreateError::BadRequest(e.to_string()))?;
    let title = spec
        .title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| task::title_from_prompt(body));
    let (workspace_kind, repo_path, allow_empty) = task::resolve_intake_workspace(
        spec.dir.as_deref(),
        spec.repo.as_deref(),
        spec.workspace.as_deref(),
        |r| {
            cfg.resolve_repo_filter(r)
                .map(|e| e.path.display().to_string())
        },
    )
    .map_err(|e| CreateError::BadRequest(e.to_string()))?;
    let output_kind = match spec.output.as_deref() {
        Some(o) => OutputKind::parse(o).map_err(|e| CreateError::BadRequest(e.to_string()))?,
        None => OutputKind::Reply,
    };
    if output_kind.needs_git() && workspace_kind != WorkspaceKind::Git {
        return Err(CreateError::BadRequest(format!(
            "output {} needs a git workspace",
            output_kind.as_str()
        )));
    }
    if let Some(k) = spec.idempotency_key.as_deref() {
        validate_idempotency_key(k)?;
    }
    let hash = request_hash(&spec, &title);
    if let Some(id_key) = spec.idempotency_key.as_deref() {
        if let Some(out) = replay_if_ready(shared, id_key, &hash)? {
            return Ok(out);
        }
    }

    let proposed_key = match spec.key.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(k) => k.to_string(),
        None => allocate_create_key(shared, &cfg, &spec, &title).map_err(CreateError::Internal)?,
    };

    let key = if let Some(id_key) = spec.idempotency_key.as_deref() {
        match shared
            .db
            .insert_idempotency(id_key, &hash, &proposed_key)
            .map_err(CreateError::Internal)?
        {
            None => proposed_key,
            Some(row) => {
                if row.request_hash != hash {
                    return Err(conflict_err());
                }
                if let Some(out) = create_out_from_task(shared, &row.task_key, &row.outcome)? {
                    return Ok(out);
                }
                row.task_key
            }
        }
    } else {
        proposed_key
    };

    if let Err(e) = store_attachments(&cfg.state_db, &key, &spec.files) {
        if let Some(id_key) = spec.idempotency_key.as_deref() {
            let _ = shared.db.delete_idempotency(id_key);
        }
        return Err(CreateError::BadRequest(e.to_string()));
    }

    let text = format!("# {title}\n\n{body}");
    let output_spec = spec.webhook.clone().or(spec.patch.clone());
    let outcome = match shared.submit_with(
        &key,
        &text,
        &spec.source,
        repo_path.as_deref(),
        spec.issue,
        spec.shadow,
        spec.tests_ok,
        SubmitOpts {
            workspace_kind: Some(workspace_kind.as_str().into()),
            output_kind: Some(output_kind.as_str().into()),
            output_spec,
            source_kind: Some(spec.source_kind.clone()),
            title: Some(title.clone()),
            allow_empty_repo: allow_empty,
            provider_hint: spec.provider.clone(),
            needs: spec.needs.clone(),
        },
    ) {
        Ok(o) => o,
        Err(e) => {
            remove_attachments(&cfg.state_db, &key);
            if let Some(id_key) = spec.idempotency_key.as_deref() {
                let _ = shared.db.delete_idempotency(id_key);
            }
            return Err(CreateError::Internal(e));
        }
    };
    let status = match outcome {
        SubmitOutcome::Accepted => "accepted",
        SubmitOutcome::Queued => "queued",
    };
    if let Some(id_key) = spec.idempotency_key.as_deref() {
        let _ = shared.db.complete_idempotency(id_key, status);
    }
    Ok(CreateOut {
        key,
        title,
        workspace: workspace_kind.as_str().into(),
        output: output_kind.as_str().into(),
        status,
        replayed: false,
    })
}

fn replay_if_ready(
    shared: &Shared,
    id_key: &str,
    hash: &str,
) -> Result<Option<CreateOut>, CreateError> {
    let Some(row) = shared
        .db
        .get_idempotency(id_key)
        .map_err(CreateError::Internal)?
    else {
        return Ok(None);
    };
    if row.request_hash != hash {
        return Err(conflict_err());
    }
    if row.outcome.is_empty() {
        return Ok(None);
    }
    create_out_from_task(shared, &row.task_key, &row.outcome)
}

fn create_out_from_task(
    shared: &Shared,
    key: &str,
    outcome: &str,
) -> Result<Option<CreateOut>, CreateError> {
    let Some(t) = shared.db.get_task(key).map_err(CreateError::Internal)? else {
        return Ok(None);
    };
    let status = if outcome == "queued" {
        "queued"
    } else {
        "accepted"
    };
    Ok(Some(CreateOut {
        key: t.key,
        title: t.title,
        workspace: t.workspace_kind,
        output: t.output_kind,
        status,
        replayed: true,
    }))
}

fn conflict_err() -> CreateError {
    CreateError::Conflict(
        "idempotency key reused with a different request (same key requires the same body, workspace, output, and files)"
            .into(),
    )
}

fn allocate_create_key(
    shared: &Shared,
    cfg: &crate::config::Config,
    spec: &CreateIn,
    title: &str,
) -> Result<String> {
    let prefix = if spec.repo_named_prefix {
        spec.repo
            .as_deref()
            .and_then(|f| cfg.resolve_repo_filter(f))
            .map(|e| {
                if e.shorthand {
                    cfg.task_key_prefix.clone()
                } else {
                    e.name.clone()
                }
            })
            .unwrap_or_else(|| cfg.task_key_prefix.clone())
    } else {
        cfg.task_key_prefix.clone()
    };
    task::allocate_key(&shared.db, &prefix, title)
}

fn request_hash(spec: &CreateIn, title: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"atr-create-v1");
    put(&mut h, b"body", spec.body.trim().as_bytes());
    put(&mut h, b"title", title.as_bytes());
    put(
        &mut h,
        b"key",
        spec.key.as_deref().unwrap_or("").trim().as_bytes(),
    );
    put(
        &mut h,
        b"repo",
        spec.repo.as_deref().unwrap_or("").trim().as_bytes(),
    );
    put(
        &mut h,
        b"dir",
        spec.dir.as_deref().unwrap_or("").trim().as_bytes(),
    );
    put(
        &mut h,
        b"workspace",
        spec.workspace.as_deref().unwrap_or("").trim().as_bytes(),
    );
    put(
        &mut h,
        b"output",
        spec.output.as_deref().unwrap_or("").trim().as_bytes(),
    );
    put(
        &mut h,
        b"webhook",
        spec.webhook.as_deref().unwrap_or("").trim().as_bytes(),
    );
    put(
        &mut h,
        b"patch",
        spec.patch.as_deref().unwrap_or("").trim().as_bytes(),
    );
    put(
        &mut h,
        b"provider",
        spec.provider.as_deref().unwrap_or("").trim().as_bytes(),
    );
    put(&mut h, b"needs", spec.needs.join(",").as_bytes());
    put(
        &mut h,
        b"issue",
        spec.issue
            .map(|n| n.to_string())
            .unwrap_or_default()
            .as_bytes(),
    );
    put(&mut h, b"shadow", if spec.shadow { b"1" } else { b"0" });
    put(&mut h, b"tests_ok", if spec.tests_ok { b"1" } else { b"0" });
    for f in &spec.files {
        put(&mut h, b"file.name", f.name.as_bytes());
        let mut fh = Sha256::new();
        fh.update(&f.bytes);
        put(
            &mut h,
            b"file.sha256",
            format!("{:x}", fh.finalize()).as_bytes(),
        );
    }
    format!("{:x}", h.finalize())
}

fn put(h: &mut Sha256, label: &[u8], value: &[u8]) {
    h.update(label);
    h.update((value.len() as u64).to_be_bytes());
    h.update(value);
}

fn file_byte_cap(max_prompt_bytes: u64) -> u64 {
    if max_prompt_bytes == 0 {
        ABSOLUTE_FILE_CAP
    } else {
        max_prompt_bytes
    }
}

/// Standard or URL-safe base64, padding optional.
pub fn decode_base64(input: &str) -> Result<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1] == b'=' {
        end -= 1;
    }
    let data = &bytes[..end];
    if data.is_empty() {
        return Ok(Vec::new());
    }
    if data.iter().any(|b| val(*b).is_none()) {
        bail!("invalid base64");
    }
    if data.len() % 4 == 1 {
        bail!("invalid base64");
    }
    let mut out = Vec::with_capacity(data.len() * 3 / 4);
    let mut i = 0;
    while i < data.len() {
        let n = (data.len() - i).min(4);
        let a = val(data[i]).unwrap() as u32;
        let b = if n > 1 { val(data[i + 1]).unwrap() } else { 0 } as u32;
        let c = if n > 2 { val(data[i + 2]).unwrap() } else { 0 } as u32;
        let d = if n > 3 { val(data[i + 3]).unwrap() } else { 0 } as u32;
        let triple = (a << 18) | (b << 12) | (c << 6) | d;
        out.push((triple >> 16) as u8);
        if n > 2 {
            out.push((triple >> 8) as u8);
        }
        if n > 3 {
            out.push(triple as u8);
        }
        i += 4;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Shared;
    use crate::config::load_config;
    use std::sync::Arc;

    fn shared(dir: &std::path::Path) -> Arc<Shared> {
        let cfg_path = dir.join("config.toml");
        let prompt = dir.join("p.md");
        std::fs::write(&prompt, "p\n").unwrap();
        std::fs::write(
            &cfg_path,
            format!(
                r#"
listen = "127.0.0.1:0"
state_db = "{db}"
worktree_root = "{root}"
repo_path = "{root}"
system_prompt_file = "{prompt}"
display_timezone = "UTC"
[[provider]]
name = "claude"
kind = "claude"
cli = "/bin/true"
"#,
                db = dir.join("s.db").display(),
                root = dir.display(),
                prompt = prompt.display(),
            ),
        )
        .unwrap();
        let cfg = load_config(&cfg_path).unwrap();
        Shared::from_config(cfg_path, cfg).unwrap()
    }

    fn spec(body: &str) -> CreateIn {
        CreateIn {
            body: body.into(),
            source: "http".into(),
            source_kind: "http".into(),
            workspace: Some("none".into()),
            output: Some("text".into()),
            ..CreateIn::default()
        }
    }

    #[test]
    fn decode_base64_round_trip_padding_and_url_safe() {
        assert_eq!(decode_base64("SGVsbG8=").unwrap(), b"Hello");
        assert_eq!(decode_base64("SGVsbG8").unwrap(), b"Hello");
        assert_eq!(decode_base64("aGVsbG8h").unwrap(), b"hello!");
        assert_eq!(decode_base64("QQ==").unwrap(), b"A");
        assert!(decode_base64("!!!!").is_err());
    }

    #[test]
    fn sanitize_filename_rejects_paths_and_reserved() {
        assert_eq!(sanitize_filename("notes.md").unwrap(), "notes.md");
        assert!(sanitize_filename("../x").is_err());
        assert!(sanitize_filename("a/b").is_err());
        assert!(sanitize_filename("a\\b").is_err());
        assert!(sanitize_filename(".git").is_err());
        assert!(sanitize_filename(".atr").is_err());
        assert!(sanitize_filename("").is_err());
    }

    #[test]
    fn parse_files_text_and_base64() {
        let files = parse_files(
            vec![
                FileIn {
                    name: Some("a.txt".into()),
                    text: Some("hi".into()),
                    ..FileIn::default()
                },
                FileIn {
                    name: Some("b.bin".into()),
                    content_base64: Some("QQ==".into()),
                    ..FileIn::default()
                },
            ],
            1024,
        )
        .unwrap();
        assert_eq!(files[0].bytes, b"hi");
        assert_eq!(files[1].bytes, b"A");
        assert!(parse_files(
            vec![FileIn {
                name: Some("x.txt".into()),
                text: Some("a".into()),
                content_base64: Some("YQ==".into()),
            }],
            1024
        )
        .is_err());
        assert!(parse_files(
            vec![
                FileIn {
                    name: Some("dup.txt".into()),
                    text: Some("a".into()),
                    ..FileIn::default()
                },
                FileIn {
                    name: Some("dup.txt".into()),
                    text: Some("b".into()),
                    ..FileIn::default()
                },
            ],
            1024
        )
        .is_err());
    }

    #[test]
    fn materialize_writes_basename_into_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        let wt = dir.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        store_attachments(
            &db,
            "t1",
            &[AttachFile {
                name: "brief.md".into(),
                bytes: b"hello attach".to_vec(),
            }],
        )
        .unwrap();
        materialize_attachments(&db, "t1", &wt).unwrap();
        assert_eq!(
            std::fs::read_to_string(wt.join("brief.md")).unwrap(),
            "hello attach"
        );
        assert_eq!(list_attachment_names(&db, "t1"), vec!["brief.md"]);
    }

    #[test]
    fn create_replays_same_idempotency_key_and_conflicts_on_new_body() {
        let dir = tempfile::tempdir().unwrap();
        let s = shared(dir.path());
        let mut first = spec("what is 2+2?");
        first.idempotency_key = Some("k-1".into());
        let a = create(&s, first.clone()).unwrap();
        assert!(!a.replayed);
        let b = create(&s, first).unwrap();
        assert!(b.replayed);
        assert_eq!(a.key, b.key);
        assert_eq!(s.db.list_tasks().unwrap().len(), 1);
        assert_eq!(s.db.all_messages(&a.key).unwrap().len(), 1);

        let mut other = spec("a different prompt");
        other.idempotency_key = Some("k-1".into());
        match create(&s, other) {
            Err(CreateError::Conflict(msg)) => {
                assert!(msg.contains("different request"), "{msg}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(s.db.list_tasks().unwrap().len(), 1);
    }

    #[test]
    fn create_stores_files_then_materialize_matches_name() {
        let dir = tempfile::tempdir().unwrap();
        let s = shared(dir.path());
        let mut req = spec("use the attached file");
        req.files = vec![AttachFile {
            name: "notes.txt".into(),
            bytes: b"pay attention".to_vec(),
        }];
        let out = create(&s, req).unwrap();
        let cfg = s.config.read().unwrap().clone();
        let wt = crate::worktree::ensure_none_workspace(&cfg.worktree_root, &out.key).unwrap();
        materialize_attachments(&cfg.state_db, &out.key, &wt).unwrap();
        assert_eq!(
            std::fs::read_to_string(wt.join("notes.txt")).unwrap(),
            "pay attention"
        );
    }
}
