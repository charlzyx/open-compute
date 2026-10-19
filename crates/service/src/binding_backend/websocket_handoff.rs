//! HTTP body ownership for `ServiceWebSocketLease` handoffs.

use super::*;
use crate::service_invocations::ServiceInvocationRegistry;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HandoffRequest {
    handles: Vec<String>,
}

pub(super) fn response(
    registry: &ServiceInvocationRegistry,
    bytes: &[u8],
) -> Result<Response, PlatformError> {
    let request = serde_json::from_slice::<HandoffRequest>(bytes).map_err(|_| protocol_error())?;
    let lease = registry.activate_websocket_handoffs(&request.handles)?;
    // The caller owns this silent body; cancellation or runtime exit drops the existing lease.
    let stream = futures::stream::unfold(lease, |lease| async move {
        std::future::pending::<()>().await;
        Some((
            Ok::<_, std::convert::Infallible>(axum::body::Bytes::new()),
            lease,
        ))
    });
    Ok((
        [(header::CONTENT_TYPE, "application/octet-stream")],
        Body::from_stream(stream),
    )
        .into_response())
}
