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

/// Given a response value from an Oxide API call, returns:
///
/// - `Ok` if the call succeeded.
/// - `Ok` if the call failed but produced an error response value, irrespective
///   of the type of error response.
/// - `Err` if the call failed without producing an error response value, e.g.
///   because the connection to Nexus was interrupted or because a malformed
///   response was received.
pub fn fail_if_no_response<T, U>(
    result: core::result::Result<
        oxide_api::ResponseValue<T>,
        oxide_api::Error<U>,
    >,
) -> core::result::Result<(), oxide_api::Error<U>>
where
    U: std::fmt::Debug + Send + Sync,
{
    match result {
        Ok(_) => Ok(()),
        Err(e) => match e {
            oxide_api::Error::ErrorResponse(_) => Ok(()),
            _ => Err(e),
        },
    }
}
