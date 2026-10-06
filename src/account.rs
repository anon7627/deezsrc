//! Stored Deezer session (in the data directory the host gives the plugin).
//!
//! Only the `arl` cookie is kept, in a file readable by the owner alone. It
//! is never logged: it opens the account like a password would.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Shortest and longest `arl` accepted. Deezer's are 192 hex characters.
const MIN_ARL: usize = 64;
const MAX_ARL: usize = 1024;

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct Credentials {
    pub arl: String,
}

/// Never print the cookie, even in debug output.
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credentials { arl: <hidden> }")
    }
}

fn path(data_dir: &Path) -> PathBuf {
    data_dir.join("account.json")
}

impl Credentials {
    pub fn load(data_dir: &Path) -> Result<Option<Self>> {
        let path = path(data_dir);
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let mode = std::fs::metadata(&path)?.permissions().mode();
        if mode & 0o077 != 0 {
            tracing::warn!("{} is readable by other users; run `chmod 600` on it", path.display());
        }
        let creds: Self = serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        Ok(extract_arl(&creds.arl).map(|arl| Credentials { arl }))
    }

    pub fn save(&self, data_dir: &Path) -> Result<PathBuf> {
        std::fs::create_dir_all(data_dir).context("creating data dir")?;
        let path = path(data_dir);
        let tmp = path.with_extension("json.tmp");
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .with_context(|| format!("writing {}", tmp.display()))?;
            f.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }

    /// Returns whether something was deleted.
    pub fn delete(data_dir: &Path) -> Result<bool> {
        match std::fs::remove_file(path(data_dir)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}

/// The `arl` value out of what the user pasted: the bare value, `arl=…`,
/// a whole `Cookie:` header or a line copied from the browser's cookie
/// table (name, value, domain… separated by tabs). `None` when nothing
/// looks like one.
pub fn extract_arl(input: &str) -> Option<String> {
    let input = input.trim();
    let input = input.strip_prefix("Cookie:").or_else(|| input.strip_prefix("cookie:")).unwrap_or(input);
    let candidate = input
        .split([';', '\n'])
        .map(str::trim)
        .find_map(|part| part.strip_prefix("arl=").or_else(|| part.strip_prefix("arl\t")))
        .unwrap_or(input);
    // A row of the browser's cookie table: the value is the first long field.
    let candidate = candidate.split(['\t', ' ']).map(str::trim).find(|f| f.len() >= MIN_ARL).unwrap_or(candidate);
    let arl = candidate.trim().trim_matches('"');
    let ok = (MIN_ARL..=MAX_ARL).contains(&arl.len()) && arl.chars().all(|c| c.is_ascii_alphanumeric());
    ok.then(|| arl.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARL: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn round_trip_with_private_permissions() {
        let dir = std::env::temp_dir().join(format!("deezsrc-auth-{}", uuid::Uuid::new_v4()));
        let c = Credentials { arl: ARL.into() };
        let path = c.save(&dir).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(Credentials::load(&dir).unwrap(), Some(c.clone()));
        assert!(!format!("{c:?}").contains(ARL), "never printed");
        assert!(Credentials::delete(&dir).unwrap());
        assert_eq!(Credentials::load(&dir).unwrap(), None);
        assert!(!Credentials::delete(&dir).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn arl_is_found_in_what_users_paste() {
        assert_eq!(extract_arl(&format!("  {ARL}\n")).as_deref(), Some(ARL));
        assert_eq!(extract_arl(&format!("arl={ARL}")).as_deref(), Some(ARL));
        assert_eq!(extract_arl(&format!("\"{ARL}\"")).as_deref(), Some(ARL));
        assert_eq!(extract_arl(&format!("Cookie: dzr_uniq_id=x; arl={ARL}; sid=fr123")).as_deref(), Some(ARL));
        assert_eq!(extract_arl(&format!("arl\t{ARL}\t.deezer.com\t/\t2027-01-01")).as_deref(), Some(ARL));
        assert_eq!(extract_arl(""), None);
        assert_eq!(extract_arl("abc123"), None, "too short");
        assert_eq!(extract_arl(&format!("{ARL}/../x")), None, "not alphanumeric");
        assert_eq!(extract_arl("https://www.deezer.com/login"), None);
    }
}
