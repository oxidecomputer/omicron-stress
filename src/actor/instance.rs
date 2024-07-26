//! An antagonist that exercises instance lifecycle commands (create, start,
//! stop, destroy).

use async_trait::async_trait;
use core::result::Result;
use oxide_api::{types::InstanceState, ClientInstancesExt};
use tracing::{info, trace, warn};

use crate::actor::AntagonistError;
use crate::util::sleep_random_ms;
use crate::util::unwrap_oxide_api_error;
use crate::util::OxideApiError;

/// The possible actions that the antagonist can take.
#[derive(Debug, Clone, Copy)]
enum Action {
    Wait,
    Create,
    Start,
    Stop,
    Destroy,
}

/// The parameters used to configure an instance antagonist.
pub struct Params {
    /// The name of the project to create this antagonist's instance in.
    pub project: String,

    /// The name of the instance this antagonist should act on.
    pub instance_name: String,
}

/// The internal state for an instance antagonist.
#[derive(Debug)]
pub(super) struct InstanceActor {
    client: oxide_api::Client,
    project: String,
    instance_name: String,
}

impl InstanceActor {
    /// Creates a new instance antagonist.
    pub(super) fn new(params: Params) -> anyhow::Result<Self> {
        Ok(Self {
            client: crate::client::get_client(crate::config())?,
            project: params.project,
            instance_name: params.instance_name,
        })
    }

    /// Gets this actor's instance's current state.
    ///
    /// # Return value
    ///
    /// - Ok(Some(state)) if the query succeeded.
    /// - Ok(None) if the query failed with a "not found" error.
    /// - Err if the query failed for any other reason.
    async fn get_instance_state(
        &self,
    ) -> Result<Option<InstanceState>, OxideApiError> {
        let res = self
            .client
            .instance_view()
            .project(&self.project)
            .instance(&self.instance_name)
            .send()
            .await;

        match res {
            Ok(response_value) => {
                Ok(Some(response_value.into_inner().run_state))
            }
            Err(e) => match &e {
                oxide_api::Error::InvalidRequest(_)
                | oxide_api::Error::CommunicationError(_)
                | oxide_api::Error::InvalidResponsePayload(_)
                | oxide_api::Error::UnexpectedResponse(_) => Err(e),
                oxide_api::Error::ErrorResponse(response_value) => {
                    let status = response_value.status();

                    // It's OK if the instance just isn't there. Any other error
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

    /// Asks to create this actor's instance. The created instance has 1 vCPU,
    /// 1 GB RAM, and no disks or NICs.
    async fn create_instance(&self) -> Result<(), OxideApiError> {
        let body = oxide_api::types::InstanceCreate {
            description: self.instance_name.to_owned(),
            disks: vec![],
            external_ips: vec![],
            hostname: self.instance_name.to_owned(),
            memory: oxide_api::types::ByteCount(1024 * 1024 * 1024),
            name: oxide_api::types::Name::try_from(&self.instance_name)
                .unwrap(),
            ncpus: oxide_api::types::InstanceCpuCount(1),
            network_interfaces:
                oxide_api::types::InstanceNetworkInterfaceAttachment::None,
            start: true,
            user_data: String::new(),
        };

        info!(body = ?body, "sending instance create request");
        let res = self
            .client
            .instance_create()
            .project(&self.project)
            .body(body)
            .send()
            .await;

        if res.is_err() {
            warn!(result = ?res, "instance create request returned");
        } else {
            info!(result = ?res, "instance create request returned");
        }

        unwrap_oxide_api_error(res)
    }

    /// Asks to start this actor's instance.
    async fn start_instance(&self) -> Result<(), OxideApiError> {
        info!("sending instance start request");
        let res = self
            .client
            .instance_start()
            .project(&self.project)
            .instance(&self.instance_name)
            .send()
            .await;

        if res.is_err() {
            warn!(result = ?res, "instance start request returned");
        } else {
            info!(result = ?res, "instance start request returned");
        }
        unwrap_oxide_api_error(res)
    }

    /// Asks to stop this actor's instance.
    async fn stop_instance(&self) -> Result<(), OxideApiError> {
        info!("sending instance stop request");
        let res = self
            .client
            .instance_stop()
            .project(&self.project)
            .instance(&self.instance_name)
            .send()
            .await;

        if res.is_err() {
            warn!(result = ?res, "instance stop request returned");
        } else {
            info!(result = ?res, "instance stop request returned");
        }
        unwrap_oxide_api_error(res)
    }

    /// Asks to delete this actor's instance.
    async fn delete_instance(&self) -> Result<(), OxideApiError> {
        info!("sending instance delete request");
        let res = self
            .client
            .instance_delete()
            .project(&self.project)
            .instance(&self.instance_name)
            .send()
            .await;

        if res.is_err() {
            warn!(result = ?res, "instance delete request returned");
        } else {
            info!(result = ?res, "instance delete request returned");
        }
        unwrap_oxide_api_error(res)
    }

    /// Selects an action for this antagonist to take given that its instance
    /// was observed to be in the supplied `state`.
    fn get_next_action(&self, state: InstanceState) -> anyhow::Result<Action> {
        use rand::prelude::Distribution;
        let actions = [
            Action::Wait,
            Action::Create,
            Action::Start,
            Action::Stop,
            Action::Destroy,
        ];

        let weights = match state {
            // If the instance is still starting up, favor politely waiting for
            // it to finish.
            InstanceState::Creating | InstanceState::Starting => {
                [60, 10, 10, 10, 10]
            }
            // If the instance is running or winding down, give it a mix of
            // operations that favors asking to start or stop it again.
            InstanceState::Running
            | InstanceState::Rebooting
            | InstanceState::Stopping => [35, 5, 25, 25, 10],

            // If the instance is already stopped, favor starting it again, but
            // give it a modest chance of being destroyed.
            InstanceState::Stopped => [25, 5, 40, 10, 20],

            // Raise errors for things that shouldn't happen or unrecoverable
            // conditions.
            InstanceState::Migrating => anyhow::bail!(
                "instance {} unexpectedly migrating",
                self.instance_name
            ),
            InstanceState::Repairing => anyhow::bail!(
                "instance {} unexpectedly repairing",
                self.instance_name
            ),
            InstanceState::Failed => {
                anyhow::bail!("instance {} has failed", self.instance_name)
            }
            InstanceState::Destroyed => anyhow::bail!(
                "instance {} unexpectedly destroyed",
                self.instance_name
            ),
        };

        // `new` returns an error if the iterator is empty, if any weight is <
        // 0, or if its total value is 0.
        let dist = rand::distributions::WeightedIndex::new(weights)
            .unwrap();
        let mut rng = rand::thread_rng();
        Ok(actions[dist.sample(&mut rng)])
    }
}

#[async_trait]
impl super::Antagonist for InstanceActor {
    #[tracing::instrument(level = "info", skip(self), fields(instance_name = self.instance_name))]
    async fn antagonize(&self) -> Result<(), AntagonistError> {
        trace!("querying instance state");
        let state = match self.get_instance_state().await? {
            None => {
                info!("instance doesn't exist, will try to create it");
                return self.create_instance().await.map_err(Into::into);
            }
            Some(state) => {
                trace!(?state, "got instance state");
                state
            }
        };

        sleep_random_ms(100).await;

        let action = self.get_next_action(state)?;
        trace!(?action, "selected action");
        let result = match action {
            Action::Wait => Ok(()),
            Action::Create => self.create_instance().await,
            Action::Start => self.start_instance().await,
            Action::Stop => self.stop_instance().await,
            Action::Destroy => self.delete_instance().await,
        };

        sleep_random_ms(100).await;

        result.map_err(Into::into)
    }
}
