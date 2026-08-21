use std::{
    env,
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct NgrokConfig {
    #[allow(dead_code)]
    version: Option<u8>,
    authtoken: Option<String>,
    agent: Option<AgentConfig>,
}

#[derive(Debug, Deserialize)]
struct AgentConfig {
    authtoken: Option<String>,
}

/// Resolve an ngrok authtoken without involving the ngrok executable.
///
/// The environment has precedence over the standard Linux agent config. Both
/// the v2 top-level field and v3 `agent.authtoken` field are accepted.
pub fn authtoken() -> Result<Option<String>> {
    resolve(
        env::var_os("NGROK_AUTHTOKEN"),
        default_config_path().as_deref(),
    )
}

fn resolve(environment: Option<OsString>, config_path: Option<&Path>) -> Result<Option<String>> {
    if let Some(token) = environment
        .and_then(|value| value.into_string().ok())
        .and_then(nonempty)
    {
        return Ok(Some(token));
    }

    let Some(path) = config_path else {
        return Ok(None);
    };
    match fs::read_to_string(path) {
        Ok(contents) => parse_config(&contents)
            .with_context(|| format!("failed to parse ngrok config at {}", path.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("failed to read ngrok config at {}", path.display()))
        }
    }
}

fn parse_config(contents: &str) -> Result<Option<String>> {
    let config: NgrokConfig = serde_yaml_ng::from_str(contents)?;
    Ok(config
        .agent
        .and_then(|agent| agent.authtoken)
        .and_then(nonempty)
        .or_else(|| config.authtoken.and_then(nonempty)))
}

fn nonempty(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

fn default_config_path() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(".config/ngrok/ngrok.yml"))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn reads_v2_top_level_token() {
        assert_eq!(
            parse_config("version: 2\nauthtoken: v2-token\n").unwrap(),
            Some("v2-token".into())
        );
    }

    #[test]
    fn reads_v3_nested_token() {
        assert_eq!(
            parse_config("version: 3\nagent:\n  authtoken: v3-token\n").unwrap(),
            Some("v3-token".into())
        );
    }

    #[test]
    fn environment_token_takes_precedence() {
        let mut config = tempfile::NamedTempFile::new().unwrap();
        writeln!(config, "version: 2\nauthtoken: file-token").unwrap();
        assert_eq!(
            resolve(
                Some(OsString::from("environment-token")),
                Some(config.path())
            )
            .unwrap(),
            Some("environment-token".into())
        );
    }

    #[test]
    fn missing_config_is_not_an_error() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve(None, Some(&directory.path().join("missing"))).unwrap(),
            None
        );
    }

    #[test]
    fn malformed_present_config_is_an_error() {
        let mut config = tempfile::NamedTempFile::new().unwrap();
        writeln!(config, "agent: [").unwrap();
        assert!(resolve(None, Some(config.path())).is_err());
    }
}
