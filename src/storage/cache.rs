use std::ffi::OsString;
use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};

use super::linux::Scan;

pub(super) struct Cache(Connection);

pub(super) struct FolderKey<'a> {
    path: &'a [u8],
    identity: String,
}

impl<'a> FolderKey<'a> {
    pub fn read(path: &'a Path) -> io::Result<Self> {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_dir() {
            return Err(io::Error::other(
                "The selected directory is no longer available.",
            ));
        }
        Ok(Self {
            path: path.as_os_str().as_bytes(),
            identity: format!("{}:{}", metadata.dev(), metadata.ino()),
        })
    }
}

impl Cache {
    pub fn open_default() -> io::Result<Self> {
        let base = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
            .ok_or_else(|| io::Error::other("No cache directory is available."))?;
        let dir = base.join("dominion");
        DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        Self::open(&dir.join("storage.sqlite3")).map_err(io::Error::other)
    }

    fn open(path: &Path) -> rusqlite::Result<Self> {
        let db = Connection::open(path)?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS folder_scans_v1 (
                path BLOB PRIMARY KEY,
                identity TEXT NOT NULL,
                scanned_at INTEGER NOT NULL,
                snapshot BLOB NOT NULL
            ) WITHOUT ROWID;",
        )?;
        Ok(Self(db))
    }

    pub fn load(&self, folder: &FolderKey<'_>) -> io::Result<Option<(Scan, i64)>> {
        let row: Option<(Vec<u8>, i64)> = self
            .0
            .query_row(
                "SELECT snapshot, scanned_at FROM folder_scans_v1 WHERE path = ?1 AND identity = ?2",
                params![folder.path, folder.identity],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(io::Error::other)?;
        row.map(|(snapshot, timestamp)| Ok((serde_json::from_slice(&snapshot)?, timestamp)))
            .transpose()
    }

    pub fn save(&self, folder: &FolderKey<'_>, scan: &Scan, scanned_at: i64) -> io::Result<()> {
        let snapshot = serde_json::to_vec(scan)?;
        self.0
            .execute(
                "INSERT INTO folder_scans_v1 (path, identity, scanned_at, snapshot)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(path) DO UPDATE SET
                    identity = excluded.identity,
                    scanned_at = excluded.scanned_at,
                    snapshot = excluded.snapshot
                 WHERE excluded.scanned_at >= folder_scans_v1.scanned_at",
                params![folder.path, folder.identity, scanned_at, snapshot],
            )
            .map_err(io::Error::other)?;
        Ok(())
    }
}

// Linux file names are byte sequences and can contain invalid UTF-8.
pub(super) mod entries {
    use super::*;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(
        entries: &[(u64, PathBuf)],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        entries
            .iter()
            .map(|(size, path)| (*size, path.as_os_str().as_bytes()))
            .collect::<Vec<_>>()
            .serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<(u64, PathBuf)>, D::Error> {
        Vec::<(u64, Vec<u8>)>::deserialize(deserializer).map(|entries| {
            entries
                .into_iter()
                .map(|(size, path)| (size, PathBuf::from(OsString::from_vec(path))))
                .collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Cache, FolderKey};
    use crate::storage::linux::Scan;
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn reopens_snapshots_and_replaces_them_on_refresh() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("storage.sqlite3");
        let folder = temp.path().join("folder");
        fs::create_dir(&folder).unwrap();
        let entry = folder.join(OsString::from_vec(b"file-\xff".to_vec()));
        let scan = Scan {
            files: vec![(4096, entry.clone())],
            total: 4096,
            file_count: 1,
            ..Scan::default()
        };

        {
            let cache = Cache::open(&db).unwrap();
            assert!(
                cache
                    .load(&FolderKey::read(&folder).unwrap())
                    .unwrap()
                    .is_none()
            );
            cache
                .save(&FolderKey::read(&folder).unwrap(), &scan, 1000)
                .unwrap();
        }

        let cache = Cache::open(&db).unwrap();
        let (cached, scanned_at) = cache
            .load(&FolderKey::read(&folder).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(scanned_at, 1000);
        assert_eq!(cached.files, vec![(4096, entry)]);
        assert_eq!(cached.total, 4096);
        assert_eq!(cached.file_count, 1);

        cache
            .save(&FolderKey::read(&folder).unwrap(), &Scan::default(), 2000)
            .unwrap();
        cache
            .save(&FolderKey::read(&folder).unwrap(), &scan, 1000)
            .unwrap();
        let (cached, scanned_at) = cache
            .load(&FolderKey::read(&folder).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(scanned_at, 2000);
        assert!(cached.files.is_empty());

        fs::rename(&folder, temp.path().join("old-folder")).unwrap();
        fs::create_dir(&folder).unwrap();
        assert!(
            cache
                .load(&FolderKey::read(&folder).unwrap())
                .unwrap()
                .is_none()
        );
    }
}
