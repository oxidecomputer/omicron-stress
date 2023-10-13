//! An antagonist that exercises disk lifecycle commands (create, delete).

use anyhow::Context;
use async_trait::async_trait;
use core::result::Result;
use oxide_api::types::BlockSize;
use oxide_api::types::ByteCount;
use oxide_api::types::DiskCreate;
use oxide_api::types::DiskSource;
use oxide_api::types::DiskState;
use oxide_api::types::Name;
use oxide_api::ClientDisksExt;
use tracing::{info, trace, warn};

use crate::actor::AntagonistError;
use crate::util::sleep_random_ms;
use crate::util::unwrap_oxide_api_error;
use crate::util::OxideApiError;

/// The possible actions that this antagonist can take.
#[derive(Debug, Clone, Copy)]
enum Action {
    Wait,
    Create,
    Delete,
}

/// The parameters used to configure a disk antagonist.
pub struct Params {
    /// The name of the project to create this antagonist's disk in.
    pub project: String,

    /// The name of the disk this antagonist should act on.
    pub disk_name: String,
}

/// The internal state for a disk antagonist.
#[derive(Debug)]
pub(super) struct DiskActor {
    client: oxide_api::Client,
    project: String,
    disk_name: String,
}

impl DiskActor {
    /// Creates a new disk antagonist.
    pub(super) fn new(params: Params) -> anyhow::Result<Self> {
        Ok(Self {
            client: crate::client::get_client(crate::config())?,
            project: params.project,
            disk_name: params.disk_name,
        })
    }

    /// Gets this actor's disk's current state.
    ///
    /// # Return value
    ///
    /// - Ok(Some(state)) if the query succeeded.
    /// - Ok(None) if the query failed with a "not found" error.
    /// - Err if the query failed for any other reason.
    async fn get_disk_state(&self) -> Result<Option<DiskState>, OxideApiError> {
        let res = self
            .client
            .disk_view()
            .project(&self.project)
            .disk(&self.disk_name)
            .send()
            .await;

        match res {
            Ok(response_value) => Ok(Some(response_value.into_inner().state)),

            Err(e) => match &e {
                oxide_api::Error::InvalidRequest(_)
                | oxide_api::Error::CommunicationError(_)
                | oxide_api::Error::InvalidResponsePayload(_)
                | oxide_api::Error::UnexpectedResponse(_) => Err(e),

                oxide_api::Error::ErrorResponse(response_value) => {
                    let status = response_value.status();

                    // It's OK if the disk just isn't there. Any other error
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

    /// Asks to create this actor's disk. The created disk size is 1 GB.
    async fn create_disk(&self) -> Result<(), OxideApiError> {
        let body = DiskCreate {
            description: self.disk_name.to_owned(),
            disk_source: DiskSource::Blank {
                block_size: BlockSize::try_from(512_i64).unwrap(),
            },
            name: Name::try_from(&self.disk_name).unwrap(),
            size: ByteCount::from(1024 * 1024 * 1024_u64),
        };

        info!(body = ?body, "sending disk create request");
        let res = self
            .client
            .disk_create()
            .project(&self.project)
            .body(body)
            .send()
            .await;

        if res.is_err() {
            warn!(result = ?res, "disk create request returned");
        } else {
            info!(result = ?res, "disk create request returned");
        }
        unwrap_oxide_api_error(res)
    }

    /// Asks to delete this actor's disk.
    async fn delete_disk(&self) -> Result<(), OxideApiError> {
        info!("sending disk delete request");
        let res = self
            .client
            .disk_delete()
            .project(&self.project)
            .disk(&self.disk_name)
            .send()
            .await;

        if res.is_err() {
            warn!(result = ?res, "disk delete request returned");
        } else {
            info!(result = ?res, "disk delete request returned");
        }
        unwrap_oxide_api_error(res)
    }

    /// Selects an action for this antagonist to take given that its disk was
    /// observed to be in the supplied `state`.
    fn get_next_action(&self, state: DiskState) -> anyhow::Result<Action> {
        use rand::prelude::Distribution;
        let actions = [Action::Wait, Action::Create, Action::Delete];

        let weights = match state {
            // If the disk is still starting up, favour politely waiting for it
            // to finish most of the time, but slightly favour asking for it to
            // be deleted.
            DiskState::Creating => [70, 10, 20],

            // If the disk is detached, equally perform any action on it.
            DiskState::Detached => [35, 30, 35],

            _ => {
                anyhow::bail!(
                    "disk {} unexpectedly in state {:?}",
                    self.disk_name,
                    state,
                );
            }
        };

        let dist = rand::distributions::WeightedIndex::new(weights)
            .context("generating disk action weights")?;
        let mut rng = rand::thread_rng();
        Ok(actions[dist.sample(&mut rng)])
    }
}

#[async_trait]
impl super::Antagonist for DiskActor {
    #[tracing::instrument(level = "info", skip(self), fields(disk_name = self.disk_name))]
    async fn antagonize(&self) -> Result<(), AntagonistError> {
        trace!("querying disk state");
        let state = match self.get_disk_state().await? {
            None => {
                info!("disk doesn't exist, will try to create it");
                return self.create_disk().await.map_err(|e| e.into());
            }
            Some(state) => {
                trace!(?state, "got disk state");
                state
            }
        };

        sleep_random_ms(100).await;

        let action = self.get_next_action(state)?;
        trace!(?action, "selected action");
        let result = match action {
            Action::Wait => Ok(()),
            Action::Create => self.create_disk().await,
            Action::Delete => self.delete_disk().await,
        };

        sleep_random_ms(100).await;

        result.map_err(|e| e.into())
    }
}
