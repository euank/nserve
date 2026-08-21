use std::ffi::OsString;

use anyhow::{bail, Result};
use clap::Parser;

/// Put a development server on a public ngrok URL.
#[derive(Debug, Parser)]
#[command(version, trailing_var_arg = true)]
pub struct Cli {
    /// Open the public URL in the default browser.
    #[arg(long)]
    pub open: bool,

    /// Forward to this local port instead of detecting a listener.
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
    pub port: Option<u16>,

    /// Request a particular public URL from ngrok.
    #[arg(long)]
    pub url: Option<String>,

    /// Require Google OAuth and an email matching this RE2 expression.
    #[arg(long, value_name = "REGEX")]
    pub google_oauth: Option<String>,

    /// Expose a raw TCP endpoint rather than an HTTP endpoint.
    #[arg(long)]
    pub tcp: bool,

    /// Command to run. Put it after `--`.
    #[arg(required = true, allow_hyphen_values = true)]
    pub command: Vec<OsString>,
}

impl Cli {
    pub fn validate(&self) -> Result<()> {
        if self.tcp && self.google_oauth.is_some() {
            bail!("--google-oauth is only available for HTTP endpoints");
        }
        if self
            .command
            .first()
            .is_none_or(|command| command.is_empty())
        {
            bail!("a command is required after --");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_command_after_separator() {
        let cli = Cli::try_parse_from(["nserve", "--port", "3000", "--", "server", "-d"]).unwrap();
        assert_eq!(cli.port, Some(3000));
        assert_eq!(cli.command, ["server", "-d"]);
    }

    #[test]
    fn rejects_oauth_for_tcp() {
        let cli = Cli::try_parse_from([
            "nserve",
            "--tcp",
            "--google-oauth",
            ".*@example.com",
            "--",
            "server",
        ])
        .unwrap();
        assert!(cli.validate().is_err());
    }
}
