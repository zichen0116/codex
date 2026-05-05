use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use codex_config::ConfigLayerStack;
use codex_plugin::validate_plugin_segment;
use codex_utils_absolute_path::AbsolutePathBuf;
use tracing::warn;

use crate::marketplace::find_marketplace_manifest_path;

pub const INSTALLED_MARKETPLACES_DIR: &str = ".tmp/marketplaces";
pub const MARKETPLACE_STALE_TEMP_DIR_MAX_AGE: Duration = Duration::from_secs(10 * 60);

pub fn marketplace_install_root(codex_home: &Path) -> PathBuf {
    codex_home.join(INSTALLED_MARKETPLACES_DIR)
}

pub fn remove_stale_marketplace_temp_dirs(install_root: &Path, max_age: Duration) {
    for (parent, prefixes) in [
        (
            install_root.join(".staging"),
            ["marketplace-upgrade-", "marketplace-add-"].as_slice(),
        ),
        (
            install_root.to_path_buf(),
            ["marketplace-backup-"].as_slice(),
        ),
    ] {
        let entries = match std::fs::read_dir(&parent) {
            Ok(entries) => entries,
            Err(err) => {
                warn!(
                    error = %err,
                    parent = %parent.display(),
                    "failed to list marketplace temp directory parent for stale cleanup"
                );
                continue;
            }
        };

        for entry in entries.flatten() {
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(err) => {
                    warn!(
                        error = %err,
                        path = %entry.path().display(),
                        "failed to inspect marketplace temp directory entry"
                    );
                    continue;
                }
            };
            if !file_type.is_dir() {
                continue;
            }

            let path = entry.path();
            let matches_prefix = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| prefixes.iter().any(|prefix| name.starts_with(prefix)));
            if !matches_prefix {
                continue;
            }

            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(err) => {
                    warn!(
                        error = %err,
                        path = %path.display(),
                        "failed to read marketplace temp directory metadata"
                    );
                    continue;
                }
            };
            let modified = match metadata.modified() {
                Ok(modified) => modified,
                Err(err) => {
                    warn!(
                        error = %err,
                        path = %path.display(),
                        "failed to read marketplace temp directory modification time"
                    );
                    continue;
                }
            };
            let age = match modified.elapsed() {
                Ok(age) => age,
                Err(err) => {
                    warn!(
                        error = %err,
                        path = %path.display(),
                        "failed to compute marketplace temp directory age"
                    );
                    continue;
                }
            };
            if age < max_age {
                continue;
            }

            if let Err(err) = std::fs::remove_dir_all(&path) {
                warn!(
                    error = %err,
                    path = %path.display(),
                    "failed to remove stale marketplace temp directory"
                );
            }
        }
    }
}

pub fn installed_marketplace_roots_from_layer_stack(
    config_layer_stack: &ConfigLayerStack,
    codex_home: &Path,
) -> Vec<AbsolutePathBuf> {
    let Some(user_layer) = config_layer_stack.get_user_layer() else {
        return Vec::new();
    };
    let Some(marketplaces_value) = user_layer.config.get("marketplaces") else {
        return Vec::new();
    };
    let Some(marketplaces) = marketplaces_value.as_table() else {
        warn!("invalid marketplaces config: expected table");
        return Vec::new();
    };
    let default_install_root = marketplace_install_root(codex_home);
    let mut roots = marketplaces
        .iter()
        .filter_map(|(marketplace_name, marketplace)| {
            if !marketplace.is_table() {
                warn!(
                    marketplace_name,
                    "ignoring invalid configured marketplace entry"
                );
                return None;
            }
            if let Err(err) = validate_plugin_segment(marketplace_name, "marketplace name") {
                warn!(
                    marketplace_name,
                    error = %err,
                    "ignoring invalid configured marketplace name"
                );
                return None;
            }
            let path = resolve_configured_marketplace_root(
                marketplace_name,
                marketplace,
                &default_install_root,
            )?;
            find_marketplace_manifest_path(&path).map(|_| path)
        })
        .filter_map(|path| AbsolutePathBuf::try_from(path).ok())
        .collect::<Vec<_>>();
    roots.sort_unstable_by(|left, right| left.as_path().cmp(right.as_path()));
    roots
}

pub fn resolve_configured_marketplace_root(
    marketplace_name: &str,
    marketplace: &toml::Value,
    default_install_root: &Path,
) -> Option<PathBuf> {
    match marketplace.get("source_type").and_then(toml::Value::as_str) {
        Some("local") => marketplace
            .get("source")
            .and_then(toml::Value::as_str)
            .filter(|source| !source.is_empty())
            .map(PathBuf::from),
        _ => Some(default_install_root.join(marketplace_name)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(not(windows))]
    use std::fs::File;
    #[cfg(not(windows))]
    use std::fs::FileTimes;
    use std::process::Command;
    use std::time::Duration;
    use std::time::SystemTime;
    use tempfile::tempdir;

    #[cfg(windows)]
    fn set_dir_mtime(path: &Path, age: Duration) -> Result<(), Box<dyn std::error::Error>> {
        let modified_at = SystemTime::now()
            .checked_sub(age)
            .ok_or("failed to compute stale directory time")?;
        let modified_at = chrono::DateTime::<chrono::Utc>::from(modified_at).to_rfc3339();
        let escaped_path = path.to_string_lossy().replace('\'', "''");
        let command = format!(
            "(Get-Item -LiteralPath '{escaped_path}').LastWriteTimeUtc = [DateTime]::Parse('{modified_at}')"
        );
        let output = Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", &command])
            .output()?;
        if !output.status.success() {
            return Err(format!(
                "failed to set directory time: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )
            .into());
        }
        Ok(())
    }

    #[cfg(not(windows))]
    fn set_dir_mtime(path: &Path, age: Duration) -> Result<(), Box<dyn std::error::Error>> {
        let modified_at = SystemTime::now()
            .checked_sub(age)
            .ok_or("failed to compute stale directory time")?;
        let times = FileTimes::new().set_modified(modified_at);
        File::options().read(true).open(path)?.set_times(times)?;
        Ok(())
    }

    #[test]
    fn remove_stale_marketplace_temp_dirs_removes_only_matching_old_directories() {
        let tmp = tempdir().expect("tempdir");
        let install_root = marketplace_install_root(tmp.path());
        let staging_root = install_root.join(".staging");
        let stale_upgrade_dir = staging_root.join("marketplace-upgrade-stale");
        let fresh_upgrade_dir = staging_root.join("marketplace-upgrade-fresh");
        let stale_add_dir = staging_root.join("marketplace-add-stale");
        let stale_backup_dir = install_root.join("marketplace-backup-stale");
        let unrelated_staging_dir = staging_root.join("not-a-marketplace-temp-dir");
        let unrelated_install_root_dir = install_root.join("debug");

        std::fs::create_dir_all(&stale_upgrade_dir).expect("create stale upgrade dir");
        std::fs::create_dir_all(&fresh_upgrade_dir).expect("create fresh upgrade dir");
        std::fs::create_dir_all(&stale_add_dir).expect("create stale add dir");
        std::fs::create_dir_all(&stale_backup_dir).expect("create stale backup dir");
        std::fs::create_dir_all(&unrelated_staging_dir).expect("create unrelated staging dir");
        std::fs::create_dir_all(&unrelated_install_root_dir)
            .expect("create unrelated install-root dir");

        let stale_age = MARKETPLACE_STALE_TEMP_DIR_MAX_AGE + Duration::from_secs(60);
        set_dir_mtime(&stale_upgrade_dir, stale_age).expect("age stale upgrade dir");
        set_dir_mtime(&fresh_upgrade_dir, Duration::ZERO).expect("age fresh upgrade dir");
        set_dir_mtime(&stale_add_dir, stale_age).expect("age stale add dir");
        set_dir_mtime(&stale_backup_dir, stale_age).expect("age stale backup dir");
        set_dir_mtime(&unrelated_staging_dir, stale_age).expect("age unrelated staging dir");
        set_dir_mtime(&unrelated_install_root_dir, stale_age)
            .expect("age unrelated install-root dir");

        remove_stale_marketplace_temp_dirs(&install_root, MARKETPLACE_STALE_TEMP_DIR_MAX_AGE);

        assert!(!stale_upgrade_dir.exists());
        assert!(fresh_upgrade_dir.is_dir());
        assert!(!stale_add_dir.exists());
        assert!(!stale_backup_dir.exists());
        assert!(unrelated_staging_dir.is_dir());
        assert!(unrelated_install_root_dir.is_dir());
    }
}
