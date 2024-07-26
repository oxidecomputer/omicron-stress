use rand::Rng;
use tracing::trace;

/// Sleeps for [0..max_millis] milliseconds.
pub async fn sleep_random_ms(max_millis: u64) {
    let duration = {
        let mut rng = rand::thread_rng();
        std::time::Duration::from_millis(rng.gen_range(0..=max_millis))
    };

    trace!(?duration, "taking a nap");
    tokio::time::sleep(duration).await;
}

pub type OxideApiError = oxide::Error<oxide::types::Error>;

pub fn unwrap_oxide_api_error<T>(
    result: core::result::Result<oxide::ResponseValue<T>, OxideApiError>,
) -> core::result::Result<(), OxideApiError> {
    result.map(|_| ())
}

/// Given an error response from an Oxide API call, returns:
///
/// - `Ok` if the call failed but produced an error response value, irrespective
///   of the type of error response.
/// - `Err` if the call failed without producing an error response value, e.g.
///   because the connection to Nexus was interrupted or because a malformed
///   response was received.
pub fn fail_if_no_response<U>(
    e: oxide::Error<U>,
) -> core::result::Result<(), oxide::Error<U>>
where
    U: std::fmt::Debug + Send + Sync,
{
    match e {
        oxide::Error::ErrorResponse(_) => Ok(()),
        _ => Err(e),
    }
}

/// Given an error response from an Oxide API call, returns:
///
/// - `Err` if the call failed but produced an error response value, if it is a
///   500 error.
///
/// - `Err` if the call failed without producing an error response value, e.g.
///   because the connection to Nexus was interrupted or because a malformed
///   or unexpected response was received.
///
/// - `Ok` otherwise
pub fn fail_if_500<U>(
    e: oxide::Error<U>,
) -> core::result::Result<(), oxide::Error<U>>
where
    U: std::fmt::Debug + Send + Sync,
{
    match &e {
        oxide::Error::ErrorResponse(r) => match r.status() {
            // The call returned an error response
            reqwest::StatusCode::INTERNAL_SERVER_ERROR => Err(e),

            _ => Ok(()),
        },

        // There was a communication error
        oxide::Error::CommunicationError(_)
        // or there was an error reading the response
        | oxide::Error::ResponseBodyError(_)
        // or deserialization failed
        | oxide::Error::InvalidResponsePayload(_, _)
        // or an unexpected response was received
        | oxide::Error::UnexpectedResponse(_)
        // or there was an error upgrading the connection
        | oxide::Error::InvalidUpgrade(_)
        // or there was an error processing a request pre-hook
        | oxide::Error::PreHookError(_) => Err(e),

        // The request was invalid
        oxide::Error::InvalidRequest(_) => Ok(()),
    }
}
