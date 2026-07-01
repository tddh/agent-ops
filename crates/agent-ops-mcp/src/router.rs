//! In-memory host registry that loads target host configurations from a YAML file
//! and provides lookup, listing, and counting operations.

use agent_ops_core::types::HostConfig;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;

/// Maps host names to their `HostConfig` for fast lookup by tool handlers.
pub struct HostRouter {
    hosts: HashMap<String, HostConfig>,
}

impl HostRouter {
    /// Loads and parses a YAML hosts file into an in-memory host registry.
    pub fn from_file(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read hosts file: {}", path.display()))?;

        let registry: agent_ops_core::types::HostRegistry =
            serde_yaml::from_str(&content).context("failed to parse hosts YAML")?;

        let hosts: HashMap<String, HostConfig> = registry
            .hosts
            .into_iter()
            .map(|h| (h.name.clone(), h))
            .collect();

        tracing::info!("loaded {} hosts from {}", hosts.len(), path.display());
        Ok(Self { hosts })
    }

    /// Returns the `HostConfig` for the given host name, or `None` if not found.
    pub fn get(&self, name: &str) -> Option<&HostConfig> {
        self.hosts.get(name)
    }

    /// Returns all registered hosts as a flat list.
    pub fn list(&self) -> Vec<&HostConfig> {
        self.hosts.values().collect()
    }

    /// Returns the total number of registered hosts.
    pub fn len(&self) -> usize {
        self.hosts.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn write_temp_yaml(content: &str) -> std::path::PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "agent-ops-test-hosts-{}-{}.yaml",
            std::process::id(),
            id
        ));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn test_load_valid_yaml() {
        let yaml = r#"
hosts:
  - name: host1
    bridge_addr: 10.0.0.1:9778
    bridge_token: tok1
    group: prod
    tags: [web]
    labels:
      dc: shanghai
  - name: host2
    bridge_addr: 10.0.0.2:9778
    bridge_token: tok2
    group: staging
    tags: [db]
"#;
        let path = write_temp_yaml(yaml);
        let router = HostRouter::from_file(&path).unwrap();
        assert_eq!(router.len(), 2);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_get_existing_host() {
        let yaml = r#"
hosts:
  - name: test-host
    bridge_addr: 10.0.0.1:9778
    bridge_token: abc
"#;
        let path = write_temp_yaml(yaml);
        let router = HostRouter::from_file(&path).unwrap();
        let host = router.get("test-host").expect("host should exist");
        assert_eq!(host.name, "test-host");
        assert_eq!(host.bridge_addr, "10.0.0.1:9778");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_get_non_existing_host() {
        let yaml = r#"
hosts:
  - name: test-host
    bridge_addr: 10.0.0.1:9778
    bridge_token: abc
"#;
        let path = write_temp_yaml(yaml);
        let router = HostRouter::from_file(&path).unwrap();
        assert!(router.get("nonexistent").is_none());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_list_returns_all_hosts() {
        let yaml = r#"
hosts:
  - name: host1
    bridge_addr: 10.0.0.1:9778
    bridge_token: tok1
  - name: host2
    bridge_addr: 10.0.0.2:9778
    bridge_token: tok2
  - name: host3
    bridge_addr: 10.0.0.3:9778
    bridge_token: tok3
"#;
        let path = write_temp_yaml(yaml);
        let router = HostRouter::from_file(&path).unwrap();
        let list = router.list();
        assert_eq!(list.len(), 3);
        let names: Vec<&str> = list.iter().map(|h| h.name.as_str()).collect();
        assert!(names.contains(&"host1"));
        assert!(names.contains(&"host2"));
        assert!(names.contains(&"host3"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_len_returns_correct_count() {
        let yaml = r#"
hosts:
  - name: only
    bridge_addr: 10.0.0.1:9778
    bridge_token: tok
"#;
        let path = write_temp_yaml(yaml);
        let router = HostRouter::from_file(&path).unwrap();
        assert_eq!(router.len(), 1);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_load_invalid_yaml() {
        let yaml = "not: valid: yaml: : broken";
        let path = write_temp_yaml(yaml);
        let result = HostRouter::from_file(&path);
        assert!(result.is_err());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_load_empty_hosts() {
        let yaml = "hosts: []";
        let path = write_temp_yaml(yaml);
        let router = HostRouter::from_file(&path).unwrap();
        assert_eq!(router.len(), 0);
        assert!(router.list().is_empty());
        assert!(router.get("any").is_none());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_load_missing_file() {
        let path = std::env::temp_dir().join("agent-ops-nonexistent-file.yaml");
        let result = HostRouter::from_file(&path);
        assert!(result.is_err());
    }
}
