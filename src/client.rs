use std::{collections::HashMap, path::PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// The contents of an Oxide CLI `hosts.toml` file.
#[derive(Debug, Deserialize, Serialize)]
struct Hosts {
    /// A map from host names to per-host token and user information.
    #[serde(flatten)]
    hosts: HashMap<String, Host>,
}

/// An individual entry in `hosts.toml`.
#[derive(Debug, Deserialize, Serialize)]
struct Host {
    /// The ID of the user session for this entry.
    user: String,

    /// The authentication token associated with this entry's session.
    token: String,
}

/// The contents of an Oxide CLI `credentials.toml` file.
#[derive(Debug, Deserialize, Serialize)]
struct Credentials {
    /// A map from host names to per-host token and user information.
    profile: HashMap<String, Credential>,
}

/// The contents of an Oxide CLI `credentials.toml` file.
#[derive(Debug, Deserialize, Serialize)]
struct Credential {
    /// The ID of the user session for this entry.
    user: String,

    /// The URL of the host for this entry.
    host: String,

    /// The authentication token associated with this entry's session.
    token: String,
}

/// The supported types of login config files, `credentials.toml` or `hosts.toml`.
#[derive(Copy, Clone, Debug)]
enum ConfigType {
    Credentials,
    Hosts,
}

impl ConfigType {
    /// The name of the login config file.
    fn file_name(&self) -> &str {
        match self {
            ConfigType::Credentials => "credentials.toml",
            ConfigType::Hosts => "hosts.toml",
        }
    }
}

/// An abstraction over `credentials.toml` and `hosts.toml` files.
struct LoginConfig {
    /// The directory containing the login config file.
    dir: PathBuf,

    /// The type of login config file.
    cfg_ty: ConfigType,
}

impl LoginConfig {
    pub fn try_new(dir: PathBuf) -> Option<Self> {
        let creds = dir.clone().join("credentials.toml").exists();
        let hosts = dir.clone().join("hosts.toml").exists();

        let cfg_ty = match (creds, hosts) {
            (true, _) => ConfigType::Credentials,
            (_, true) => ConfigType::Hosts,
            _ => return None,
        };

        Some(Self { dir, cfg_ty })
    }

    /// Read `credentials.toml` in, falling back to `hosts.toml if not present.
    pub fn read_config(&self) -> Result<Hosts> {
        match self.cfg_ty {
            ConfigType::Credentials => self.read_credentials_toml(),
            ConfigType::Hosts => self.read_hosts_toml(),
        }
    }

    /// Reads the contents of a hosts.toml file located in `dir`.
    fn read_hosts_toml(&self) -> Result<Hosts> {
        let dir = self.dir.join("hosts.toml");
        let hosts = std::fs::read_to_string(dir)?;

        warn!("hosts.toml is deprecated. Please migrate to credentials.toml");
        Ok(toml::from_str(&hosts)?)
    }

    /// Reads the contents of a `credentials.toml` file located in `dir`.
    fn read_credentials_toml(&self) -> Result<Hosts> {
        let dir = self.dir.join("credentials.toml");
        let credentials_content = std::fs::read_to_string(dir)?;
        let creds: Credentials = toml::from_str(&credentials_content)?;

        let mut hosts = HashMap::new();

        for cred in creds.profile.into_values() {
            hosts
                .insert(cred.host, Host { user: cred.user, token: cred.token });
        }

        Ok(Hosts { hosts })
    }
}

/// Gets an Oxide SDK client. See the doc commens in `[crate::config::Config]`
/// and in the project README for host and token resolution rules.
pub fn get_client(config: &crate::config::Config) -> Result<oxide::Client> {
    // Prefer an explicitly-passed host URI to the value of OXIDE_HOST. At least
    // one of these must be specified.
    let host = match config.host_uri.as_ref() {
        Some(host) => host.to_owned(),
        None => std::env::var("OXIDE_HOST").context("reading OXIDE_HOST")?,
    };
    info!(%host, "Nexus URI");

    let config_dir =
        match (&config.credentials_toml_dir, &config.hosts_toml_dir) {
            (Some(creds), _) => Some(creds),
            (_, Some(hosts)) => Some(hosts),
            _ => None,
        };

    // If the config containins a directory to search for login credentials, look
    // there. Otherwise, try to get the current user's home directory and
    // search in its `.config/oxide` subdirectory.
    let creds_toml_dir = if let Some(dir) = config_dir {
        Some(dir.clone())
    } else if let Some(mut path) = dirs::home_dir() {
        path.push(".config/oxide");
        Some(path)
    } else {
        None
    };

    // Attempt to read credentials config and extract a token from it. If this fails
    // for any reason (`credentials/hosts.toml` not found or malformed, or no search path
    // was present), fall back to the OXIDE_TOKEN variable.
    let token = if let Some(creds_toml_dir) = creds_toml_dir {
        if let Some(login_config) = LoginConfig::try_new(creds_toml_dir.clone())
        {
            info!("reading credentials from {}", login_config.dir.display());
            let hosts = login_config.read_config()?;
            info!(
                "attempting to read token from {}",
                login_config.cfg_ty.file_name()
            );
            match hosts.hosts.get(&host) {
                Some(entry) => Some(entry.token.clone()),
                None => {
                    info!("no token found");
                    None
                }
            }
        } else {
            info!("could not find credentials.toml or hosts.toml file");
            None
        }
    } else {
        info!("no search path for login credentials");
        None
    };

    let token = match token {
        Some(t) => t,
        None => {
            info!("reading OXIDE_TOKEN from environment");
            std::env::var("OXIDE_TOKEN").context("reading OXIDE_TOKEN")?
        }
    };

    let auth = format!("Bearer {}", token);
    let mut auth_value = reqwest::header::HeaderValue::from_str(&auth)?;
    auth_value.set_sensitive(true);

    // Instance creations can take a while, so pick a relatively generous
    // timeout.
    let timeout = std::time::Duration::from_secs(120);
    let rclient = reqwest::Client::builder()
        .connect_timeout(timeout)
        .timeout(timeout)
        .default_headers(
            [(http::header::AUTHORIZATION, auth_value)].into_iter().collect(),
        )
        .build()
        .unwrap();

    Ok(oxide::Client::new_with_client(&host, rclient))
}
