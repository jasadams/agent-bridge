use serde_json::Value;
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::OnceLock,
};

/// Explicit native observations, independent of the broker's registration DB.
pub fn write_cli(root: &Path, rows: &Value) -> PathBuf {
    static TEMPLATE: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    let (_, template) = TEMPLATE.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let template = dir.path().join("claude-template");
        std::fs::write(&template,"#!/bin/sh\n[ \"$#\" -eq 2 ] && [ \"$1\" = agents ] && [ \"$2\" = --json ] || exit 64\nexec cat -- \"$0.sessions.json\"\n").unwrap();
        std::fs::set_permissions(&template,std::fs::Permissions::from_mode(0o700)).unwrap();
        (dir,template)
    });
    let binary = root.join("claude");
    // Avoid writable executable fds inherited by a parallel test's fork (ETXTBSY).
    let next = root.join(format!(".claude-link-{}", uuid::Uuid::new_v4()));
    std::os::unix::fs::symlink(template, &next).unwrap();
    std::fs::rename(next, &binary).unwrap();
    set_rows(root, rows);
    binary
}

pub fn set_rows(root: &Path, rows: &Value) {
    let next = root.join(format!(".claude-rows-{}", uuid::Uuid::new_v4()));
    std::fs::write(&next, serde_json::to_vec(rows).unwrap()).unwrap();
    std::fs::rename(next, root.join("claude.sessions.json")).unwrap();
}
