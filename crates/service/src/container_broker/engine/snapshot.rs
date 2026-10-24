//! Bounded engine primitives shared by capture and active qualification.

use super::*;
use std::path::Path;

impl Engine {
    pub(crate) async fn capture_directory(
        &self,
        id: &str,
        path: &open_compute_core::containers::ContainerDirectoryPath,
        directory: &Path,
        maximum: u64,
    ) -> Result<(tokio::fs::File, u64), PlatformError> {
        // The trailing dot captures contents rather than wrapping them in a basename directory.
        let path = format!("{}/.", path.as_str().trim_end_matches('/'));
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("path", &path)
            .finish();
        let exported = self
            .send(
                Method::GET,
                &format!("/containers/{id}/archive?{query}"),
                Vec::new(),
                false,
            )
            .await?;
        if exported.status().as_u16() != 200
            || exported.content_length().is_some_and(|size| size > maximum)
        {
            return Err(invalid());
        }
        crate::container_broker::spool::stage_rootfs(
            axum::body::Body::from_stream(exported.bytes_stream()),
            directory,
            maximum,
        )
        .await
    }

    pub(crate) async fn capture_rootfs(
        &self,
        id: &str,
        directory: &Path,
        maximum: u64,
    ) -> Result<(tokio::fs::File, u64), PlatformError> {
        let (status, _) = self
            .json(
                Method::POST,
                &format!("/containers/{id}/pause"),
                Value::Null,
            )
            .await?;
        if status != 204 {
            return Err(invalid());
        }
        let exported = self
            .send(
                Method::GET,
                &format!("/containers/{id}/export"),
                Vec::new(),
                false,
            )
            .await?;
        if exported.status().as_u16() != 200
            || exported.content_length().is_some_and(|size| size > maximum)
        {
            return Err(invalid());
        }
        let (file, size) = crate::container_broker::spool::stage_rootfs(
            axum::body::Body::from_stream(exported.bytes_stream()),
            directory,
            maximum,
        )
        .await?;
        let (status, _) = self
            .json(
                Method::POST,
                &format!("/containers/{id}/unpause"),
                Value::Null,
            )
            .await?;
        if status != 204 {
            return Err(invalid());
        }
        Ok((file, size))
    }
}
