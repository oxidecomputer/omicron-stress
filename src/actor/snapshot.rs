//! An antagonist that exercises snapshot lifecycle commands (create, delete).

use async_trait::async_trait;
use core::result::Result;
use oxide::types::Name;
use oxide::types::SnapshotCreate;
use oxide::types::SnapshotState;
use oxide::ClientDisksExt;
use oxide::ClientSnapshotsExt;
use tracing::{info, trace, warn};

use crate::actor::AntagonistError;
use crate::util::sleep_random_ms;
use crate::util::unwrap_oxide_api_error;
use crate::util::OxideApiError;

#[derive(Debug, Clone)]
enum BailReason {
    /// This snapshot is in an invalid state
    InvalidState { state: SnapshotState },
}

/// The possible actions that this antagonist can take.
#[derive(Debug, Clone)]
enum Action {
    Wait,
    Create,
    Delete,
    Bail { reason: BailReason },
}

/// The parameters used to configure a snapshot antagonist.
pub struct Params {
    /// The name of the project to create this antagonist's snapshots in.
    pub project: String,

    /// The name of the disk this antagonist should act on.
    pub disk_name: String,

    /// The name of the snapshot this antagonist should act on.
    pub snapshot_name: String,
}

/// Local types for the disk create request body used when creating the backing
/// disk for a snapshot. Mirrors the server's current schema.
#[derive(Debug, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum DiskSource {
    Blank { block_size: u64 },
}

#[derive(Debug, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum DiskBackend {
    Distributed { disk_source: DiskSource },
}

#[derive(Debug, serde::Serialize)]
struct DiskCreateBody {
    name: String,
    description: String,
    disk_backend: DiskBackend,
    size: u64,
}

/// The internal state for a snapshot antagonist.
#[derive(Debug)]
pub(super) struct SnapshotActor {
    client: oxide::Client,
    http_client: reqwest::Client,
    host: String,
    project: String,
    disk_name: String,
    snapshot_name: String,
    snapshot_name_counter: std::sync::Mutex<u64>,
}

impl SnapshotActor {
    /// Creates a new snapshot antagonist.
    pub(super) fn new(params: Params) -> anyhow::Result<Self> {
        let (host, http_client) =
            crate::client::get_http_client(crate::config())?;
        Ok(Self {
            client: oxide::Client::new_with_client(&host, http_client.clone()),
            http_client,
            host,
            project: params.project,
            disk_name: params.disk_name,
            snapshot_name: params.snapshot_name,
            snapshot_name_counter: std::sync::Mutex::new(0),
        })
    }

    fn get_snapshot_name(&self) -> String {
        format!(
            "{}{}",
            self.snapshot_name,
            self.snapshot_name_counter.lock().unwrap(),
        )
    }

    async fn create_backing_disk(&self) -> Result<(), OxideApiError> {
        let res = self
            .client
            .disk_view()
            .project(&self.project)
            .disk(&self.disk_name)
            .send()
            .await;

        match res {
            Ok(_) => Ok(()),

            Err(e) => match &e {
                oxide::Error::InvalidRequest(_)
                | oxide::Error::CommunicationError(_)
                | oxide::Error::InvalidResponsePayload(_, _)
                | oxide::Error::UnexpectedResponse(_)
                | oxide::Error::InvalidUpgrade(_)
                | oxide::Error::ResponseBodyError(_)
                | oxide::Error::PreHookError(_) => Err(e),

                oxide::Error::ErrorResponse(response_value) => {
                    let status = response_value.status();

                    if status == http::StatusCode::NOT_FOUND {
                        let body = DiskCreateBody {
                            name: self.disk_name.clone(),
                            description: self.disk_name.clone(),
                            disk_backend: DiskBackend::Distributed {
                                disk_source: DiskSource::Blank {
                                    block_size: 512,
                                },
                            },
                            size: 1024 * 1024 * 1024,
                        };

                        info!(body = ?body, "sending disk create request");
                        let res = self
                            .http_client
                            .post(format!("{}/v1/disks", self.host))
                            .query(&[("project", &self.project)])
                            .json(&body)
                            .send()
                            .await
                            .map_err(oxide::Error::CommunicationError)?;

                        if res.status().is_success() {
                            info!("disk create request returned ok");
                            Ok(())
                        } else {
                            let err = oxide::ResponseValue::<
                                oxide::types::Error,
                            >::from_response(res)
                            .await?;
                            warn!(
                                result = ?err.status(),
                                "disk create request returned error"
                            );
                            Err(oxide::Error::ErrorResponse(err))
                        }
                    } else {
                        Err(e)
                    }
                }
            },
        }
    }

    /// Gets this actor's snapshot's current state.
    ///
    /// # Return value
    ///
    /// - Ok(Some(state)) if the query succeeded.
    /// - Ok(None) if the query failed with a "not found" error.
    /// - Err if the query failed for any other reason.
    async fn get_snapshot_state(
        &self,
    ) -> Result<Option<SnapshotState>, OxideApiError> {
        let res = self
            .client
            .snapshot_view()
            .project(&self.project)
            .snapshot(&self.get_snapshot_name())
            .send()
            .await;

        match res {
            Ok(response_value) => Ok(Some(response_value.into_inner().state)),

            Err(e) => match &e {
                oxide::Error::InvalidRequest(_)
                | oxide::Error::CommunicationError(_)
                | oxide::Error::InvalidResponsePayload(_, _)
                | oxide::Error::UnexpectedResponse(_)
                | oxide::Error::InvalidUpgrade(_)
                | oxide::Error::ResponseBodyError(_)
                | oxide::Error::PreHookError(_) => Err(e),

                oxide::Error::ErrorResponse(response_value) => {
                    let status = response_value.status();

                    // It's OK if the snapshot just isn't there. Any other error
                    // is unexpected.
                    if status == http::StatusCode::NOT_FOUND {
                        Ok(None)
                    } else {
                        Err(e)
                    }
                }
            },
        }
    }

    /// Asks to create this actor's snapshot
    async fn create_snapshot(&self) -> Result<(), OxideApiError> {
        let body = SnapshotCreate {
            name: Name::try_from(&self.get_snapshot_name()).unwrap(),
            description: self.get_snapshot_name(),
            disk: self.disk_name.clone().try_into().unwrap(),
        };

        info!(body = ?body, "sending snapshot create request");
        let res = self
            .client
            .snapshot_create()
            .project(&self.project)
            .body(body)
            .send()
            .await;

        if res.is_err() {
            warn!(result = ?res, "snapshot create request returned");
        } else {
            info!(result = ?res, "snapshot create request returned");
        }

        unwrap_oxide_api_error(res)
    }

    /// Asks to delete this actor's snapshot.
    async fn delete_snapshot(&self) -> Result<(), OxideApiError> {
        info!("sending snapshot delete request");
        let res = self
            .client
            .snapshot_delete()
            .project(&self.project)
            .snapshot(&self.get_snapshot_name())
            .send()
            .await;

        if res.is_err() {
            warn!(result = ?res, "snapshot delete request returned");
        } else {
            info!(result = ?res, "snapshot delete request returned");
        }

        unwrap_oxide_api_error(res)
    }

    /// Selects an action for this antagonist to take given that its snapshot
    /// was observed to be in the supplied `state`.
    fn get_next_action(&self, state: SnapshotState) -> Action {
        use rand::prelude::Distribution;
        let actions = [Action::Wait, Action::Create, Action::Delete];

        let weights = match state {
            // If the snapshot is still starting up, favour politely waiting for it
            // to finish most of the time, but slightly favour asking for it to
            // be deleted.
            SnapshotState::Creating => [70, 10, 20],

            // If the snapshot is ready, equally perform any action on it.
            SnapshotState::Ready => [35, 30, 35],

            // If the snapshot is destroyed, bump the name counter, then
            // equally perform any action on it.
            SnapshotState::Destroyed => {
                *self.snapshot_name_counter.lock().unwrap() += 1;
                [35, 30, 35]
            }

            _ => {
                return Action::Bail {
                    reason: BailReason::InvalidState { state },
                }
            }
        };

        // `new` returns an error if the iterator is empty, if any weight is <
        // 0, or if its total value is 0.
        let dist = rand::distributions::WeightedIndex::new(weights).unwrap();
        let mut rng = rand::thread_rng();
        actions[dist.sample(&mut rng)].clone()
    }
}

#[async_trait]
impl super::Antagonist for SnapshotActor {
    #[tracing::instrument(level = "info", skip(self), fields(snapshot_name = self.snapshot_name))]
    async fn antagonize(&self) -> Result<(), AntagonistError> {
        trace!("querying disk state");
        self.create_backing_disk().await?;

        trace!("querying snapshot state");
        let state = match self.get_snapshot_state().await? {
            None => {
                info!("snapshot doesn't exist, will try to create it");
                return self.create_snapshot().await.map_err(Into::into);
            }
            Some(state) => {
                trace!(?state, "got snapshot state");
                state
            }
        };

        sleep_random_ms(100).await;

        let action = self.get_next_action(state);
        trace!(?action, "selected action");
        let result = match action {
            Action::Wait => Ok(()),
            Action::Create => self.create_snapshot().await,
            Action::Delete => self.delete_snapshot().await,
            Action::Bail { reason } => match reason {
                BailReason::InvalidState { state } => {
                    return Err(AntagonistError::InvalidState(format!(
                        "snapshot {} is in invalid state {:?}",
                        self.snapshot_name, state,
                    )));
                }
            },
        };

        sleep_random_ms(100).await;

        result.map_err(Into::into)
    }
}
