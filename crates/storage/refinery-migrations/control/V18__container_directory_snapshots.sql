-- A directory snapshot remains private to its application and distinct from a boot image.
ALTER TABLE container_snapshots ADD COLUMN directory_path TEXT
  CHECK(directory_path IS NULL OR (
    length(CAST(directory_path AS BLOB)) BETWEEN 1 AND 4096
    AND substr(directory_path,1,1)='/' AND instr(directory_path,char(0))=0
  ));
CREATE TRIGGER container_snapshot_directory_identity_guard BEFORE UPDATE OF directory_path ON container_snapshots
WHEN NEW.directory_path IS NOT OLD.directory_path
BEGIN SELECT RAISE(ABORT, 'immutable Container snapshot directory'); END;
CREATE TRIGGER container_directory_snapshot_boot_guard BEFORE INSERT ON container_instance_leases
WHEN NEW.snapshot_id IS NOT NULL AND EXISTS (
  SELECT 1 FROM container_snapshots WHERE id=NEW.snapshot_id AND directory_path IS NOT NULL
)
BEGIN SELECT RAISE(ABORT, 'directory snapshot is not a Container boot image'); END;

-- Existing executions have no directory restores; new plans are immutable lease authority.
ALTER TABLE container_instance_leases ADD COLUMN directory_restores_json BLOB NOT NULL DEFAULT X'5B5D'
  CHECK(json_valid(directory_restores_json) AND json_type(directory_restores_json)='array');
CREATE TRIGGER container_directory_restore_identity_guard BEFORE UPDATE ON container_instance_leases
WHEN NEW.directory_restores_json IS NOT OLD.directory_restores_json
BEGIN SELECT RAISE(ABORT, 'immutable Container directory restore'); END;
CREATE TRIGGER container_directory_restore_admission_guard BEFORE INSERT ON container_instance_leases
BEGIN
  SELECT CASE WHEN json_array_length(NEW.directory_restores_json)>0 AND NOT EXISTS (
    SELECT 1 FROM container_applications a WHERE a.id=NEW.application_id
      AND a.deletion_requested_at_ms IS NULL AND a.deleted_at_ms IS NULL
  ) THEN RAISE(ABORT, 'invalid Container directory restore application') END;
  SELECT CASE WHEN EXISTS (
    SELECT 1 FROM json_each(NEW.directory_restores_json) d
    WHERE json_type(d.value)!='object' OR json_type(d.value,'$.mountPoint') IS NOT 'text'
      OR length(CAST(json_extract(d.value,'$.mountPoint') AS BLOB)) NOT BETWEEN 2 AND 4096
      OR substr(json_extract(d.value,'$.mountPoint'),1,1)!='/'
      OR instr(json_extract(d.value,'$.mountPoint'),char(0))!=0
      OR json_type(d.value,'$.snapshotId') IS NULL
      OR (json_type(d.value,'$.snapshotId')!='null' AND NOT EXISTS (
        SELECT 1 FROM container_snapshots s WHERE s.id=json_extract(d.value,'$.snapshotId')
          AND s.application_id=NEW.application_id AND s.state='ready'
          AND s.directory_path IS NOT NULL AND s.expires_at_ms>NEW.created_at_ms
      ))
  ) THEN RAISE(ABORT, 'invalid Container directory restore plan') END;
  SELECT CASE WHEN (SELECT COUNT(*)!=COUNT(DISTINCT json_extract(value,'$.mountPoint'))
    FROM json_each(NEW.directory_restores_json))
    THEN RAISE(ABORT, 'duplicate Container directory restore destination') END;
END;
