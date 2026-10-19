//! Unix filesystem and caller checks for the private provider endpoint.

use super::contract::invalid;
use open_compute_core::{PlatformError, config::validate_container_socket};
use std::{
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

pub(super) fn validate_socket(path: &Path, uid: u32) -> Result<(), PlatformError> {
    validate_container_socket(path)?;
    validate_parent(path, uid)?;
    let metadata = std::fs::symlink_metadata(path).map_err(|_| invalid())?;
    if !metadata.file_type().is_socket()
        || metadata.uid() != uid
        || metadata.mode() & 0o777 != 0o600
    {
        return Err(invalid());
    }
    Ok(())
}

pub(super) fn validate_engine_socket(path: &Path) -> Result<(), PlatformError> {
    validate_container_socket(path)?;
    let mut current = PathBuf::new();
    for part in path.components() {
        current.push(part);
        let metadata = std::fs::symlink_metadata(&current).map_err(|_| invalid())?;
        if metadata.file_type().is_symlink() {
            return Err(invalid());
        }
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|_| invalid())?;
    if !metadata.file_type().is_socket()
        || ![0, rustix::process::getuid().as_raw()].contains(&metadata.uid())
        || metadata.mode() & 0o002 != 0
    {
        return Err(invalid());
    }
    Ok(())
}

fn validate_parent(path: &Path, uid: u32) -> Result<(), PlatformError> {
    let parent = path.parent().ok_or_else(invalid)?;
    validate_directory(parent, uid)
}

pub(super) fn validate_directory(parent: &Path, uid: u32) -> Result<(), PlatformError> {
    let mut current = PathBuf::new();
    for part in parent.components() {
        current.push(part);
        let metadata = std::fs::symlink_metadata(&current).map_err(|_| invalid())?;
        if !metadata.file_type().is_dir() {
            return Err(invalid());
        }
    }
    let metadata = std::fs::symlink_metadata(parent).map_err(|_| invalid())?;
    if metadata.uid() != uid || metadata.mode() & 0o777 != 0o700 {
        return Err(invalid());
    }
    Ok(())
}

pub(super) struct BrokerSocket {
    pub(super) listener: Option<tokio::net::UnixListener>,
    path: PathBuf,
    identity: (u64, u64),
}

impl BrokerSocket {
    pub(super) fn bind(path: PathBuf, uid: u32) -> Result<Self, PlatformError> {
        validate_container_socket(&path)?;
        validate_parent(&path, uid)?;
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => return Err(invalid()),
        }
        let listener = tokio::net::UnixListener::bind(&path).map_err(|_| invalid())?;
        if std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).is_err() {
            let _ = std::fs::remove_file(&path);
            return Err(invalid());
        }
        let metadata = std::fs::symlink_metadata(&path).map_err(|_| invalid())?;
        Ok(Self {
            listener: Some(listener),
            path,
            identity: (metadata.dev(), metadata.ino()),
        })
    }
}

impl Drop for BrokerSocket {
    fn drop(&mut self) {
        if std::fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == self.identity)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
