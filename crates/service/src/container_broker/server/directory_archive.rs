//! Rebase a verified tree into an unstarted receiver without changing stored link targets.

use super::*;
use std::{
    fs::File,
    io::{self, BufReader, BufWriter, Read as _, Seek as _, Write},
    ops::Range,
    path::{Component, Path, PathBuf},
};

pub(super) async fn rebase(
    source: tokio::fs::File,
    range: Range<u64>,
    position: usize,
    spool: PathBuf,
) -> Result<tokio::fs::File, PlatformError> {
    let source = source.into_std().await;
    let output = tokio::task::spawn_blocking(move || rebase_file(source, range, position, &spool))
        .await
        .map_err(|_| invalid())??;
    Ok(tokio::fs::File::from_std(output))
}

fn prefixed(prefix: &Path, path: &Path) -> Result<PathBuf, PlatformError> {
    let mut output = prefix.to_owned();
    for part in path.components() {
        match part {
            Component::Normal(name) => output.push(name),
            Component::CurDir => {}
            _ => return Err(invalid()),
        }
    }
    Ok(output)
}

fn rebase_file(
    mut source: File,
    range: Range<u64>,
    position: usize,
    spool: &Path,
) -> Result<File, PlatformError> {
    let size = range
        .end
        .checked_sub(range.start)
        .filter(|n| *n > 0)
        .ok_or_else(invalid)?;
    let source_size = source.metadata().map_err(|_| invalid())?.len();
    if range.end > source_size {
        return Err(invalid());
    }
    source
        .seek(io::SeekFrom::Start(range.start))
        .map_err(|_| invalid())?;
    // Prefixing can add a GNU name and link header for each original 512-byte header.
    let maximum = size
        .checked_mul(5)
        .and_then(|n| n.checked_add(16_384))
        .ok_or_else(invalid)?;
    let mut output = tempfile::tempfile_in(spool).map_err(|_| invalid())?;
    let limited = LimitedWriter {
        file: &mut output,
        left: maximum,
    };
    let mut builder = tar::Builder::new(BufWriter::with_capacity(256 * 1024, limited));
    let mut archive = tar::Archive::new(BufReader::with_capacity(
        256 * 1024,
        (&mut source).take(size),
    ));
    let prefix = PathBuf::from(format!(
        "{}/{position}",
        super::super::directory_helper::MOUNTS.trim_start_matches('/')
    ));
    for (count, entry) in archive.entries().map_err(|_| invalid())?.enumerate() {
        if count >= 100_000 {
            return Err(invalid());
        }
        let mut entry = entry.map_err(|_| invalid())?;
        let name = prefixed(&prefix, &entry.path().map_err(|_| invalid())?)?;
        let mut metadata = Vec::new();
        if let Some(extensions) = entry.pax_extensions().map_err(|_| invalid())? {
            for extension in extensions {
                let extension = extension.map_err(|_| invalid())?;
                let key = extension.key().map_err(|_| invalid())?;
                // Sparse formats can reinterpret paths or expand beyond the verified tree.
                if key.starts_with("GNU.sparse.") || key == "SCHILY.realsize" {
                    return Err(invalid());
                }
                if !matches!(key, "path" | "linkpath" | "size") {
                    metadata.push((key.to_owned(), extension.value_bytes().to_vec()));
                }
            }
        }
        builder
            .append_pax_extensions(metadata.iter().map(|(k, v)| (k.as_str(), v.as_slice())))
            .map_err(|_| invalid())?;
        let mut header = entry.header().clone();
        header.set_size(entry.size());
        match header.entry_type() {
            tar::EntryType::Regular | tar::EntryType::Directory => {
                builder
                    .append_data(&mut header, name, &mut entry)
                    .map_err(|_| invalid())?;
            }
            tar::EntryType::Symlink | tar::EntryType::Link => {
                let target = entry
                    .link_name()
                    .map_err(|_| invalid())?
                    .ok_or_else(invalid)?;
                let target = if header.entry_type().is_hard_link() {
                    prefixed(&prefix, &target)?
                } else {
                    target.into_owned()
                };
                header.set_size(0);
                builder
                    .append_link(&mut header, name, target)
                    .map_err(|_| invalid())?;
            }
            _ => return Err(invalid()),
        }
    }
    let writer = builder.into_inner().map_err(|_| invalid())?;
    let _ = writer.into_inner().map_err(|_| invalid())?;
    drop(archive);
    if source.metadata().map_err(|_| invalid())?.len() != source_size {
        return Err(invalid());
    }
    output.rewind().map_err(|_| invalid())?;
    Ok(output)
}

struct LimitedWriter<'a> {
    file: &'a mut File,
    left: u64,
}
impl Write for LimitedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > self.left {
            return Err(io::Error::other(
                "directory archive transport bound exceeded",
            ));
        }
        let count = self.file.write(bytes)?;
        self.left -= count as u64;
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(bytes: &[u8]) -> File {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(bytes).unwrap();
        file.rewind().unwrap();
        file
    }

    #[test]
    fn rebasing_preserves_long_pax_paths_links_owners_modes_and_timestamps() {
        let long = format!("{}/state", "nested/".repeat(30));
        let mut tar = tar::Builder::new(Vec::new());
        tar.append_pax_extensions([
            ("path", long.as_bytes()),
            ("mtime", b"1700000000.123456789".as_slice()),
        ])
        .unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o640);
        header.set_uid(1001);
        header.set_gid(1002);
        header.set_mtime(1_700_000_000);
        header.set_size(4);
        header.set_cksum();
        tar.append_data(&mut header, "short", b"data".as_slice())
            .unwrap();
        for (name, target, kind) in [
            ("alias", long.as_str(), tar::EntryType::Link),
            ("parent-link", "../elsewhere", tar::EntryType::Symlink),
            ("absolute-link", "/etc/example", tar::EntryType::Symlink),
        ] {
            let mut header = header.clone();
            header.set_size(0);
            header.set_entry_type(kind);
            tar.append_link(&mut header, name, target).unwrap();
        }
        let bytes = tar.into_inner().unwrap();
        let root = tempfile::tempdir().unwrap();
        let out = rebase_file(input(&bytes), 0..bytes.len() as u64, 7, root.path()).unwrap();
        let mut tar = tar::Archive::new(out);
        let mut entries = tar.entries().unwrap();
        let mut file = entries.next().unwrap().unwrap();
        assert_eq!(
            file.path().unwrap().as_ref(),
            Path::new("open-compute-directories/7").join(&long)
        );
        assert_eq!(
            (
                file.header().mode().unwrap(),
                file.header().uid().unwrap(),
                file.header().gid().unwrap()
            ),
            (0o640, 1001, 1002)
        );
        assert!(file.pax_extensions().unwrap().unwrap().any(|x| {
            let x = x.unwrap();
            x.key_bytes() == b"mtime" && x.value_bytes() == b"1700000000.123456789"
        }));
        let mut body = String::new();
        file.read_to_string(&mut body).unwrap();
        assert_eq!(body, "data");
        let link = entries.next().unwrap().unwrap();
        assert_eq!(
            link.link_name().unwrap().unwrap().as_ref(),
            Path::new("open-compute-directories/7").join(&long)
        );
        assert_eq!(
            entries
                .next()
                .unwrap()
                .unwrap()
                .link_name()
                .unwrap()
                .unwrap()
                .as_ref(),
            Path::new("../elsewhere")
        );
        assert_eq!(
            entries
                .next()
                .unwrap()
                .unwrap()
                .link_name()
                .unwrap()
                .unwrap()
                .as_ref(),
            Path::new("/etc/example")
        );
        assert!(entries.next().is_none());
    }

    #[test]
    fn rebasing_rejects_escape_truncation_sparse_and_output_overflow() {
        let root = tempfile::tempdir().unwrap();
        assert!(rebase_file(input(&[0; 511]), 0..511, 0, root.path()).is_err());
        assert!(rebase_file(input(&[0; 1024]), 0..2048, 0, root.path()).is_err());
        assert!(prefixed(Path::new("fixed/0"), Path::new("../other")).is_err());
        assert!(prefixed(Path::new("fixed/0"), Path::new("/other")).is_err());
        let mut tar = tar::Builder::new(Vec::new());
        tar.append_pax_extensions([("GNU.sparse.name", b"../escape".as_slice())])
            .unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o600);
        header.set_size(0);
        header.set_cksum();
        tar.append_data(&mut header, "state", io::empty()).unwrap();
        let bytes = tar.into_inner().unwrap();
        assert!(rebase_file(input(&bytes), 0..bytes.len() as u64, 0, root.path()).is_err());
        let mut out = tempfile::tempfile().unwrap();
        let mut limited = LimitedWriter {
            file: &mut out,
            left: 2,
        };
        assert!(limited.write_all(b"too long").is_err());
        assert_eq!(out.metadata().unwrap().len(), 0);
        assert!(root.path().read_dir().unwrap().next().is_none());
    }
}
