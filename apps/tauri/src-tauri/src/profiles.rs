//! Multi-server Profile metadata store & OS Keychain token bridge for OpenMirasim Desktop.
//!
//! Profile URLs and display names are stored in `profiles.json` under the user's private OS
//! config directory (`0700`/`0600` on Unix, never falling back to shared `/tmp`), while bearer
//! tokens (`omt_...`) are stored exclusively inside the native OS Keychain (`keyring::Entry`),
//! never written in plaintext to disk.

use keyring::Entry;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use url::Url;

const KEYCHAIN_SERVICE: &str = "dev.openmirasim.workbench";
const MAX_PROFILES: usize = 16;
const MAX_PROFILE_NAME_CHARS: usize = 80;

static PROFILES_LOCK: Mutex<()> = Mutex::new(());
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerProfile {
    pub id: String,
    pub name: String,
    pub server_url: String,
    pub last_used_at: i64,
}

/// Returns the user's private config path for `profiles.json`.
/// Fails closed if the OS config directory cannot be resolved — never falls back to `/tmp`.
pub fn default_profiles_path() -> Result<PathBuf, String> {
    let base = dirs::config_dir()
        .ok_or_else(|| "cannot resolve user OS config directory for server profiles".to_string())?;
    Ok(base.join("openmirasim-desktop").join("profiles.json"))
}

fn validate_profile_id(id: &str) -> Result<&str, String> {
    let trimmed = id.trim();
    if trimmed.is_empty()
        || trimmed.len() > 80
        || !trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("invalid profile id".into());
    }
    Ok(trimmed)
}

fn validate_server_url(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("server_url cannot be empty".into());
    }
    let parsed = Url::parse(trimmed).map_err(|e| format!("invalid server_url: {e}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!(
                "unsupported server_url scheme `{other}` (expected http or https)"
            ));
        }
    }
    if parsed.host_str().is_none() {
        return Err("server_url must include a host".into());
    }
    Ok(trimmed.to_string())
}

fn ensure_not_symlink(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(format!(
            "refusing to follow symlink at {}",
            path.display()
        )),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("stat {} failed: {e}", path.display())),
    }
}

fn load_profiles_unlocked(path: &Path) -> Vec<ServerProfile> {
    if ensure_not_symlink(path).is_err() {
        return Vec::new();
    }
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Vec<ServerProfile>>(&bytes).ok())
        .unwrap_or_default()
}

pub fn load_profiles(path: &Path) -> Vec<ServerProfile> {
    let _guard = PROFILES_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    load_profiles_unlocked(path)
}

fn write_profiles_unlocked(path: &Path, profiles: &[ServerProfile]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        ensure_not_symlink(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)
                .map_err(|e| format!("create config dir failed: {e}"))?;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
        #[cfg(not(unix))]
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create config dir failed: {e}"))?;
        }
    }
    ensure_not_symlink(path)?;

    let bytes = serde_json::to_vec_pretty(profiles)
        .map_err(|e| format!("serialize profiles failed: {e}"))?;
    let seq = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_name = format!("profiles.{}.{seq}.tmp", std::process::id());
    let tmp = path
        .parent()
        .map(|p| p.join(&tmp_name))
        .unwrap_or_else(|| PathBuf::from(&tmp_name));

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(&tmp)
        .map_err(|e| format!("create profiles temp failed: {e}"))?;
    if let Err(e) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("write profiles temp failed: {e}"));
    }
    drop(file);

    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("commit profiles failed: {e}"));
    }
    Ok(())
}

/// Inserts or updates a server profile. Returns `(updated_profiles, evicted_profile_ids)` so
/// callers can purge orphaned Keychain entries for deduplicated or truncated profiles.
pub fn upsert_profile(
    path: &Path,
    mut profile: ServerProfile,
) -> Result<(Vec<ServerProfile>, Vec<String>), String> {
    let trimmed_id = validate_profile_id(&profile.id)?.to_string();
    profile.id = trimmed_id;
    profile.server_url = validate_server_url(&profile.server_url)?;
    profile.name = profile
        .name
        .trim()
        .chars()
        .take(MAX_PROFILE_NAME_CHARS)
        .collect();
    if profile.name.is_empty() {
        profile.name = profile.server_url.clone();
    }

    let _guard = PROFILES_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut list = load_profiles_unlocked(path);
    let mut evicted = Vec::new();
    list.retain(|p| {
        let keep = p.id != profile.id && p.server_url != profile.server_url;
        if !keep && p.id != profile.id {
            evicted.push(p.id.clone());
        }
        keep
    });
    list.insert(0, profile);
    if list.len() > MAX_PROFILES {
        for dropped in list.drain(MAX_PROFILES..) {
            evicted.push(dropped.id);
        }
    }
    write_profiles_unlocked(path, &list)?;
    Ok((list, evicted))
}

pub fn remove_profile(path: &Path, profile_id: &str) -> Result<Vec<ServerProfile>, String> {
    let id = validate_profile_id(profile_id)?;
    let _guard = PROFILES_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut list = load_profiles_unlocked(path);
    list.retain(|p| p.id != id);
    write_profiles_unlocked(path, &list)?;
    Ok(list)
}

pub fn save_token_in_keychain(profile_id: &str, token: &str) -> Result<(), String> {
    let id = validate_profile_id(profile_id)?;
    let entry = Entry::new(KEYCHAIN_SERVICE, id)
        .map_err(|e| format!("open OS keychain entry failed: {e}"))?;
    entry
        .set_password(token.trim())
        .map_err(|e| format!("save token in OS keychain failed: {e}"))
}

pub fn load_token_from_keychain(profile_id: &str) -> Result<Option<String>, String> {
    let id = validate_profile_id(profile_id)?;
    let entry = Entry::new(KEYCHAIN_SERVICE, id)
        .map_err(|e| format!("open OS keychain entry failed: {e}"))?;
    match entry.get_password() {
        Ok(pw) => Ok(Some(pw)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(format!("read token from OS keychain failed: {e}")),
    }
}

pub fn delete_token_from_keychain(profile_id: &str) -> Result<(), String> {
    let id = validate_profile_id(profile_id)?;
    let entry = Entry::new(KEYCHAIN_SERVICE, id)
        .map_err(|e| format!("open OS keychain entry failed: {e}"))?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!("delete token from OS keychain failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_store_upserts_deduplicates_evicts_and_removes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("profiles.json");
        let (list, evicted) = upsert_profile(
            &path,
            ServerProfile {
                id: "  prod_1  ".into(),
                name: "Prod Linux".into(),
                server_url: "https://om.internal:4939/".into(),
                last_used_at: 100,
            },
        )
        .unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "prod_1");
        assert_eq!(list[0].server_url, "https://om.internal:4939");
        assert!(evicted.is_empty());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "profiles.json must be mode 0600");
        }

        // Replacing same URL with a new ID evicts the old ID for keychain cleanup.
        let (list, evicted) = upsert_profile(
            &path,
            ServerProfile {
                id: "prod_2".into(),
                name: "Prod Renamed".into(),
                server_url: "https://om.internal:4939".into(),
                last_used_at: 200,
            },
        )
        .unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "prod_2");
        assert_eq!(evicted, vec!["prod_1".to_string()]);

        let list = remove_profile(&path, " prod_2 ").unwrap();
        assert!(list.is_empty());

        for bad_url in ["javascript:alert(1)", "file:///etc/passwd", "not-a-url"] {
            assert!(
                upsert_profile(
                    &path,
                    ServerProfile {
                        id: "bad_url".into(),
                        name: "bad".into(),
                        server_url: bad_url.into(),
                        last_used_at: 0,
                    },
                )
                .is_err(),
                "expected {bad_url} to be rejected"
            );
        }

        assert!(
            upsert_profile(
                &path,
                ServerProfile {
                    id: "../bad".into(),
                    name: "bad".into(),
                    server_url: "http://127.0.0.1:4939".into(),
                    last_used_at: 0,
                },
            )
            .is_err()
        );
    }
}
