//! Multi-server Profile metadata store & OS Keychain token bridge for OpenMirasim Desktop.
//!
//! Profile URLs and display names are stored in `profiles.json` under the user's OS config
//! directory, while bearer tokens (`omt_...`) are stored exclusively inside the native OS
//! Keychain (`keyring::Entry`), never written in plaintext to disk.

use keyring::Entry;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const KEYCHAIN_SERVICE: &str = "dev.openmirasim.workbench";
const MAX_PROFILES: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerProfile {
    pub id: String,
    pub name: String,
    pub server_url: String,
    pub last_used_at: i64,
}

pub fn default_profiles_path() -> PathBuf {
    let base = dirs::config_dir().unwrap_or_else(std::env::temp_dir);
    base.join("openmirasim-desktop").join("profiles.json")
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

pub fn load_profiles(path: &Path) -> Vec<ServerProfile> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Vec<ServerProfile>>(&bytes).ok())
        .unwrap_or_default()
}

fn write_profiles(path: &Path, profiles: &[ServerProfile]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create config dir failed: {e}"))?;
    }
    let bytes = serde_json::to_vec_pretty(profiles)
        .map_err(|e| format!("serialize profiles failed: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes).map_err(|e| format!("write profiles temp failed: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("commit profiles failed: {e}"))?;
    Ok(())
}

pub fn upsert_profile(
    path: &Path,
    mut profile: ServerProfile,
) -> Result<Vec<ServerProfile>, String> {
    validate_profile_id(&profile.id)?;
    profile.server_url = profile.server_url.trim().trim_end_matches('/').to_string();
    if profile.server_url.is_empty() {
        return Err("server_url cannot be empty".into());
    }
    let mut list = load_profiles(path);
    list.retain(|p| p.id != profile.id && p.server_url != profile.server_url);
    list.insert(0, profile);
    list.truncate(MAX_PROFILES);
    write_profiles(path, &list)?;
    Ok(list)
}

pub fn remove_profile(path: &Path, profile_id: &str) -> Result<Vec<ServerProfile>, String> {
    validate_profile_id(profile_id)?;
    let mut list = load_profiles(path);
    list.retain(|p| p.id != profile_id);
    write_profiles(path, &list)?;
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
    fn profile_store_upserts_deduplicates_and_removes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.json");
        let list = upsert_profile(
            &path,
            ServerProfile {
                id: "prod_1".into(),
                name: "Prod Linux".into(),
                server_url: "https://om.internal:4939/".into(),
                last_used_at: 100,
            },
        )
        .unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].server_url, "https://om.internal:4939");

        let list = remove_profile(&path, "prod_1").unwrap();
        assert!(list.is_empty());
        assert!(
            upsert_profile(
                &path,
                ServerProfile {
                    id: "../bad".into(),
                    name: "bad".into(),
                    server_url: "http://127.0.0.1:4939".into(),
                    last_used_at: 0,
                }
            )
            .is_err()
        );
    }
}
