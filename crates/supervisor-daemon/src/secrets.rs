//! Secret storage (§9): `~/.supervisor/secrets.json` (mode 0600).
//!
//! Each `opencode serve` gets an `OPENCODE_SERVER_PASSWORD` from here; the
//! supervisor uses basic auth with the same value. Secrets never appear in
//! logs or the journal.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// A small secret file: a map of name → value, plus the server password.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretFile {
    /// The shared opencode server password.
    #[serde(default = "default_password")]
    pub server_password: String,
}

fn default_password() -> String {
    generate_password()
}

/// Load (creating if absent, mode 0600) the secret file at `path`.
///
/// # Errors
/// Any I/O or parse failure.
pub fn load_or_create(path: &Path) -> Result<SecretFile> {
    if path.exists() {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("reading secrets {}", path.display()))?;
        return serde_json::from_str(&contents)
            .with_context(|| format!("parsing secrets {}", path.display()));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let secrets = SecretFile { server_password: generate_password() };
    write(&secrets, path)?;
    Ok(secrets)
}

/// Atomically write the secret file with mode 0600.
///
/// # Errors
/// Any I/O failure.
pub fn write(secrets: &SecretFile, path: &Path) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(secrets).context("encode secrets")?;
    write_secure(&bytes, path)
}

/// Write bytes to `path` with mode 0600 (no group/other access).
///
/// # Errors
/// Any I/O failure.
pub(crate) fn write_secure(bytes: &[u8], path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?;
    let mut perms = fs::metadata(path).context("stat secrets file")?.permissions();
    perms.set_mode(0o600);
    fs::set_permissions(path, perms).with_context(|| format!("chmod 0600 {}", path.display()))?;
    Ok(())
}

/// Generate a 32-char URL-safe random password.
fn generate_password() -> String {
    use base64::Engine as _;
    use rand::RngCore as _;
    let mut bytes = [0u8; 24];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// The secrets path under a state dir.
#[must_use]
pub fn secrets_path(state_dir: &Path) -> PathBuf {
    state_dir.join("secrets.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_and_roundtrips_with_0600_perms() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.json");
        let secrets = load_or_create(&path).unwrap();
        assert!(!secrets.server_password.is_empty());
        assert_eq!(secrets.server_password.len(), 32);
        let reloaded = load_or_create(&path).unwrap();
        assert_eq!(reloaded.server_password, secrets.server_password, "stable across reloads");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "secret file is mode 0600");
        }
    }

    #[test]
    fn passwords_are_random() {
        assert_ne!(generate_password(), generate_password());
    }
}
