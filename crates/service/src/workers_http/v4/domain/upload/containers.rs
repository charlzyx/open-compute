//! Admit immutable Container associations from Worker upload metadata.

use super::*;
use crate::workers_http::v4::do_lifecycle;

impl UploadInput {
    pub(in crate::workers_http::v4) fn apply_containers(
        &mut self,
        api: &WorkerApiState,
        instance: InstanceId,
        worker: WorkerId,
        migration_tag: Option<&str>,
    ) -> Result<(), PlatformError> {
        do_lifecycle::validate_containers(&self.metadata)?;
        if self.metadata.containers.is_empty() {
            return Ok(());
        }
        let namespaces = DurableObjectRepository::new(&api.storage);
        self.runtime_features.containers = self
            .metadata
            .containers
            .iter()
            .map(|container| {
                let namespace = namespaces.namespace_for_worker_upload(
                    instance,
                    worker,
                    &container.class_name,
                    migration_tag,
                )?;
                let authority =
                    open_compute_storage::containers::ContainerAuthority::new(api.storage.db());
                for reference in container.images.values() {
                    let digest =
                        open_compute_storage::containers::registry_image_digest(reference)?;
                    authority.ordinary_image(instance, &digest)?;
                }
                open_compute_storage::containers::VersionContainerDeclaration::with_images(
                    namespace.resource.id,
                    container.name.clone(),
                    container.images.clone(),
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "containers_tests.rs"]
mod tests;
