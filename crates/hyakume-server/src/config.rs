use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_bind")]
    pub bind: SocketAddr,
    /// Must be on local persistent storage, not a network filesystem.
    pub database: PathBuf,
    #[serde(default = "default_session_idle_days")]
    pub session_idle_days: u32,
    /// Enables `/v1/dev/login`, which lets anyone reaching the port act as any
    /// configured test identity. Never enable it with real data.
    #[serde(default)]
    pub dev_mode: bool,
    /// Say yes to a dev-mode server being reachable from other machines.
    /// Without it, dev mode refuses anything but loopback.
    #[serde(default)]
    pub dev_allow_remote: bool,
    #[serde(default)]
    pub dev: DevConfig,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevConfig {
    #[serde(default)]
    pub identities: Vec<DevIdentity>,
    #[serde(default)]
    pub workspaces: Vec<DevWorkspace>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevIdentity {
    pub name: String,
    pub display_name: Option<String>,
}

/// Synthetic workspace created at startup if its slug does not exist yet.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevWorkspace {
    pub slug: String,
    pub name: String,
    pub owner: String,
    #[serde(default)]
    pub members: Vec<DevMember>,
    #[serde(default)]
    pub projects: Vec<DevProject>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevMember {
    pub identity: String,
    /// Role names, such as "Member".
    #[serde(default)]
    pub roles: Vec<String>,
    /// Project keys. A non-empty list makes this a guest limited to them.
    #[serde(default)]
    pub guest_projects: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevProject {
    pub key: String,
    pub name: String,
}

fn default_bind() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 7878))
}

fn default_session_idle_days() -> u32 {
    90
}

impl Config {
    /// Reads the TOML file, then applies `HYAKUME_SERVER_*` environment overrides.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let mut config: Config =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        config.apply_env(|name| std::env::var(name).ok())?;
        // A relative database path is relative to the config file, not the
        // directory the server happened to start in.
        if config.database.is_relative()
            && let Some(dir) = path.parent()
        {
            config.database = dir.join(&config.database);
        }
        config.validate()?;
        Ok(config)
    }

    fn apply_env(&mut self, var: impl Fn(&str) -> Option<String>) -> anyhow::Result<()> {
        if let Some(bind) = var("HYAKUME_SERVER_BIND") {
            self.bind = bind.parse().context("HYAKUME_SERVER_BIND")?;
        }
        if let Some(database) = var("HYAKUME_SERVER_DATABASE") {
            self.database = database.into();
        }
        if let Some(dev) = var("HYAKUME_SERVER_DEV_MODE") {
            self.dev_mode = parse_bool(&dev).context("HYAKUME_SERVER_DEV_MODE")?;
        }
        if let Some(remote) = var("HYAKUME_SERVER_DEV_ALLOW_REMOTE") {
            self.dev_allow_remote =
                parse_bool(&remote).context("HYAKUME_SERVER_DEV_ALLOW_REMOTE")?;
        }
        Ok(())
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.dev_mode && !self.bind.ip().is_loopback() && !self.dev_allow_remote {
            bail!(
                "dev_mode is on and bind address {} is reachable from other machines. \
                 Dev login lets anyone who can reach the port act as any test identity. \
                 Bind to 127.0.0.1 and use SSH port forwarding, or set \
                 dev_allow_remote = true to acknowledge a deliberately exposed test server.",
                self.bind
            );
        }
        if self.session_idle_days == 0 {
            bail!("session_idle_days must be at least 1");
        }
        Ok(())
    }

    pub fn session_idle_ms(&self) -> i64 {
        i64::from(self.session_idle_days) * 24 * 60 * 60 * 1000
    }
}

fn parse_bool(s: &str) -> anyhow::Result<bool> {
    match s.trim() {
        "1" | "true" => Ok(true),
        "0" | "false" => Ok(false),
        other => bail!("expected true or false, got {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(toml_text: &str) -> Config {
        toml::from_str(toml_text).expect("valid toml")
    }

    #[test]
    fn dev_mode_refuses_a_public_bind_unless_acknowledged() {
        let exposed = config("database = 'x.db'\ndev_mode = true\nbind = '0.0.0.0:7878'");
        assert!(exposed.validate().is_err());

        let acknowledged = config(
            "database = 'x.db'\ndev_mode = true\nbind = '0.0.0.0:7878'\ndev_allow_remote = true",
        );
        assert!(acknowledged.validate().is_ok());

        let loopback = config("database = 'x.db'\ndev_mode = true");
        assert!(loopback.validate().is_ok());
    }

    #[test]
    fn environment_overrides_the_file() {
        let mut c = config("database = 'x.db'");
        c.apply_env(|name| match name {
            "HYAKUME_SERVER_BIND" => Some("0.0.0.0:9000".into()),
            "HYAKUME_SERVER_DEV_MODE" => Some("true".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(c.bind.port(), 9000);
        // The override turns dev mode on, so the loopback rule now applies.
        assert!(c.validate().is_err());
    }
}
