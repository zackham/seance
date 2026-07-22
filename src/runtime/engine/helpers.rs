//! Pure helpers for the session engine (status vocab, pad I/O, task JSON).
//! gpui-free; unit-tested here.

use std::path::PathBuf;

use serde_json::json;

use crate::events;
use crate::runtime::protocol::TaskRecord;

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(crate) fn task_json(t: &TaskRecord) -> serde_json::Value {
    json!({
        "id": t.id,
        "pane": t.pane,
        "status": t.status,
        "inject_pad_rev": t.inject_pad_rev,
        "inject_pad_bytes": t.inject_pad_bytes,
        "created_ms": t.created_ms,
        "finished_ms": t.finished_ms,
        "body": t.body,
        "body_chars": t.body.len(),
    })
}

/// Write discoverable task id next to the scratchpad (worker re-orientation).
pub(crate) fn write_task_sidecar(scratch_path: &std::path::Path, rec: &TaskRecord) {
    let id_path = scratch_path.with_extension("taskid");
    let json_path = scratch_path.with_extension("task.json");
    let _ = std::fs::write(&id_path, format!("{}\n", rec.id));
    let summary = json!({
        "id": rec.id,
        "pane": rec.pane,
        "status": rec.status,
        "created_ms": rec.created_ms,
        "body": rec.body,
        "body_chars": rec.body.len(),
        "hint": "seance ctl task   # or: cat $SEANCE_SCRATCHPAD with .taskid extension",
    });
    if let Ok(s) = serde_json::to_string_pretty(&summary) {
        let _ = std::fs::write(&json_path, s);
    }
}

pub(crate) const VALID_STATUSES: &[&str] = &[
    "planning",
    "working",
    "blocked",
    "needs-human",
    "done",
    "idle",
];

pub(crate) fn validate_status(state: &str) -> Result<(), String> {
    if VALID_STATUSES.contains(&state) {
        Ok(())
    } else {
        Err(format!(
            "invalid status '{state}' — use one of: {}",
            VALID_STATUSES.join("|")
        ))
    }
}

/// Agents may only mutate their own pane's status/pad unless `from` is unset
/// (external cli orchestrator) or target matches session.
pub(crate) fn assert_self_or_cross(
    target_slug: &str,
    from: &Option<String>,
    actor: &str,
) -> Result<(), String> {
    // External orchestrator (no $SEANCE_SESSION) → cli may cross-pane.
    if from.is_none() || actor == "cli" || actor == "agent:cli" {
        return Ok(());
    }
    let principal = from.as_deref().unwrap_or("");
    let principal = principal.strip_prefix("agent:").unwrap_or(principal);
    if principal == target_slug {
        return Ok(());
    }
    Err(format!(
        "self-only: agent '{principal}' cannot status/note/finish pane '{target_slug}' \
         (orchestrators outside a pane may; or set $SEANCE_SESSION to self)"
    ))
}

/// Atomic replace via temp+rename in the same directory.
pub(crate) fn atomic_write_pad(path: &std::path::Path, contents: &str) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let tmp = parent.join(format!(
        ".{}.tmp.{}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("pad"),
        std::process::id()
    ));
    std::fs::write(&tmp, contents).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

/// Atomic append: read existing + write new via temp+rename.
pub(crate) fn atomic_append_pad(path: &std::path::Path, chunk: &str) -> Result<(), String> {
    let mut body = if path.exists() {
        std::fs::read_to_string(path).map_err(|e| e.to_string())?
    } else {
        String::new()
    };
    body.push_str(chunk);
    atomic_write_pad(path, &body)
}

/// Cheap local stamp without chrono dep (HH:MM:SS).
pub(crate) fn chrono_lite_stamp() -> String {
    events::fmt_time(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    )
}

/// Wrapper so from_handoff can take owned fds with index.
pub struct OwnedFdAdopt {
    pub fd: std::os::fd::OwnedFd,
}

pub(crate) fn shell_rc_path() -> PathBuf {
    PathBuf::from(shellexpand::tilde("~/.local/share/seance/seance.bash").into_owned())
}

pub(crate) fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(input.trim())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn validate_status_accepts_vocab() {
        for s in [
            "planning",
            "working",
            "blocked",
            "needs-human",
            "done",
            "idle",
        ] {
            assert!(validate_status(s).is_ok(), "{s}");
        }
        assert!(validate_status("DONE").is_err());
        assert!(validate_status("needs_human").is_err());
        assert!(validate_status("").is_err());
        let err = validate_status("shipped").unwrap_err();
        assert!(err.contains("planning|working"));
    }

    #[test]
    fn assert_self_or_cross_external_and_cli() {
        assert!(assert_self_or_cross("w1", &None, "cli").is_ok());
        assert!(assert_self_or_cross("w1", &Some("other".into()), "cli").is_ok());
        assert!(assert_self_or_cross("w1", &Some("other".into()), "agent:cli").is_ok());
    }

    #[test]
    fn assert_self_or_cross_agent_self_only() {
        assert!(assert_self_or_cross("w1", &Some("w1".into()), "agent:w1").is_ok());
        assert!(assert_self_or_cross("w1", &Some("agent:w1".into()), "agent:w1").is_ok());
        let err = assert_self_or_cross("w1", &Some("w2".into()), "agent:w2").unwrap_err();
        assert!(err.contains("self-only"));
        assert!(err.contains("w1"));
    }

    #[test]
    fn atomic_write_and_append_pad_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "seance-pad-test-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("note.md");
        atomic_write_pad(&path, "hello\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\n");
        atomic_append_pad(&path, "world\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\nworld\n");
        // overwrite
        atomic_write_pad(&path, "fresh\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_append_creates_missing_file() {
        let dir = std::env::temp_dir().join(format!(
            "seance-pad-append-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("new.md");
        atomic_append_pad(&path, "first\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn task_json_shape() {
        let t = TaskRecord {
            id: "t1".into(),
            pane: "w".into(),
            inject_pad_rev: 3,
            inject_pad_bytes: 12,
            body: "do the thing".into(),
            status: "open".into(),
            created_ms: 100,
            finished_ms: None,
        };
        let v = task_json(&t);
        assert_eq!(v["id"], "t1");
        assert_eq!(v["pane"], "w");
        assert_eq!(v["status"], "open");
        assert_eq!(v["inject_pad_rev"], 3);
        assert_eq!(v["body"], "do the thing");
        assert_eq!(v["body_chars"], 12);
        assert!(v.get("finished_ms").is_none() || v["finished_ms"].is_null());
    }

    #[test]
    fn write_task_sidecar_files() {
        let dir = std::env::temp_dir().join(format!(
            "seance-sidecar-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let scratch = dir.join("worker.md");
        std::fs::write(&scratch, "# pad\n").unwrap();
        let rec = TaskRecord {
            id: "task_9".into(),
            pane: "worker".into(),
            inject_pad_rev: 1,
            inject_pad_bytes: 0,
            body: "payload".into(),
            status: "open".into(),
            created_ms: 1,
            finished_ms: None,
        };
        write_task_sidecar(&scratch, &rec);
        assert_eq!(
            std::fs::read_to_string(scratch.with_extension("taskid"))
                .unwrap()
                .trim(),
            "task_9"
        );
        let j: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(scratch.with_extension("task.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(j["id"], "task_9");
        assert_eq!(j["body"], "payload");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn base64_decode_trims_and_roundtrips() {
        assert_eq!(base64_decode("Aw==").unwrap(), vec![0x03]);
        assert_eq!(base64_decode(" DQ== \n").unwrap(), vec![0x0d]);
        assert!(base64_decode("!!!").is_err());
    }

    #[test]
    fn chrono_lite_stamp_hh_mm_ss() {
        let s = chrono_lite_stamp();
        // events::fmt_time → typically "HH:MM:SS" or similar short stamp
        assert!(!s.is_empty());
        assert!(s.len() <= 16, "stamp too long: {s}");
    }

    #[test]
    fn shell_rc_path_is_under_home_share() {
        let p: PathBuf = shell_rc_path();
        let s = p.to_string_lossy();
        assert!(s.ends_with("seance.bash"));
        assert!(s.contains("seance"));
    }
}
