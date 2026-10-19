-- Container images and targets are immutable; provider caches are not authority.
CREATE TABLE container_images (
  manifest_sha256 BLOB PRIMARY KEY CHECK(length(manifest_sha256) = 32),
  config_sha256 BLOB NOT NULL CHECK(length(config_sha256) = 32),
  diff_ids BLOB NOT NULL CHECK(length(diff_ids) <= 4096 AND length(diff_ids) % 32 = 0),
  architecture TEXT NOT NULL CHECK(architecture IN ('amd64', 'arm64')),
  compressed_bytes INTEGER NOT NULL CHECK(compressed_bytes > 0),
  unpacked_bytes INTEGER NOT NULL CHECK(unpacked_bytes > 0),
  artifact_sha256 BLOB NOT NULL CHECK(length(artifact_sha256) = 32),
  artifact_size INTEGER NOT NULL CHECK(artifact_size > 0),
  created_at_ms INTEGER NOT NULL
) WITHOUT ROWID, STRICT;
CREATE TRIGGER container_images_immutable BEFORE UPDATE ON container_images
BEGIN SELECT RAISE(ABORT, 'immutable Container image'); END;

CREATE TABLE container_applications (
  id TEXT PRIMARY KEY,
  namespace_resource_id TEXT NOT NULL REFERENCES do_namespaces(resource_id),
  name TEXT NOT NULL CHECK(length(name) BETWEEN 1 AND 63),
  max_instances INTEGER NOT NULL CHECK(max_instances > 0),
  scheduling_policy TEXT NOT NULL CHECK(scheduling_policy IN ('default','durable_object')),
  target_generation INTEGER CHECK(target_generation > 0),
  admission_paused INTEGER NOT NULL DEFAULT 1 CHECK(admission_paused IN (0, 1)),
  created_at_ms INTEGER NOT NULL,
  deletion_requested_at_ms INTEGER,
  deleted_at_ms INTEGER,
  CHECK(deleted_at_ms IS NULL OR (deletion_requested_at_ms IS NOT NULL AND admission_paused=1)),
  FOREIGN KEY(id, target_generation) REFERENCES container_application_targets(application_id, generation)
    DEFERRABLE INITIALLY DEFERRED
) STRICT;
CREATE UNIQUE INDEX container_active_application_namespace ON container_applications(namespace_resource_id) WHERE deleted_at_ms IS NULL;
CREATE UNIQUE INDEX container_active_application_name ON container_applications(name) WHERE deleted_at_ms IS NULL;
CREATE TABLE container_application_targets (
  application_id TEXT NOT NULL REFERENCES container_applications(id),
  generation INTEGER NOT NULL CHECK(generation > 0),
  worker_version_id TEXT NOT NULL REFERENCES worker_versions(id),
  image_sha256 BLOB REFERENCES container_images(manifest_sha256) CHECK(image_sha256 IS NULL OR length(image_sha256)=32),
  provider_contract_sha256 BLOB NOT NULL CHECK(length(provider_contract_sha256) = 32),
  resources_json BLOB NOT NULL,
  enable_internet INTEGER NOT NULL CHECK(enable_internet IN (0, 1)),
  created_at_ms INTEGER NOT NULL,
  PRIMARY KEY(application_id, generation)
) WITHOUT ROWID, STRICT;
CREATE TRIGGER container_targets_immutable BEFORE UPDATE ON container_application_targets
BEGIN SELECT RAISE(ABORT, 'immutable Container target'); END;
CREATE TRIGGER container_target_referrer_insert AFTER INSERT ON container_application_targets
BEGIN
  INSERT INTO version_referrers(version_id,kind,ref_id,created_at_ms)
  VALUES (NEW.worker_version_id,'container_target',NEW.application_id || ':' || NEW.generation,NEW.created_at_ms);
END;
CREATE TRIGGER container_target_referrer_delete AFTER DELETE ON container_application_targets
BEGIN
  DELETE FROM version_referrers WHERE version_id=OLD.worker_version_id AND kind='container_target'
    AND ref_id=OLD.application_id || ':' || OLD.generation;
END;
CREATE TRIGGER container_target_owner_guard BEFORE INSERT ON container_application_targets
BEGIN
  SELECT CASE WHEN NOT EXISTS (
    SELECT 1 FROM container_applications a
    JOIN do_namespaces n ON n.resource_id = a.namespace_resource_id
    JOIN worker_versions v ON v.worker_id = n.owner_worker_id
    WHERE a.id = NEW.application_id AND v.id = NEW.worker_version_id
      AND v.state IN ('validating', 'ready') AND v.deleted_at_ms IS NULL
  ) THEN RAISE(ABORT, 'Container target Worker authority mismatch') END;
END;
CREATE TRIGGER container_application_identity_guard BEFORE UPDATE ON container_applications
WHEN NEW.id != OLD.id OR NEW.namespace_resource_id != OLD.namespace_resource_id
  OR NEW.scheduling_policy != OLD.scheduling_policy
  OR NEW.created_at_ms != OLD.created_at_ms OR NEW.target_generation < OLD.target_generation
  OR (OLD.target_generation IS NOT NULL AND NEW.target_generation IS NULL)
BEGIN SELECT RAISE(ABORT, 'immutable Container application identity'); END;

CREATE TABLE version_containers (
  version_id TEXT NOT NULL REFERENCES worker_versions(id),
  namespace_resource_id TEXT NOT NULL REFERENCES do_namespaces(resource_id),
  application_name TEXT NOT NULL CHECK(length(application_name) BETWEEN 1 AND 63),
  descriptor_sha256 BLOB NOT NULL CHECK(length(descriptor_sha256) = 32),
  prepared_migration_tag TEXT CHECK(prepared_migration_tag IS NULL OR length(prepared_migration_tag) BETWEEN 1 AND 128),
  PRIMARY KEY(version_id, namespace_resource_id),
  UNIQUE(version_id, application_name)
) WITHOUT ROWID, STRICT;
CREATE TRIGGER version_container_insert_guard BEFORE INSERT ON version_containers
BEGIN
  SELECT CASE WHEN NOT EXISTS (
    SELECT 1 FROM worker_versions v
    JOIN do_namespaces n ON n.owner_worker_id = v.worker_id
    JOIN resources r ON r.id = n.resource_id AND r.state = 'ready'
    WHERE v.id = NEW.version_id AND v.state = 'staging'
      AND n.resource_id = NEW.namespace_resource_id
      AND ((n.lifecycle_state = 'active' AND NEW.prepared_migration_tag IS NULL) OR
           (n.lifecycle_state = 'pending' AND NEW.prepared_migration_tag = n.migration_tag))
  ) THEN RAISE(ABORT, 'Container version authority mismatch') END;
END;
CREATE TRIGGER version_container_update_guard BEFORE UPDATE ON version_containers
BEGIN SELECT RAISE(ABORT, 'immutable Container version descriptor'); END;
CREATE TRIGGER version_container_delete_guard BEFORE DELETE ON version_containers
WHEN (SELECT state FROM worker_versions WHERE id = OLD.version_id) NOT IN ('staging', 'rejected', 'deleting')
BEGIN SELECT RAISE(ABORT, 'immutable Container version descriptor'); END;

CREATE TABLE container_instance_leases (
  application_id TEXT NOT NULL,
  namespace_resource_id TEXT NOT NULL,
  object_id TEXT NOT NULL,
  object_generation INTEGER NOT NULL,
  lease_generation INTEGER NOT NULL CHECK(lease_generation > 0),
  target_generation INTEGER NOT NULL,
  effective_image_sha256 BLOB NOT NULL REFERENCES container_images(manifest_sha256) CHECK(length(effective_image_sha256)=32),
  effective_resources_json BLOB NOT NULL,
  enable_internet INTEGER NOT NULL CHECK(enable_internet IN (0,1)),
  startup_json BLOB NOT NULL,
  startup_envelope_json BLOB NOT NULL CHECK(length(startup_envelope_json) BETWEEN 1 AND 131072),
  opaque_engine_key TEXT NOT NULL UNIQUE CHECK(length(opaque_engine_key) = 64
    AND opaque_engine_key NOT GLOB '*[^0-9a-f]*'),
  start_identity TEXT CHECK(start_identity IS NULL OR (length(start_identity) = 64
    AND start_identity NOT GLOB '*[^0-9a-f]*')),
  state TEXT NOT NULL CHECK(state IN ('admitting', 'starting', 'running', 'stopping', 'stopped', 'lost')),
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  stopped_at_ms INTEGER,
  stop_requested_at_ms INTEGER,
  PRIMARY KEY(namespace_resource_id, object_id, object_generation, lease_generation),
  FOREIGN KEY(namespace_resource_id, object_id, object_generation)
    REFERENCES do_objects(namespace_resource_id, object_id, generation),
  FOREIGN KEY(application_id, target_generation)
    REFERENCES container_application_targets(application_id, generation),
  CHECK((state = 'stopped') = (stopped_at_ms IS NOT NULL)),
  CHECK(state != 'running' OR start_identity IS NOT NULL),
  CHECK(state != 'stopping' OR stop_requested_at_ms IS NOT NULL)
) WITHOUT ROWID, STRICT;
CREATE UNIQUE INDEX container_live_object_identity
ON container_instance_leases(namespace_resource_id, object_id)
WHERE state != 'stopped';
CREATE INDEX container_lease_capacity ON container_instance_leases(application_id, state);
CREATE TRIGGER container_lease_owner_guard BEFORE INSERT ON container_instance_leases
BEGIN
  SELECT CASE WHEN NOT EXISTS (
    SELECT 1 FROM container_applications a
    JOIN do_objects o ON o.namespace_resource_id = a.namespace_resource_id
    WHERE a.id = NEW.application_id AND a.namespace_resource_id = NEW.namespace_resource_id
      AND o.object_id = NEW.object_id AND o.generation = NEW.object_generation
      AND o.state IN ('creating', 'ready')
  ) THEN RAISE(ABORT, 'Container lease object authority mismatch') END;
END;
CREATE TRIGGER container_lease_identity_guard BEFORE UPDATE ON container_instance_leases
WHEN NEW.application_id != OLD.application_id OR NEW.namespace_resource_id != OLD.namespace_resource_id
  OR NEW.object_id != OLD.object_id OR NEW.object_generation != OLD.object_generation
  OR NEW.lease_generation != OLD.lease_generation OR NEW.target_generation != OLD.target_generation
  OR NEW.effective_image_sha256 != OLD.effective_image_sha256
  OR NEW.effective_resources_json != OLD.effective_resources_json OR NEW.enable_internet != OLD.enable_internet
  OR NEW.startup_json != OLD.startup_json OR NEW.startup_envelope_json IS NOT OLD.startup_envelope_json
  OR NEW.opaque_engine_key != OLD.opaque_engine_key OR NEW.created_at_ms != OLD.created_at_ms
  OR (OLD.start_identity IS NOT NULL AND NEW.start_identity IS NOT OLD.start_identity)
  OR (OLD.stop_requested_at_ms IS NOT NULL AND NEW.stop_requested_at_ms IS NOT OLD.stop_requested_at_ms)
  OR OLD.state = 'stopped'
BEGIN SELECT RAISE(ABORT, 'immutable Container lease identity'); END;

-- Public desired configuration is separate from each running execution's target.
CREATE TABLE container_application_settings (
  application_id TEXT PRIMARY KEY REFERENCES container_applications(id),
  configuration_json BLOB,
  observability_logs INTEGER NOT NULL CHECK(observability_logs IN (0,1)),
  updated_at_ms INTEGER NOT NULL
) WITHOUT ROWID, STRICT;
CREATE TABLE container_rollouts (
  id TEXT PRIMARY KEY,
  application_id TEXT NOT NULL,
  target_generation INTEGER NOT NULL,
  steps_json BLOB NOT NULL,
  current_generation INTEGER NOT NULL,
  details_json BLOB NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('running','completed','failed','cancelled')),
  current_step INTEGER NOT NULL CHECK(current_step >= 0),
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  FOREIGN KEY(application_id,target_generation) REFERENCES container_application_targets(application_id,generation)
) STRICT;
CREATE UNIQUE INDEX container_one_active_rollout ON container_rollouts(application_id) WHERE state='running';
CREATE TABLE container_rollout_members (
  rollout_id TEXT NOT NULL REFERENCES container_rollouts(id),
  namespace_resource_id TEXT NOT NULL,
  object_id TEXT NOT NULL,
  object_generation INTEGER NOT NULL,
  lease_generation INTEGER NOT NULL,
  PRIMARY KEY(rollout_id,namespace_resource_id,object_id,object_generation,lease_generation),
  FOREIGN KEY(namespace_resource_id,object_id,object_generation,lease_generation)
    REFERENCES container_instance_leases(namespace_resource_id,object_id,object_generation,lease_generation)
) WITHOUT ROWID, STRICT;
CREATE TRIGGER container_rollout_identity_guard BEFORE UPDATE ON container_rollouts
WHEN NEW.id != OLD.id OR NEW.application_id != OLD.application_id
  OR NEW.target_generation != OLD.target_generation OR NEW.steps_json != OLD.steps_json
  OR NEW.current_generation != OLD.current_generation OR NEW.details_json != OLD.details_json
  OR NEW.created_at_ms != OLD.created_at_ms OR NEW.current_step < OLD.current_step
  OR OLD.state != 'running'
BEGIN SELECT RAISE(ABORT,'immutable Container rollout identity'); END;

-- Short-lived registry credentials store only domain-separated password hashes.
CREATE TABLE container_registry_tokens (
  token_sha256 BLOB PRIMARY KEY CHECK(length(token_sha256)=32),
  repository_scope TEXT NOT NULL, can_push INTEGER NOT NULL CHECK(can_push IN (0,1)),
  can_pull INTEGER NOT NULL CHECK(can_pull IN (0,1)), expires_at_ms INTEGER NOT NULL
) WITHOUT ROWID, STRICT;
CREATE TABLE container_registry_uploads (
  id TEXT PRIMARY KEY, repository TEXT NOT NULL, size INTEGER NOT NULL CHECK(size>=0),
  busy INTEGER NOT NULL CHECK(busy IN (0,1)), expires_at_ms INTEGER NOT NULL
) WITHOUT ROWID, STRICT;
CREATE TABLE container_registry_blobs (
  repository TEXT NOT NULL, artifact_sha256 BLOB NOT NULL CHECK(length(artifact_sha256)=32),
  artifact_size INTEGER NOT NULL CHECK(artifact_size>0), created_at_ms INTEGER NOT NULL, expires_at_ms INTEGER NOT NULL,
  PRIMARY KEY(repository,artifact_sha256)
) WITHOUT ROWID, STRICT;
CREATE TABLE container_registry_tags (
  repository TEXT NOT NULL, tag TEXT NOT NULL, manifest_sha256 BLOB NOT NULL REFERENCES container_images(manifest_sha256),
  updated_at_ms INTEGER NOT NULL, PRIMARY KEY(repository,tag)
) WITHOUT ROWID, STRICT;
CREATE TABLE container_registry_manifests (
  repository TEXT NOT NULL, manifest_sha256 BLOB NOT NULL REFERENCES container_images(manifest_sha256),
  content BLOB NOT NULL CHECK(length(content)>0 AND length(content)<=1048576),
  PRIMARY KEY(repository,manifest_sha256)
) WITHOUT ROWID, STRICT;
CREATE TRIGGER container_registry_manifest_immutable BEFORE UPDATE ON container_registry_manifests
BEGIN SELECT RAISE(ABORT,'immutable Container registry manifest'); END;
CREATE TABLE container_target_configuration (
  application_id TEXT NOT NULL, target_generation INTEGER NOT NULL,
  configuration_json BLOB CHECK(configuration_json IS NULL OR (length(configuration_json)>0 AND length(configuration_json)<=16384)),
  PRIMARY KEY(application_id,target_generation),
  FOREIGN KEY(application_id,target_generation) REFERENCES container_application_targets(application_id,generation)
) WITHOUT ROWID, STRICT;
CREATE TRIGGER container_target_configuration_immutable BEFORE UPDATE ON container_target_configuration
BEGIN SELECT RAISE(ABORT,'immutable Container target configuration'); END;


CREATE TABLE version_container_images (
  version_id TEXT NOT NULL,
  namespace_resource_id TEXT NOT NULL,
  image_name TEXT NOT NULL,
  image_reference TEXT NOT NULL,
  image_sha256 BLOB NOT NULL REFERENCES container_images(manifest_sha256) CHECK(length(image_sha256)=32),
  PRIMARY KEY(version_id,namespace_resource_id,image_name),
  FOREIGN KEY(version_id,namespace_resource_id) REFERENCES version_containers(version_id,namespace_resource_id) ON DELETE CASCADE
) WITHOUT ROWID, STRICT;
CREATE TRIGGER version_container_image_insert_guard BEFORE INSERT ON version_container_images
WHEN (SELECT state FROM worker_versions WHERE id=NEW.version_id) != 'staging'
BEGIN SELECT RAISE(ABORT, 'Container images require a staging Version'); END;
CREATE TRIGGER version_container_image_update_guard BEFORE UPDATE ON version_container_images
BEGIN SELECT RAISE(ABORT, 'immutable Container named image'); END;
CREATE TRIGGER version_container_image_delete_guard BEFORE DELETE ON version_container_images
WHEN (SELECT state FROM worker_versions WHERE id=OLD.version_id) NOT IN ('staging','rejected','deleting')
BEGIN SELECT RAISE(ABORT, 'immutable Container named image'); END;
CREATE TRIGGER container_target_image_mode_guard BEFORE INSERT ON container_application_targets
WHEN (SELECT scheduling_policy FROM container_applications WHERE id=NEW.application_id) = 'default' AND NEW.image_sha256 IS NULL
  OR (SELECT scheduling_policy FROM container_applications WHERE id=NEW.application_id) = 'durable_object' AND NEW.image_sha256 IS NOT NULL
BEGIN SELECT RAISE(ABORT, 'Container target scheduling mismatch'); END;

CREATE TRIGGER container_settings_mode_guard BEFORE INSERT ON container_application_settings
WHEN ((SELECT scheduling_policy FROM container_applications WHERE id=NEW.application_id)='default') != (NEW.configuration_json IS NOT NULL)
BEGIN SELECT RAISE(ABORT, 'Container configuration scheduling mismatch'); END;
CREATE TRIGGER container_settings_update_mode_guard BEFORE UPDATE ON container_application_settings
WHEN ((SELECT scheduling_policy FROM container_applications WHERE id=NEW.application_id)='default') != (NEW.configuration_json IS NOT NULL)
BEGIN SELECT RAISE(ABORT, 'Container configuration scheduling mismatch'); END;
CREATE TRIGGER container_target_configuration_mode_guard BEFORE INSERT ON container_target_configuration
WHEN ((SELECT scheduling_policy FROM container_applications WHERE id=NEW.application_id)='default') != (NEW.configuration_json IS NOT NULL)
BEGIN SELECT RAISE(ABORT, 'Container configuration scheduling mismatch'); END;
