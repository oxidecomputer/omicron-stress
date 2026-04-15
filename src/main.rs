use std::{net::Ipv4Addr, sync::OnceLock};

use actor::{disk, instance, snapshot, ActorKind};
use anyhow::{Context, Result};
use clap::Parser;
use futures::stream::FuturesUnordered;
use oxide::{
    builder::ProjectView,
    types::{IpRange, Ipv4Range, Name, ProjectCreate},
    ClientProjectsExt, ClientSystemNetworkingExt,
};
use tracing::{error, info};
use tracing_subscriber::layer::SubscriberExt;

mod actor;
mod client;
mod config;
mod util;

use actor::AntagonistError;
use util::fail_if_500;
use util::fail_if_no_response;

/// The global command-line configuration for a stress runner instance.
pub static CONFIG: OnceLock<config::Config> = OnceLock::new();

/// The stress test project name. In the future the harness can be expanded to
/// have actors that create and destroy projects, but for now the harness
/// focuses on instances.
const PROJECT_NAME: &str = "omicron-stress";

/// Creates the harness's test project and ensures that there are external IPs
/// in its IP pool.
async fn create_test_project(client: &oxide::Client) -> Result<()> {
    info!("Checking for existing stress project");
    if ProjectView::new(client).project(PROJECT_NAME).send().await.is_ok() {
        info!("Project already exists");
    } else {
        info!("Stress project doesn't exist, creating it");
        let body = ProjectCreate {
            name: Name::try_from(PROJECT_NAME.to_owned()).unwrap(),
            description: "Omicron stress".to_owned(),
        };
        client.project_create().body(body).send().await?;
        info!("Successfully created test project!");
    }

    info!("Checking for IPs in default IP pool");
    let ranges =
        client.ip_pool_range_list().pool("default").send().await?.into_inner();
    if ranges.items.is_empty() {
        info!("No IPs found in pool, adding some");
        let range = IpRange::V4(Ipv4Range {
            first: Ipv4Addr::new(168, 254, 1, 100),
            last: Ipv4Addr::new(168, 254, 1, 110),
        });
        client.ip_pool_range_add().pool("default").body(range).send().await?;
        info!("Added IPs to pool");
    } else {
        info!("Default IP pool has IPs, won't add any");
    }

    Ok(())
}

/// Sets a subscriber that emits tracing messages to stdout.
fn set_tracing_subscriber() {
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(tracing::Level::INFO.into());
    let sub =
        tracing_subscriber::Registry::default().with(filter.from_env_lossy());
    let stdout_log = tracing_subscriber::fmt::layer().with_line_number(true);
    let sub = sub.with(stdout_log);
    tracing::subscriber::set_global_default(sub).unwrap();
}

/// Yields a reference to the global command-line config.
pub fn config() -> &'static config::Config {
    CONFIG.get_or_init(config::Config::parse)
}

#[tokio::main]
async fn main() -> Result<()> {
    // Preload the config (and exit if the command-line options couldn't be
    // parsed) before doing any other work.
    let _ = config();
    set_tracing_subscriber();

    let (ctrlc_tx, mut ctrlc_rx) = tokio::sync::mpsc::unbounded_channel();
    ctrlc::set_handler(move || {
        let _ = ctrlc_tx.send(());
    })
    .context("setting Ctrl-C handler")?;

    let client = client::get_client(config()).context("getting client")?;
    create_test_project(&client).await?;

    let mut actors = Vec::new();
    let mut error_channels: Vec<_> = Vec::new();

    for inst in 0..config().num_test_instances {
        for actor_index in 0..config().threads_per_instance {
            let (actor, error_ch) = actor::Actor::new(
                format!("inst{}_{}", inst, actor_index),
                ActorKind::Instance(instance::Params {
                    project: PROJECT_NAME.to_owned(),
                    instance_name: format!("inst{}", inst),
                }),
            )?;

            error_channels.push((actor.name().to_string(), error_ch));
            actors.push(actor);
        }
    }

    for disk in 0..config().num_test_disks {
        for actor_index in 0..config().threads_per_disk {
            let (actor, error_ch) = actor::Actor::new(
                format!("disk{}_{}", disk, actor_index),
                ActorKind::Disk(disk::Params {
                    project: PROJECT_NAME.to_owned(),
                    disk_name: format!("disk{}", disk),
                    backend: disk::DiskBackendKind::Distributed,
                }),
            )?;

            error_channels.push((actor.name().to_string(), error_ch));
            actors.push(actor);
        }
    }

    for disk in 0..config().num_test_local_disks {
        for actor_index in 0..config().threads_per_local_disk {
            let (actor, error_ch) = actor::Actor::new(
                format!("ldisk{}_{}", disk, actor_index),
                ActorKind::Disk(disk::Params {
                    project: PROJECT_NAME.to_owned(),
                    disk_name: format!("ldisk{}", disk),
                    backend: disk::DiskBackendKind::Local,
                }),
            )?;

            error_channels.push((actor.name().to_string(), error_ch));
            actors.push(actor);
        }
    }

    for snapshot in 0..config().num_test_snapshots {
        for actor_index in 0..config().threads_per_snapshot {
            let (actor, error_ch) = actor::Actor::new(
                format!("snapshot{}_{}", snapshot, actor_index),
                ActorKind::Snapshot(snapshot::Params {
                    project: PROJECT_NAME.to_owned(),
                    disk_name: if config().snapshots_use_same_disk {
                        format!("disk{}", snapshot)
                    } else {
                        format!("disk{}{}", snapshot, actor_index)
                    },
                    snapshot_name: format!("snapshot{}", snapshot),
                }),
            )?;

            error_channels.push((actor.name().to_string(), error_ch));
            actors.push(actor);
        }
    }

    let (error_tx, mut error_rx) =
        tokio::sync::mpsc::channel::<(String, AntagonistError)>(1);

    for (name, mut error_ch) in error_channels {
        let error_tx = error_tx.clone();
        tokio::spawn(async move {
            loop {
                match error_ch.recv().await {
                    Some(e) => {
                        let _ = error_tx.send((name.clone(), e)).await;
                    }

                    None => {
                        let _ = error_tx
                            .send((
                                name.clone(),
                                AntagonistError::DisconnectedErrorChannel {
                                    name,
                                },
                            ))
                            .await;
                        break;
                    }
                }
            }
        });
    }

    info!("Starting stress test");
    let mut collected_errors: Vec<(String, String)> = Vec::new();
    loop {
        tokio::select! {
            err = error_rx.recv() => {
                match err {
                    None => {
                        error!("error_rx disconnected!");
                        break;
                    }

                    Some((actor_name, err)) => {
                        match err {
                            AntagonistError::ApiError(api_err) => {
                                // Capture the error message before the value
                                // is consumed by the fatality check below.
                                let err_str = format!("{api_err:?}");
                                let is_fatal = if config().server_errors_fatal {
                                    fail_if_500(api_err).is_err()
                                } else {
                                    fail_if_no_response(api_err).is_err()
                                };
                                collected_errors
                                    .push((actor_name.clone(), err_str));
                                if is_fatal {
                                    error!(
                                        "actor error: {}",
                                        collected_errors.last().unwrap().1
                                    );
                                    break;
                                }
                            }

                            AntagonistError::InvalidState(_)
                            | AntagonistError::DisconnectedErrorChannel { .. } => {
                                let err_str = format!("{err}");
                                error!("{err_str}");
                                collected_errors.push((actor_name, err_str));
                                break;
                            }
                        }
                    }
                }
            }

            _ = ctrlc_rx.recv() => {
                info!("got ctrl-c, exiting");
                break;
            }
        }
    }

    let join_futures = FuturesUnordered::new();
    info!("Halting actors");
    // Grab the operation-count arcs before consuming the actors. The counts
    // are read after join_all so that all tasks have fully stopped.
    let mut actor_arcs: Vec<(String, std::sync::Arc<std::sync::atomic::AtomicU64>)> =
        Vec::new();
    for a in actors {
        actor_arcs.push((a.name().to_string(), a.operations_arc()));
        join_futures.push(a.halt().await);
    }

    info!("Waiting for actors to halt");
    futures::future::join_all(join_futures).await;

    info!("b'bye");

    // Print the summary after all tasks have finished so operation counts are
    // final and errors are complete.
    println!();
    println!("=== Stress Test Summary ===");
    println!();
    println!("Operations per actor:");
    let total_ops: u64 = actor_arcs
        .iter()
        .map(|(_, arc)| arc.load(std::sync::atomic::Ordering::Relaxed))
        .sum();
    for (name, arc) in &actor_arcs {
        println!(
            "  {name}: {}",
            arc.load(std::sync::atomic::Ordering::Relaxed)
        );
    }
    println!("  Total: {total_ops}");
    println!();
    // How many distinct error messages to show per actor-type group.
    const MAX_ERRORS_SHOWN: usize = 5;

    if collected_errors.is_empty() {
        println!("No failures recorded.");
    } else {
        println!("Failures ({} total):", collected_errors.len());
        println!();

        // Print failures grouped by actor type, deduplicating identical error
        // messages within each group. Only the top MAX_ERRORS_SHOWN most
        // frequent unique errors are printed; the rest are counted.
        let actor_types: &[(&str, &str)] = &[
            ("Instance", "inst"),
            ("Disk", "disk"),
            ("Local disk", "ldisk"),
            ("Snapshot", "snapshot"),
        ];

        for (type_label, prefix) in actor_types {
            let type_errors: Vec<(&str, &str)> = collected_errors
                .iter()
                .filter(|(actor, _)| actor.starts_with(prefix))
                .map(|(actor, msg)| (actor.as_str(), msg.as_str()))
                .collect();

            if type_errors.is_empty() {
                continue;
            }

            // Group by exact error message, accumulating actor names.
            let mut by_msg: std::collections::HashMap<&str, Vec<&str>> =
                std::collections::HashMap::new();
            for (actor, msg) in &type_errors {
                by_msg.entry(msg).or_default().push(actor);
            }

            // Sort most-frequent errors first.
            let mut entries: Vec<(&str, Vec<&str>)> =
                by_msg.into_iter().collect();
            entries.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

            println!(
                "  {}s ({} failure{}, {} unique):",
                type_label,
                type_errors.len(),
                if type_errors.len() == 1 { "" } else { "s" },
                entries.len(),
            );
            for (msg, actors) in entries.iter().take(MAX_ERRORS_SHOWN) {
                let count = actors.len();
                let mut unique_actors = actors.to_vec();
                unique_actors.sort();
                unique_actors.dedup();
                println!("    {count}x [{}] {msg}", unique_actors.join(", "));
            }
            if entries.len() > MAX_ERRORS_SHOWN {
                println!(
                    "    ... and {} more unique error{}",
                    entries.len() - MAX_ERRORS_SHOWN,
                    if entries.len() - MAX_ERRORS_SHOWN == 1 { "" } else { "s" },
                );
            }
            println!();
        }

        // Catch-all for any actor names that don't match a known prefix.
        let known_prefixes = ["inst", "disk", "ldisk", "snapshot"];
        let other_errors: Vec<(&str, &str)> = collected_errors
            .iter()
            .filter(|(actor, _)| {
                !known_prefixes.iter().any(|p| actor.starts_with(p))
            })
            .map(|(actor, msg)| (actor.as_str(), msg.as_str()))
            .collect();

        if !other_errors.is_empty() {
            let mut by_msg: std::collections::HashMap<&str, Vec<&str>> =
                std::collections::HashMap::new();
            for (actor, msg) in &other_errors {
                by_msg.entry(msg).or_default().push(actor);
            }
            let mut entries: Vec<(&str, Vec<&str>)> =
                by_msg.into_iter().collect();
            entries.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

            println!(
                "  Other ({} failure{}, {} unique):",
                other_errors.len(),
                if other_errors.len() == 1 { "" } else { "s" },
                entries.len(),
            );
            for (msg, actors) in entries.iter().take(MAX_ERRORS_SHOWN) {
                let count = actors.len();
                let mut unique_actors = actors.to_vec();
                unique_actors.sort();
                unique_actors.dedup();
                println!("    {count}x [{}] {msg}", unique_actors.join(", "));
            }
            if entries.len() > MAX_ERRORS_SHOWN {
                println!(
                    "    ... and {} more unique error{}",
                    entries.len() - MAX_ERRORS_SHOWN,
                    if entries.len() - MAX_ERRORS_SHOWN == 1 { "" } else { "s" },
                );
            }
            println!();
        }
    }
    println!("===========================");

    Ok(())
}
