//! Record/Replay — recording ID validation, path helpers, ReplayProvider.
//! See docs/design/RECORD_REPLAY.md.

use crate::error::ProviderResult;
use std::path::PathBuf;
use regex::Regex;
use std::sync::OnceLock;

static ID_REGEX: OnceLock<Regex> = OnceLock::new();

fn id_regex() -> &'static Regex {
    ID_REGEX.get_or_init(|| Regex::new(r"^[a-zA-Z0-9._-]{1,80}$").unwrap())
}

/// Validate a recording ID. Only [a-zA-Z0-9._-], 1-80 chars.
/// Prevents path traversal (../, /, url-encoded chars, spaces).
pub fn validate_recording_id(id: &str) -> ProviderResult<()> {
    if !id_regex().is_match(id) {
        return Err(crate::ProviderError::Stream(format!(
            "invalid recording id '{}': only [a-zA-Z0-9._-] allowed, 1-80 chars", id
        )));
    }
    // Dots are allowed by the regex (for version suffixes like "v1.2"), but a
    // bare "." or ".." is a path-special component that escapes the recordings
    // dir. Since the regex already forbids "/", the id is a single component;
    // require it to be Normal. This also rejects ".".
    if std::path::Path::new(id)
        .components()
        .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err(crate::ProviderError::Stream(format!(
            "invalid recording id '{}': only [a-zA-Z0-9._-] allowed, 1-80 chars", id
        )));
    }
    Ok(())
}

/// Base directory for all recordings: ~/.ion/recordings
pub fn recordings_dir() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".ion").join("recordings")
}

/// Full path to a recording's trace file, after validating ID.
pub fn recording_trace_path(id: &str) -> ProviderResult<PathBuf> {
    validate_recording_id(id)?;
    let base = recordings_dir();
    let path = base.join(id).join("trace.jsonl");
    // Lexical check: path must stay under base (use components, not canonicalize,
    // since the file may not exist yet).
    let mut depth = 0i32;
    for comp in path.components() {
        match comp {
            std::path::Component::ParentDir => depth -= 1,
            std::path::Component::Normal(_) => depth += 1,
            _ => {}
        }
        if depth < 0 {
            return Err(crate::ProviderError::Stream(format!(
                "recording id escapes recordings dir: {}", id
            )));
        }
    }
    Ok(path)
}

/// Full path to a recording's meta file.
pub fn recording_meta_path(id: &str) -> ProviderResult<PathBuf> {
    validate_recording_id(id)?;
    Ok(recordings_dir().join(id).join("meta.json"))
}

use crate::faux::FauxProvider;
use crate::event_stream::EventStream;
use crate::registry::ApiProvider;
use crate::types::{Context, Model, StreamOptions};
use async_trait::async_trait;

/// Replay provider: loads a recording by model.id, delegates to FauxProvider.
/// Register under "replay" key. Use via `--model replay/<recording-id>`.
pub struct ReplayProvider;

#[async_trait]
impl ApiProvider for ReplayProvider {
    async fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
        _cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> ProviderResult<EventStream> {
        let recording_id = &model.id;
        let trace_path = recording_trace_path(recording_id)?;

        if !trace_path.exists() {
            return Err(crate::ProviderError::Stream(format!(
                "recording '{}' not found at {}", recording_id, trace_path.display()
            )));
        }

        // Loud warning: tools will execute for real during replay
        eprintln!("[replay] ⚠️  Tools will execute for real. Replaying decisions from '{}'.", recording_id);
        eprintln!("[replay] ⚠️  Ensure you are in an isolated workspace.");

        let steps = crate::faux::load_script(&trace_path)?;
        let faux = FauxProvider::new();
        faux.set_responses(steps);
        faux.stream(model, context, options, _cancel).await
    }
}

use std::path::Path;
use std::io::Write;

/// RAII guard for a recording lock file. Releases (deletes) on drop.
pub struct RecordingLock(PathBuf);
impl Drop for RecordingLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Acquire an exclusive lock for a recording directory.
/// Returns Err if already locked and `overwrite` is false.
/// If `overwrite` is true, clears existing lock + trace + meta before acquiring.
pub fn acquire_recording_lock(rec_dir: &Path, overwrite: bool) -> ProviderResult<Option<RecordingLock>> {
    let lock_path = rec_dir.join(".lock");
    if lock_path.exists() && !overwrite {
        return Err(crate::ProviderError::Stream(
            "recording already exists or is active. Set ION_RECORD_OVERWRITE=1 to overwrite.".into()
        ));
    }
    // overwrite=true clears any prior lock + trace + meta so we start fresh.
    if overwrite {
        let _ = std::fs::remove_file(&lock_path);
        let trace = rec_dir.join("trace.jsonl");
        if trace.exists() { let _ = std::fs::remove_file(&trace); }
        let meta = rec_dir.join("meta.json");
        if meta.exists() { let _ = std::fs::remove_file(&meta); }
    }
    std::fs::create_dir_all(rec_dir)
        .map_err(|e| crate::ProviderError::Stream(format!("failed to create recording dir: {}", e)))?;
    let _ = std::fs::set_permissions(rec_dir, std::os::unix::fs::PermissionsExt::from_mode(0o700));
    let mut f = std::fs::OpenOptions::new().create_new(true).write(true).open(&lock_path)
        .map_err(|e| crate::ProviderError::Stream(format!("failed to acquire lock: {}", e)))?;
    let _ = writeln!(f, "{}", std::process::id());
    Ok(Some(RecordingLock(lock_path)))
}
