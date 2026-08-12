//! API key protection: keys live outside `profiles.toml` in a private
//! per-user directory, referenced only by path. Environment variables are
//! supported as an alternative that never touches the filesystem.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::model::Profile;

pub fn home_dir() -> PathBuf {
    std::env::var_os("RAINCODE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".raincode")
        })
}

fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub fn key_ref(id: &str) -> String {
    format!("keys/{}.key", sanitize(id))
}

pub fn key_path(id: &str) -> PathBuf {
    home_dir().join(key_ref(id))
}

pub fn store_key(id: &str, key: &str) -> io::Result<PathBuf> {
    let path = key_path(id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        lock_down_dir(parent);
    }
    fs::write(&path, key.trim().as_bytes())?;
    lock_down(&path);
    Ok(path)
}

pub fn delete_key(id: &str) -> io::Result<()> {
    match fs::remove_file(key_path(id)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Move a plaintext key out of the profile into the private key store.
pub fn protect_profile(profile: &mut Profile) -> io::Result<()> {
    if let Some(key) = profile.api_key.take() {
        if !key.trim().is_empty() {
            store_key(&profile.id, &key)?;
            profile.api_key_file = Some(key_ref(&profile.id));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn lock_down_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
}

#[cfg(windows)]
fn lock_down_dir(path: &Path) {
    // Test builds run inside restricted Windows sessions where icacls can
    // revoke the current process token; production builds still harden.
    if cfg!(test) { return; }
    if let Ok(user) = std::env::var("USERNAME") {
        let _ = std::process::Command::new("icacls")
            .arg(path)
            .arg("/inheritance:r")
            .arg("/grant:r")
            .arg(format!("{user}:(F)"))
            .output();
    }
}

#[cfg(unix)]
fn lock_down(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(permissions) = fs::metadata(path).map(|m| m.permissions()) {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
}

#[cfg(windows)]
fn lock_down(path: &Path) {
    // Best-effort ACL: remove inherited permissions, grant only the current
    // user full control (including delete for `model remove`). A missing
    // icacls must never fail key setup.
    if cfg!(test) { return; }
    if let Ok(user) = std::env::var("USERNAME") {
        let _ = std::process::Command::new("icacls")
            .arg(path)
            .arg("/inheritance:r")
            .arg("/grant:r")
            .arg(format!("{user}:(F)"))
            .output();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProfileKind;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn test_profile(id: &str) -> Profile {
        Profile {
            id: id.into(),
            name: id.into(),
            app: "raincode".into(),
            kind: ProfileKind::Mock,
            base_url: String::new(),
            model: "mock-1".into(),
            api_key: Some("sk-super-secret".into()),
            api_key_env: None,
            embedding_model: None,
            headers: Default::default(),
            extra: serde_json::json!({}),
            api_key_file: None,
        }
    }

    #[test]
    fn store_load_delete_roundtrip() {
        let _guard = ENV_LOCK.lock().unwrap();
        let home = std::env::temp_dir().join(format!("raincode-secrets-{}", std::process::id()));
        std::env::set_var("RAINCODE_HOME", &home);
        let path = store_key("demo", " sk-abc \n").unwrap();
        assert_eq!(fs::read_to_string(key_path("demo")).unwrap(), "sk-abc");
        assert!(path.exists());
        delete_key("demo").unwrap();
        assert!(!path.exists());
        std::env::remove_var("RAINCODE_HOME");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn protect_profile_moves_key_out_of_toml() {
        let _guard = ENV_LOCK.lock().unwrap();
        let home =
            std::env::temp_dir().join(format!("raincode-secrets-protect-{}", std::process::id()));
        std::env::set_var("RAINCODE_HOME", &home);
        let mut profile = test_profile("secure");
        protect_profile(&mut profile).unwrap();
        assert!(profile.api_key.is_none());
        assert!(profile.api_key_file.is_some());
        assert_eq!(fs::read_to_string(key_path("secure")).unwrap(), "sk-super-secret");
        delete_key("secure").unwrap();
        std::env::remove_var("RAINCODE_HOME");
        let _ = fs::remove_dir_all(&home);
    }
}
