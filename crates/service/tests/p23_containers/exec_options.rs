//! Real exec stream options through a separate already-running tenant object.

use super::{http, tenant};
use serde_json::json;

pub(super) async fn lifecycle(client: &http::Client, public: &str, host: &str) {
    assert_eq!(
        tenant(
            client,
            public,
            host,
            "/named/exec-options?object=exec-options",
        )
        .await,
        json!({
            "stdin": {"stdout": "stdin-eof:first:second", "stderr": "stdin-err", "exitCode": 0},
            "stdoutIgnored": {"absent": true, "stdoutBytes": 0, "stderr": "kept-err", "exitCode": 0},
            "stderrIgnored": {"absent": true, "stdout": "kept-out", "stderrBytes": 0, "exitCode": 0},
            "combined": {
                "isPty": false,
                "stderrAbsent": true,
                "stdoutLines": ["combined-err", "combined-out"],
                "stderrBytes": 0,
                "exitCode": 0,
            },
            "secondOutputName": "TypeError",
        })
    );
}
