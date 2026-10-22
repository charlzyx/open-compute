//! Numeric exec identity and scheduling-policy capability behavior through the public native API.

use super::{http, tenant};
use serde_json::json;

pub(super) async fn lifecycle(
    client: &http::Client,
    public: &str,
    host: &str,
    durable_object: bool,
) {
    let path = if durable_object {
        "/named/exec-users?object=exec-options"
    } else {
        "/exec-users?object=exec-lifecycle"
    };
    assert_eq!(
        tenant(client, public, host, path).await,
        json!({
            "imageUserPreserved": true,
            "numericPairPreserved": true,
            "bareUidRoot": true,
            "namedUsersRejected": true,
            "nonRootCapabilities": durable_object,
            "mainCapabilities": durable_object,
            "rootBoundPreserved": true,
        })
    );
}
