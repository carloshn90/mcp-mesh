//! TOML configuration for the `mcp-mesh` CLI: which stdio upstreams to spawn.
//!
//! ```toml
//! [[upstream]]
//! id = "pg"
//! command = "mcp-server-postgres"
//! args = ["--connection-string", "postgres://localhost/app"]
//!
//! [upstream.env]
//! RUST_LOG = "info"
//! ```

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use serde::Deserialize;

use mcp_mesh_core::UpstreamSpec;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Config {
    #[serde(rename = "upstream", default)]
    pub upstreams: Vec<UpstreamConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpstreamConfig {
    pub id: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

impl Config {
    pub fn parse(toml_source: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(toml_source)
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let contents = std::fs::read_to_string(path).map_err(ConfigError::Io)?;
        Self::parse(&contents).map_err(ConfigError::Parse)
    }
}

impl From<UpstreamConfig> for UpstreamSpec {
    fn from(config: UpstreamConfig) -> Self {
        UpstreamSpec {
            id: config.id,
            command: config.command,
            args: config.args,
            env: config.env.into_iter().collect(),
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Parse(toml::de::Error),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(err) => write!(f, "failed to read config file: {err}"),
            ConfigError::Parse(err) => write!(f, "failed to parse config file: {err}"),
        }
    }
}

impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_an_empty_config() {
        let config = Config::parse("").unwrap();
        assert!(config.upstreams.is_empty());
    }

    #[test]
    fn parses_upstreams_with_args_and_env() {
        let source = r#"
            [[upstream]]
            id = "pg"
            command = "mcp-server-postgres"
            args = ["--read-only"]

            [upstream.env]
            RUST_LOG = "info"

            [[upstream]]
            id = "fs"
            command = "mcp-server-filesystem"
        "#;

        let config = Config::parse(source).unwrap();
        assert_eq!(config.upstreams.len(), 2);

        let pg = &config.upstreams[0];
        assert_eq!(pg.id, "pg");
        assert_eq!(pg.command, "mcp-server-postgres");
        assert_eq!(pg.args, vec!["--read-only".to_string()]);
        assert_eq!(pg.env.get("RUST_LOG"), Some(&"info".to_string()));

        let fs = &config.upstreams[1];
        assert_eq!(fs.id, "fs");
        assert!(fs.args.is_empty());
        assert!(fs.env.is_empty());
    }

    #[test]
    fn rejects_malformed_toml_syntax() {
        let err = Config::parse("this is not [ valid toml").unwrap_err();
        let _ = err.to_string(); // just confirm it's a reportable error
    }

    #[test]
    fn rejects_an_upstream_missing_a_required_field() {
        let source = r#"
            [[upstream]]
            id = "pg"
        "#; // missing `command`
        assert!(Config::parse(source).is_err());
    }

    #[test]
    fn upstream_config_converts_into_an_upstream_spec() {
        let mut env = HashMap::new();
        env.insert("RUST_LOG".to_string(), "info".to_string());
        let config = UpstreamConfig {
            id: "pg".to_string(),
            command: "mcp-server-postgres".to_string(),
            args: vec!["--read-only".to_string()],
            env,
        };

        let spec: UpstreamSpec = config.into();
        assert_eq!(spec.id, "pg");
        assert_eq!(spec.command, "mcp-server-postgres");
        assert_eq!(spec.args, vec!["--read-only".to_string()]);
        assert_eq!(spec.env, vec![("RUST_LOG".to_string(), "info".to_string())]);
    }

    #[test]
    fn load_surfaces_an_io_error_for_a_missing_file() {
        let err = Config::load(Path::new("/nonexistent/mcp-mesh.toml")).unwrap_err();
        assert!(matches!(err, ConfigError::Io(_)));
    }
}
