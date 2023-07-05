use std::{collections::HashMap, path::PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::info;

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

/// Reads the contents of a hosts.toml file located in `dir`.
fn read_hosts_toml(mut dir: PathBuf) -> Result<Hosts> {
    dir.push("hosts.toml");
    let hosts = std::fs::read_to_string(dir)?;
    Ok(toml::from_str(&hosts)?)
}

/// Gets an Oxide SDK client. See the doc commens in `[crate::config::Config]`
/// and in the project README for host and token resolution rules.
pub fn get_client(config: &crate::config::Config) -> Result<oxide_api::Client> {
    // Prefer an explicitly-passed host URI to the value of OXIDE_HOST. At least
    // one of these must be specified.
    let host = match config.host_uri.as_ref() {
        Some(host) => host.to_owned(),
        None => std::env::var("OXIDE_HOST").context("reading OXIDE_HOST")?,
    };
    info!(%host, "Nexus URI");

    // If the config containins a directory to search for `hosts.toml`, look
    // there. Otherwise, try to get the current user's home directory and
    // search in its `.config/oxide` subdirectory.
    let hosts_toml_dir = if let Some(dir) = &config.hosts_toml_dir {
        Some(dir.clone())
    } else if let Some(mut path) = dirs::home_dir() {
        path.push(".config/oxide");
        Some(path)
    } else {
        None
    };

    // Attempt to read `hosts.toml` and extract a token from it. If this fails
    // for any reason (`hosts.toml` not found or malformed, or no search path
    // was present), fall back to the OXIDE_TOKEN variable.
    let token = if let Some(hosts_toml_dir) = hosts_toml_dir {
        let hosts_toml = {
            let mut hosts_toml = hosts_toml_dir.clone();
            hosts_toml.push("hosts.toml");
            hosts_toml
        };

        if hosts_toml.exists() {
            info!("reading hosts.toml from {}", hosts_toml_dir.display());
            let hosts = read_hosts_toml(hosts_toml_dir)?;
            info!("attempting to read token from hosts.toml");
            match hosts.hosts.get(&host) {
                Some(entry) => Some(entry.token.clone()),
                None => {
                    info!("no token found in hosts.toml");
                    None
                }
            }
        } else {
            info!("hosts.toml file does not exist");
            None
        }
    } else {
        info!("no search path for hosts.toml");
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

    Ok(oxide_api::Client::new_with_client(&host, rclient))
}
