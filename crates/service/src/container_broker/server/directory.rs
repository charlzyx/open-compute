//! Stage one exact frozen directory archive without granting a general Engine upload.

use super::*;
use std::io::{Seek as _, SeekFrom};

pub(super) struct Archives {
    pub(super) file: Option<tokio::fs::File>,
    pub(super) offsets: Vec<Option<u64>>,
}

impl Archives {
    pub(super) fn new(count: usize) -> Self {
        Self {
            file: None,
            offsets: vec![None; count],
        }
    }

    pub(super) fn clear(&mut self) {
        self.file = None;
        self.offsets.fill(None);
    }
}

pub(super) async fn verify_volumes(
    state: &BrokerState,
    lease: &BrokerLease,
) -> Result<(), PlatformError> {
    for position in 0..lease.directory_snapshots.len() {
        let name = super::super::directory::volume_name(lease, position);
        let (status, volume) = state
            .engine
            .json(Method::GET, &format!("/volumes/{name}"), Value::Null)
            .await?;
        if status != 200 {
            return Err(invalid());
        }
        super::super::directory::verify_volume(&volume, lease, position)?;
    }
    Ok(())
}

pub(super) async fn stage(
    state: &BrokerState,
    request: Request,
    path: &str,
) -> Result<Response<Body>, PlatformError> {
    let (key, index) = path
        .strip_prefix("/open-compute/v1/leases/")
        .and_then(|tail| tail.split_once("/directories/"))
        .filter(|(key, _)| opaque(key))
        .ok_or_else(invalid)?;
    let position: usize = index.parse().map_err(|_| invalid())?;
    if index != position.to_string() {
        return Err(invalid());
    }
    let entry = state
        .leases
        .lock()
        .await
        .get(key)
        .cloned()
        .ok_or_else(invalid)?;
    let mut entry = entry.lock().await;
    if entry.fenced
        || entry.cleaned
        || (entry.process_identity.is_some() && !entry.directories_ready)
    {
        return Err(invalid());
    }
    let expected = entry
        .lease
        .directory_snapshots
        .get(position)
        .and_then(|directory| directory.archive.as_ref())
        .cloned()
        .ok_or_else(invalid)?;
    let already_staged = entry
        .directory_archives
        .offsets
        .get(position)
        .ok_or_else(invalid)?
        .is_some();
    if request
        .headers()
        .get("Content-Type")
        .is_none_or(|value| value != "application/x-tar")
    {
        return Err(invalid());
    }
    if entry.directories_ready {
        if verify_existing(state, key, &entry.lease, &entry.startup)
            .await?
            .is_none()
            || directory_start::helper(state, &entry.lease)
                .await?
                .is_some()
        {
            return Err(invalid());
        }
    } else {
        let (status, _) = state
            .engine
            .json(Method::GET, &format!("/containers/{key}/json"), Value::Null)
            .await?;
        if status != 404 {
            return Err(invalid());
        }
    }
    let (file, size) = super::super::spool::stage_rootfs(
        request.into_body(),
        &state.config.spool_dir,
        expected.bytes,
    )
    .await?;
    if size != expected.bytes {
        return Err(invalid());
    }
    let file = file.into_std().await;
    let file = tokio::task::spawn_blocking(move || {
        let mut file = file;
        let archive = open_compute_artifacts::verify_container_directory_archive(
            &mut file,
            expected.bytes,
            100_000,
        )?;
        if archive.bytes != expected.bytes || hex::encode(archive.sha256) != expected.sha256 {
            return Err(invalid());
        }
        file.rewind().map_err(|_| invalid())?;
        Ok::<_, PlatformError>(file)
    })
    .await
    .map_err(|_| invalid())??;
    if already_staged || entry.directories_ready {
        return response(204, Vec::new());
    }
    state.journal.begin(key)?;
    entry.fenced = true;
    let bundle = match entry.directory_archives.file.take() {
        Some(file) => Some(file.into_std().await),
        None => None,
    };
    let directory = state.config.spool_dir.clone();
    let maximum = state.config.max_image_bytes;
    let (bundle, offset) =
        tokio::task::spawn_blocking(move || append_archive(bundle, file, &directory, maximum))
            .await
            .map_err(|_| invalid())??;
    entry.directory_archives.file = Some(tokio::fs::File::from_std(bundle));
    entry.directory_archives.offsets[position] = Some(offset);
    state.journal.clear(key)?;
    entry.fenced = false;
    response(204, Vec::new())
}

fn append_archive(
    bundle: Option<std::fs::File>,
    mut archive: std::fs::File,
    directory: &std::path::Path,
    maximum: u64,
) -> Result<(std::fs::File, u64), PlatformError> {
    let mut bundle = match bundle {
        Some(file) => file,
        None => tempfile::tempfile_in(directory).map_err(|_| invalid())?,
    };
    let offset = bundle.seek(SeekFrom::End(0)).map_err(|_| invalid())?;
    let bytes = archive.metadata().map_err(|_| invalid())?.len();
    if offset
        .checked_add(bytes)
        .is_none_or(|total| total > maximum)
    {
        return Err(invalid());
    }
    archive.rewind().map_err(|_| invalid())?;
    if std::io::copy(&mut archive, &mut bundle).map_err(|_| invalid())? != bytes {
        return Err(invalid());
    }
    Ok((bundle, offset))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};

    #[test]
    fn directory_bundle_keeps_one_file_with_exact_offsets_and_bounds() {
        let directory = tempfile::tempdir().unwrap();
        let mut first = tempfile::tempfile().unwrap();
        first.write_all(b"first").unwrap();
        let (file, first_offset) = append_archive(None, first, directory.path(), 16).unwrap();
        let mut second = tempfile::tempfile().unwrap();
        second.write_all(b"second").unwrap();
        let (mut file, second_offset) =
            append_archive(Some(file), second, directory.path(), 16).unwrap();
        assert_eq!((first_offset, second_offset), (0, 5));
        assert_eq!(file.metadata().unwrap().len(), 11);
        file.seek(SeekFrom::Start(second_offset)).unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"second");
        assert!(
            std::fs::read_dir(directory.path())
                .unwrap()
                .next()
                .is_none()
        );
        let mut excessive = tempfile::tempfile().unwrap();
        excessive.write_all(b"too-long").unwrap();
        assert!(append_archive(Some(file), excessive, directory.path(), 16).is_err());
    }
}
