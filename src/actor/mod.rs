//! Provides `Actor`s: wrappers around individual tasks that submit API calls to
//! Nexus.

use anyhow::Result;
use async_trait::async_trait;
use tracing::{info, info_span, Instrument};

pub mod disk;
pub mod instance;
pub mod snapshot;

use crate::util::OxideApiError;

/// The kinds of actors this module can instantiate.
pub enum ActorKind {
    /// Creates, starts, stops, and destroys instances.
    Instance(instance::Params),

    /// Creates and deletes disks.
    Disk(disk::Params),

    /// Creates and deletes snapshots.
    Snapshot(snapshot::Params),
}

/// An individual actor task.
pub struct Actor {
    /// The tracing span to use for actions taken by this actor.
    span: tracing::Span,

    /// A handle to the actor's internal task.
    task: tokio::task::JoinHandle<()>,

    /// The sender side of a channel used to pause the actor task. The protocol
    /// is to send `true` through this channel, then receive from `paused_rx`,
    /// then send `false` through this channel to unpause.
    pause_tx: tokio::sync::mpsc::Sender<bool>,

    /// Receives a message from the actor task when it has successfully paused.
    paused_rx: tokio::sync::mpsc::Receiver<()>,

    /// Sending to this channel directs the actor task to halt at the next
    /// available opportunity.
    halt_tx: tokio::sync::oneshot::Sender<()>,
}

#[derive(thiserror::Error, Debug)]
pub enum AntagonistError {
    #[error("anyhow error")]
    AnyhowError(#[from] anyhow::Error),

    #[error("oxide api error")]
    ApiError(#[from] OxideApiError),
}

/// A trait implemented by each kind of antagonist actor.
#[async_trait]
trait Antagonist: Send + Sync + 'static {
    async fn antagonize(&self) -> Result<(), AntagonistError>;
}

/// Creates an antagonist of the specified kind.
fn make_antagonist(kind: ActorKind) -> Result<Box<dyn Antagonist>> {
    match kind {
        ActorKind::Instance(params) => {
            Ok(Box::new(instance::InstanceActor::new(params)?))
        }

        ActorKind::Disk(params) => Ok(Box::new(disk::DiskActor::new(params)?)),

        ActorKind::Snapshot(params) => {
            Ok(Box::new(snapshot::SnapshotActor::new(params)?))
        }
    }
}

impl Actor {
    /// Creates a new actor with the specified actor `name` and `kind`.
    ///
    /// # Return value
    ///
    /// A tuple containing the new `Actor` and the receiver side of a channel
    /// that will be sent any errors generated by the task's antagonist.
    pub fn new(
        name: String,
        kind: ActorKind,
    ) -> Result<(Self, tokio::sync::mpsc::Receiver<AntagonistError>)> {
        let span = info_span!("actor", name = &name);
        let (error_tx, error_rx) = tokio::sync::mpsc::channel(1);
        let (pause_tx, mut pause_rx) = tokio::sync::mpsc::channel::<bool>(1);
        let (paused_tx, paused_rx) = tokio::sync::mpsc::channel(1);
        let (halt_tx, mut halt_rx) = tokio::sync::oneshot::channel();

        let antagonist = make_antagonist(kind)?;

        let task = tokio::spawn(
            async move {
                loop {
                    // If the harness asked this actor to stop, then stop.
                    if halt_rx.try_recv().is_ok() {
                        break;
                    }

                    // If the harness asked to pause, then pause.
                    if let Ok(should_pause) = pause_rx.try_recv() {
                        assert!(
                            should_pause,
                            "should only ask to pause when unpaused"
                        );

                        // Tell the harness that this actor is paused, leaving
                        // if the harness is no longer around to listen.
                        if paused_tx.send(()).await.is_err() {
                            break;
                        }

                        // Wait to be told to unpause. If the channel goes away,
                        // the harness exited, so just leave.
                        if let Some(should_unpause) = pause_rx.recv().await {
                            assert!(
                                should_unpause,
                                "should only ask to unpause when paused"
                            );
                        } else {
                            break;
                        }
                    }

                    let result = antagonist.antagonize().await;
                    if let Err(e) = result {
                        if error_tx.send(e).await.is_err() {
                            break;
                        }
                    }
                }
            }
            .instrument(span.clone()),
        );

        Ok((Self { span, task, pause_tx, paused_rx, halt_tx }, error_rx))
    }

    /// Directs this actor to pause and waits for it to report that it has done
    /// so.
    #[allow(dead_code)]
    pub async fn pause(&mut self) {
        let _span = self.span.enter();
        info!("sending pause request");
        self.pause_tx.send(true).await.unwrap();
        info!("waiting for task to pause");
        self.paused_rx.recv().await.unwrap();
    }

    /// Directs this actor to resume.
    #[allow(dead_code)]
    pub async fn resume(&self) {
        let _span = self.span.enter();
        info!("sending resume request");
        self.pause_tx.send(false).await.unwrap();
    }

    /// Directs this actor to halt.
    pub async fn halt(self) -> tokio::task::JoinHandle<()> {
        let _span = self.span.enter();
        info!("sending halt request");
        let _ = self.halt_tx.send(());
        self.task
    }
}
