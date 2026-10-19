//! Canonical Container associations in immutable Worker descriptors.

use super::*;

impl WorkerCodeDescriptorV1 {
    /// Bind validated immutable Container declarations into the Worker source digest.
    pub fn with_containers(
        mut self,
        mut declarations: Vec<open_compute_storage::containers::VersionContainerDeclaration>,
    ) -> Result<Self, PlatformError> {
        declarations.sort_by_key(|declaration| declaration.namespace_resource_id.to_string());
        if !declarations.is_empty()
            && self.content_kind
                != open_compute_storage::worker_repository::VersionContentKind::Worker
        {
            return Err(binding_invariant());
        }
        let mut names = BTreeSet::new();
        let mut namespaces = BTreeSet::new();
        for declaration in &declarations {
            declaration.validate()?;
            if !names.insert(&declaration.application_name)
                || !namespaces.insert(declaration.namespace_resource_id.to_string())
            {
                return Err(binding_invariant());
            }
        }
        self.container_declarations = declarations;
        Ok(self)
    }
}
