//! Explicit native signal and monitor lifecycle through an isolated tenant object.

use super::{http, tenant};
use serde_json::json;

pub(super) async fn lifecycle(client: &http::Client, public: &str, host: &str) {
    assert_eq!(
        tenant(
            client,
            public,
            host,
            "/named/signals-lifecycle?object=signals-lifecycle",
        )
        .await,
        json!({
            "mainSignalExitCode": 23,
            "mainRealtimeSignalExitCode": 23,
            "monitorZero": true,
            "monitorNonzeroExitCode": 7,
            "destroyReasonIdentity": true,
            "execKillExitCode": 143,
            "execKillStdout": "ready\n",
            "execKillStderr": "",
        })
    );
}
