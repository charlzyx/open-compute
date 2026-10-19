-- Snapshot artifacts have application ACLs; ordinary images cannot grant them.
ALTER TABLE container_images ADD COLUMN snapshot_only INTEGER NOT NULL DEFAULT 0 CHECK(snapshot_only IN (0,1));
ALTER TABLE container_instance_leases ADD COLUMN snapshot_id TEXT;
CREATE TRIGGER container_lease_snapshot_identity_guard BEFORE UPDATE ON container_instance_leases
WHEN NEW.snapshot_id IS NOT OLD.snapshot_id
BEGIN SELECT RAISE(ABORT, 'immutable Container restore identity'); END;
CREATE TABLE container_snapshots (
  id TEXT PRIMARY KEY,
  instance_id TEXT NOT NULL REFERENCES instance_identity(instance_id),
  application_id TEXT NOT NULL REFERENCES container_applications(id),
  namespace_resource_id TEXT NOT NULL,
  object_id TEXT NOT NULL,
  object_generation INTEGER NOT NULL CHECK(object_generation>0),
  lease_generation INTEGER NOT NULL CHECK(lease_generation>0),
  opaque_engine_key TEXT NOT NULL CHECK(length(opaque_engine_key)=64 AND opaque_engine_key NOT GLOB '*[^0-9a-f]*'),
  start_identity TEXT NOT NULL CHECK(length(start_identity)=64 AND start_identity NOT GLOB '*[^0-9a-f]*'),
  source_image_sha256 BLOB NOT NULL REFERENCES container_images(manifest_sha256),
  image_sha256 BLOB REFERENCES container_images(manifest_sha256),
  artifact_sha256 BLOB CHECK(artifact_sha256 IS NULL OR length(artifact_sha256)=32),
  artifact_size INTEGER CHECK(artifact_size IS NULL OR artifact_size>0),
  image_size INTEGER CHECK(image_size IS NULL OR image_size>=0),
  name TEXT CHECK(name IS NULL OR length(name)<=256),
  state TEXT NOT NULL CHECK(state IN ('pending','ready','retiring')),
  reserved_bytes INTEGER NOT NULL CHECK(reserved_bytes>0),
  created_at_ms INTEGER NOT NULL,
  expires_at_ms INTEGER NOT NULL,
  last_restored_at_ms INTEGER,
  captured_at_ms INTEGER,
  put_attempted INTEGER NOT NULL DEFAULT 0 CHECK(put_attempted IN (0,1)),
  CHECK(expires_at_ms>=created_at_ms),
  CHECK((image_sha256 IS NULL)=(artifact_sha256 IS NULL)),
  CHECK((image_sha256 IS NULL)=(artifact_size IS NULL)),
  CHECK(artifact_size IS NULL OR artifact_size<=reserved_bytes),
  CHECK(put_attempted=0 OR image_sha256 IS NOT NULL),
  CHECK((image_size IS NULL)=(captured_at_ms IS NULL)),
  CHECK(state!='ready' OR (image_sha256 IS NOT NULL AND captured_at_ms IS NOT NULL AND image_size IS NOT NULL))
) WITHOUT ROWID, STRICT;
CREATE INDEX container_snapshot_expiry ON container_snapshots(state,expires_at_ms);
CREATE INDEX container_restore_image_reference ON container_instance_leases(snapshot_id,state);
CREATE TRIGGER container_snapshot_identity_guard BEFORE UPDATE ON container_snapshots
WHEN NEW.id!=OLD.id OR NEW.instance_id!=OLD.instance_id OR NEW.application_id!=OLD.application_id
  OR NEW.namespace_resource_id!=OLD.namespace_resource_id OR NEW.object_id!=OLD.object_id
  OR NEW.object_generation!=OLD.object_generation OR NEW.lease_generation!=OLD.lease_generation
  OR NEW.opaque_engine_key!=OLD.opaque_engine_key OR NEW.start_identity!=OLD.start_identity
  OR NEW.source_image_sha256!=OLD.source_image_sha256 OR NEW.name IS NOT OLD.name
  OR NEW.reserved_bytes!=OLD.reserved_bytes OR NEW.created_at_ms!=OLD.created_at_ms
  OR (OLD.image_sha256 IS NOT NULL AND (NEW.image_sha256 IS NOT OLD.image_sha256
    OR NEW.artifact_sha256 IS NOT OLD.artifact_sha256 OR NEW.artifact_size IS NOT OLD.artifact_size))
  OR (NEW.image_size IS NOT OLD.image_size AND NOT
    (OLD.state='pending' AND NEW.state='ready' AND OLD.image_size IS NULL AND NEW.image_size IS NOT NULL))
  OR NEW.put_attempted<OLD.put_attempted
  OR (NEW.put_attempted!=OLD.put_attempted AND OLD.state!='pending')
  OR (OLD.state='retiring' AND NEW.state!='retiring')
  OR (OLD.state='ready' AND NEW.state='pending')
  OR (OLD.captured_at_ms IS NOT NULL AND NEW.captured_at_ms IS NOT OLD.captured_at_ms)
  OR NEW.expires_at_ms<OLD.expires_at_ms
BEGIN SELECT RAISE(ABORT, 'immutable Container snapshot authority'); END;
CREATE TRIGGER container_snapshot_image_target_guard BEFORE INSERT ON container_application_targets
WHEN (SELECT snapshot_only FROM container_images WHERE manifest_sha256=NEW.image_sha256)!=0
BEGIN SELECT RAISE(ABORT, 'snapshot image requires snapshot authority'); END;
CREATE TRIGGER container_snapshot_image_registry_guard BEFORE INSERT ON container_registry_manifests
WHEN (SELECT snapshot_only FROM container_images WHERE manifest_sha256=NEW.manifest_sha256)!=0
BEGIN SELECT RAISE(ABORT, 'snapshot image is private'); END;
CREATE TRIGGER container_snapshot_tag_insert_guard BEFORE INSERT ON container_registry_tags
WHEN (SELECT snapshot_only FROM container_images WHERE manifest_sha256=NEW.manifest_sha256)!=0
BEGIN SELECT RAISE(ABORT, 'snapshot image is private'); END;
CREATE TRIGGER container_snapshot_tag_update_guard BEFORE UPDATE ON container_registry_tags
WHEN (SELECT snapshot_only FROM container_images WHERE manifest_sha256=NEW.manifest_sha256)!=0
BEGIN SELECT RAISE(ABORT, 'snapshot image is private'); END;
CREATE TRIGGER container_restore_admission_guard BEFORE INSERT ON container_instance_leases
BEGIN
  SELECT CASE WHEN NEW.snapshot_id IS NULL AND
    (SELECT snapshot_only FROM container_images WHERE manifest_sha256=NEW.effective_image_sha256)!=0
    THEN RAISE(ABORT, 'snapshot image requires restore authority') END;
  SELECT CASE WHEN NEW.snapshot_id IS NOT NULL AND NOT EXISTS (
    SELECT 1 FROM container_snapshots s JOIN container_applications a ON a.id=s.application_id
    WHERE s.id=NEW.snapshot_id AND s.application_id=NEW.application_id AND s.state='ready'
      AND s.image_sha256=NEW.effective_image_sha256 AND s.expires_at_ms>NEW.created_at_ms
      AND a.deletion_requested_at_ms IS NULL AND a.deleted_at_ms IS NULL
  ) THEN RAISE(ABORT, 'invalid Container snapshot restore') END;
END;
