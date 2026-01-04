//! Known hosts management for TOFU (Trust On First Use)
//!
//! Stores trusted certificate fingerprints for peer machines.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::tls::Fingerprint;

/// Known hosts storage
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct KnownHosts {
    /// Map of machine name to trusted fingerprint
    #[serde(default)]
    pub hosts: HashMap<String, KnownHost>,
}

/// A known host entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownHost {
    /// Certificate fingerprint (SHA-256, hex encoded)
    pub fingerprint: String,
    /// When this host was first seen
    #[serde(default = "default_timestamp")]
    pub first_seen: String,
    /// When this host was last seen
    #[serde(default = "default_timestamp")]
    pub last_seen: String,
    /// Optional notes
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

fn default_timestamp() -> String {
    chrono_lite_now()
}

/// Simple timestamp without chrono dependency
fn chrono_lite_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", duration.as_secs())
}

impl KnownHosts {
    /// Get the default known hosts file path
    pub fn default_path() -> PathBuf {
        dirs::config_dir()
            .map(|d| d.join("hyprkvm").join("known_hosts.toml"))
            .unwrap_or_else(|| PathBuf::from("known_hosts.toml"))
    }

    /// Load known hosts from file, or create empty if not exists
    pub fn load(path: &Path) -> Result<Self, KnownHostsError> {
        if !path.exists() {
            tracing::debug!("Known hosts file not found, starting fresh");
            return Ok(Self::default());
        }

        let content = fs::read_to_string(path)
            .map_err(|e| KnownHostsError::Io(format!("Failed to read {:?}: {}", path, e)))?;

        toml::from_str(&content)
            .map_err(|e| KnownHostsError::Parse(format!("Failed to parse {:?}: {}", path, e)))
    }

    /// Save known hosts to file
    pub fn save(&self, path: &Path) -> Result<(), KnownHostsError> {
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| KnownHostsError::Io(format!("Failed to create directory: {}", e)))?;
        }

        let content = toml::to_string_pretty(self)
            .map_err(|e| KnownHostsError::Serialize(e.to_string()))?;

        fs::write(path, content)
            .map_err(|e| KnownHostsError::Io(format!("Failed to write {:?}: {}", path, e)))?;

        tracing::debug!("Saved known hosts to {:?}", path);
        Ok(())
    }

    /// Check if a host is known with the given fingerprint
    pub fn is_trusted(&self, machine_name: &str, fingerprint: &Fingerprint) -> TrustStatus {
        match self.hosts.get(machine_name) {
            None => TrustStatus::Unknown,
            Some(host) => {
                if host.fingerprint == fingerprint.to_hex() {
                    TrustStatus::Trusted
                } else {
                    TrustStatus::Changed {
                        old_fingerprint: host.fingerprint.clone(),
                        new_fingerprint: fingerprint.to_hex(),
                    }
                }
            }
        }
    }

    /// Add or update a trusted host
    pub fn trust_host(&mut self, machine_name: &str, fingerprint: &Fingerprint) {
        let now = chrono_lite_now();
        let fp_hex = fingerprint.to_hex();

        if let Some(existing) = self.hosts.get_mut(machine_name) {
            existing.fingerprint = fp_hex;
            existing.last_seen = now;
            tracing::info!("Updated known host: {}", machine_name);
        } else {
            self.hosts.insert(
                machine_name.to_string(),
                KnownHost {
                    fingerprint: fp_hex,
                    first_seen: now.clone(),
                    last_seen: now,
                    notes: None,
                },
            );
            tracing::info!("Added new known host: {}", machine_name);
        }
    }

    /// Remove a host from known hosts
    pub fn remove_host(&mut self, machine_name: &str) -> bool {
        self.hosts.remove(machine_name).is_some()
    }

    /// Get fingerprint for a known host
    pub fn get_fingerprint(&self, machine_name: &str) -> Option<Fingerprint> {
        self.hosts
            .get(machine_name)
            .and_then(|h| Fingerprint::from_hex(&h.fingerprint).ok())
    }

    /// Update last_seen timestamp for a host
    pub fn touch(&mut self, machine_name: &str) {
        if let Some(host) = self.hosts.get_mut(machine_name) {
            host.last_seen = chrono_lite_now();
        }
    }
}

/// Result of checking trust status
#[derive(Debug, Clone, PartialEq)]
pub enum TrustStatus {
    /// Host is not in known_hosts
    Unknown,
    /// Host is known and fingerprint matches
    Trusted,
    /// Host is known but fingerprint changed (potential MITM!)
    Changed {
        old_fingerprint: String,
        new_fingerprint: String,
    },
}

impl TrustStatus {
    pub fn is_trusted(&self) -> bool {
        matches!(self, TrustStatus::Trusted)
    }

    pub fn is_unknown(&self) -> bool {
        matches!(self, TrustStatus::Unknown)
    }

    pub fn is_changed(&self) -> bool {
        matches!(self, TrustStatus::Changed { .. })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum KnownHostsError {
    #[error("IO error: {0}")]
    Io(String),

    #[error("Parse error: {0}")]
    Parse(String),

    #[error("Serialize error: {0}")]
    Serialize(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_trust_workflow() {
        let mut known_hosts = KnownHosts::default();
        let fp = Fingerprint([0xAB; 32]);

        // Initially unknown
        assert!(known_hosts.is_trusted("test-machine", &fp).is_unknown());

        // Trust it
        known_hosts.trust_host("test-machine", &fp);
        assert!(known_hosts.is_trusted("test-machine", &fp).is_trusted());

        // Different fingerprint should be detected
        let fp2 = Fingerprint([0xCD; 32]);
        assert!(known_hosts.is_trusted("test-machine", &fp2).is_changed());
    }

    #[test]
    fn test_save_load() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("known_hosts.toml");

        let mut known_hosts = KnownHosts::default();
        let fp = Fingerprint([0xAB; 32]);
        known_hosts.trust_host("test-machine", &fp);

        known_hosts.save(&path).unwrap();

        let loaded = KnownHosts::load(&path).unwrap();
        assert!(loaded.is_trusted("test-machine", &fp).is_trusted());
    }
}
