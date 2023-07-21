use std::{net::Ipv4Addr, sync::OnceLock};

use actor::{disk, instance, ActorKind};
use anyhow::{Context, Result};
use clap::Parser;
use futures::stream::FuturesUnordered;
use oxide_api::{
    builder::ProjectView,
    types::{IpRange, Ipv4Range, Name, ProjectCreate},
    ClientProjectsExt, ClientSystemNetworkingExt,
};
use std::future::Future;
use std::pin::Pin;
use tokio_stream::StreamExt;
use tracing::{error, info};
use tracing_subscriber::layer::SubscriberExt;

mod actor;
mod client;
mod config;
mod util;

/// The global command-line configuration for a stress runner instance.
pub static CONFIG: OnceLock<config::Config> = OnceLock::new();

/// The stress test project name. In the future the harness can be expanded to
/// have actors that create and destroy projects, but for now the harness
/// focuses on instances.
const PROJECT_NAME: &str = "omicron-stress";

/// Creates the harness's test project and ensures that there are external IPs
/// in its IP pool.
async fn create_test_project(client: &oxide_api::Client) -> Result<()> {
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
    let mut error_futures: FuturesUnordered<
        Pin<Box<dyn Future<Output = Option<anyhow::Error>>>>,
    > = FuturesUnordered::new();

    for inst in 0..config().num_test_instances {
        for actor_index in 0..config().threads_per_instance {
            let (actor, mut error_ch) = actor::Actor::new(
                format!("inst{}_{}", inst, actor_index),
                ActorKind::Instance(instance::Params {
                    project: PROJECT_NAME.to_owned(),
                    instance_name: format!("inst{}", inst),
                }),
            )?;

            actors.push(actor);
            error_futures.push(Box::pin(async move { error_ch.recv().await }));
        }
    }

    for disk in 0..config().num_test_disks {
        for actor_index in 0..config().threads_per_disk {
            let (actor, mut error_ch) = actor::Actor::new(
                format!("disk{}_{}", disk, actor_index),
                ActorKind::Disk(disk::Params {
                    project: PROJECT_NAME.to_owned(),
                    disk_name: format!("disk{}", disk),
                }),
            )?;

            actors.push(actor);
            error_futures.push(Box::pin(async move { error_ch.recv().await }));
        }
    }

    info!("Starting stress test");
    tokio::select! {
        err = error_futures.next() => error!("actor error: {:?}", err),
        _ = ctrlc_rx.recv() => info!("got ctrl-c, exiting"),
    }

    let join_futures = FuturesUnordered::new();
    info!("Halting actors");
    for a in actors {
        join_futures.push(a.halt().await);
    }

    info!("Waiting for actors to halt");
    futures::future::join_all(join_futures).await;

    info!("b'bye");
    Ok(())
}
