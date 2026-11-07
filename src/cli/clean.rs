//! Delete only regenerable state owned by wx; preserve configuration and database keys.
use anyhow::{Context, Result};
use serde_json::json;
use std::path::Path;

fn bytes(path: &Path) -> Result<u64> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        std::fs::read_dir(path)?.try_fold(0, |sum, entry| Ok(sum + bytes(&entry?.path())?))
    } else {
        Ok(metadata.len())
    }
}

pub fn cmd_clean(apply: bool) -> Result<()> {
    let root = crate::config::cli_dir();
    let names = [
        "cache",
        "daemon.log",
        "daemon.log.1",
        "shard-meta.json",
        "shard-meta.json.tmp",
    ];
    if apply {
        super::transport::stop_daemon()?;
    }
    let mut entries = Vec::new();
    for name in names {
        let path = root.join(name);
        if let Ok(metadata) = std::fs::symlink_metadata(&path) {
            let size = bytes(&path)?;
            if apply {
                if metadata.is_dir() {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                }
                .with_context(|| format!("清理失败: {}", path.display()))?;
            }
            entries.push(json!({"path":path,"bytes":size}));
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"applied":apply,"files":entries,"preserved":["config.json","all_keys.json"],"exports":"Explicit export files remain at the paths you selected"})
        )?
    );
    Ok(())
}
