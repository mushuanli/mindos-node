//! Lazy export-owned metadata. Keys are source-relative paths, never client mount paths.
use crate::core::error::Error;
use rusqlite::{params, Connection, OptionalExtension};
use rustix::fs::{Mode, OFlags};
use serde_json::{Map, Value};
use std::{
    fs::File,
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    time::Duration,
};

const DIRECTORY: &str = ".mindos";
const DATABASE: &str = "meta.sqlite";
const APPLICATION_ID: i64 = 0x4d444d54;

pub(crate) struct Metadata {
    database: Connection,
    _directory: File,
}
impl Metadata {
    pub fn open(root: &File, write: bool) -> Result<Option<Self>, Error> {
        if write {
            match rustix::fs::mkdirat(root, DIRECTORY, Mode::from_bits_truncate(0o700)) {
                Ok(()) => root.sync_all()?,
                Err(rustix::io::Errno::EXIST) => (),
                Err(error) => return Err(error.into()),
            }
        }
        let directory = match rustix::fs::openat(
            root,
            DIRECTORY,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(value) => File::from(value),
            Err(rustix::io::Errno::NOENT) if !write => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if directory.metadata()?.uid() != rustix::process::geteuid().as_raw() {
            return Err(Error::forbidden("METADATA_OWNER"));
        }
        let flags = OFlags::CLOEXEC
            | OFlags::NOFOLLOW
            | OFlags::NONBLOCK
            | if write {
                OFlags::RDWR | OFlags::CREATE
            } else {
                OFlags::RDONLY
            };
        let file = match rustix::fs::openat(
            &directory,
            DATABASE,
            flags,
            Mode::from_bits_truncate(0o600),
        ) {
            Ok(value) => File::from(value),
            Err(rustix::io::Errno::NOENT) if !write => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let info = file.metadata()?;
        if !info.is_file()
            || info.nlink() != 1
            || info.uid() != rustix::process::geteuid().as_raw()
            || info.mode() & 0o077 != 0
        {
            return Err(Error::forbidden("METADATA_PERMISSIONS"));
        }
        if write {
            let journal = File::from(rustix::fs::openat(
                &directory,
                "meta.sqlite-journal",
                flags,
                Mode::from_bits_truncate(0o600),
            )?);
            let info = journal.metadata()?;
            if !info.is_file()
                || info.nlink() != 1
                || info.uid() != rustix::process::geteuid().as_raw()
                || info.mode() & 0o077 != 0
            {
                return Err(Error::forbidden("METADATA_PERMISSIONS"));
            }
        }
        let path = format!("/proc/self/fd/{}/{DATABASE}", directory.as_raw_fd());
        let database = Connection::open_with_flags(
            path,
            if write {
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
            } else {
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            },
        )
        .map_err(sql)?;
        database.busy_timeout(Duration::from_secs(5)).map_err(sql)?;
        let application: i64 = database
            .pragma_query_value(None, "application_id", |row| row.get(0))
            .map_err(sql)?;
        let version: i64 = database
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(sql)?;
        if write && application == 0 && version == 0 {
            database.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; BEGIN IMMEDIATE; CREATE TABLE file_metadata(path TEXT PRIMARY KEY, data TEXT NOT NULL, identity TEXT NOT NULL DEFAULT (lower(hex(randomblob(16))))); PRAGMA application_id=1296321876; PRAGMA user_version=1; COMMIT;").map_err(sql)?;
            directory.sync_all()?;
        } else if !write && application == 0 && version == 0 {
            return Ok(None);
        } else if application != APPLICATION_ID || version != 1 {
            return Err(Error::conflict("METADATA_VERSION"));
        }
        Ok(Some(Self {
            database,
            _directory: directory,
        }))
    }
    pub fn existing_write(root: &File) -> Result<Option<Self>, Error> {
        if Self::open(root, false)?.is_none() {
            return Ok(None);
        }
        Self::open(root, true)
    }
    pub fn identity(&self, path: &str) -> Result<Option<String>, Error> {
        self.database
            .query_row(
                "SELECT identity FROM file_metadata WHERE path=?1",
                [path],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql)
    }
    pub fn read(&self, path: &str) -> Result<Map<String, Value>, Error> {
        let raw: Option<String> = self
            .database
            .query_row(
                "SELECT data FROM file_metadata WHERE path=?1",
                [path],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql)?;
        raw.map(|value| {
            serde_json::from_str(&value).map_err(|_| Error::conflict("METADATA_INVALID"))
        })
        .unwrap_or_else(|| Ok(Map::new()))
    }
    pub fn patch(
        &mut self,
        path: &str,
        patch: &Map<String, Value>,
        remove: &[String],
    ) -> Result<(), Error> {
        let transaction = self
            .database
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(sql)?;
        let raw: Option<String> = transaction
            .query_row(
                "SELECT data FROM file_metadata WHERE path=?1",
                [path],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql)?;
        let mut value: Map<String, Value> = raw
            .map(|raw| serde_json::from_str(&raw))
            .transpose()
            .map_err(|_| Error::conflict("METADATA_INVALID"))?
            .unwrap_or_default();
        value.extend(patch.clone());
        for key in remove {
            value.remove(key);
        }
        let data = serde_json::to_string(&value).map_err(|_| Error::invalid())?;
        if data.len() > 512 * 1024 {
            return Err(Error::too_large("METADATA_LIMIT"));
        }
        transaction.execute("INSERT INTO file_metadata(path,data) VALUES(?1,?2) ON CONFLICT(path) DO UPDATE SET data=excluded.data", params![path,data]).map_err(sql)?;
        transaction.commit().map_err(sql)
    }
    pub fn remove(&self, path: &str) -> Result<(), Error> {
        self.database
            .execute(
                "DELETE FROM file_metadata WHERE path=?1 OR substr(path,1,length(?1)+1)=?1||'/'",
                [path],
            )
            .map_err(sql)?;
        Ok(())
    }
    pub fn rename(&mut self, path: &str, to: &str) -> Result<(), Error> {
        let tx = self.database.transaction().map_err(sql)?;
        tx.execute(
            "DELETE FROM file_metadata WHERE path=?1 OR substr(path,1,length(?1)+1)=?1||'/'",
            [to],
        )
        .map_err(sql)?;
        tx.execute("UPDATE file_metadata SET path=?2||substr(path,length(?1)+1) WHERE path=?1 OR substr(path,1,length(?1)+1)=?1||'/'", params![path,to]).map_err(sql)?;
        tx.commit().map_err(sql)
    }
}
fn sql(_: rusqlite::Error) -> Error {
    Error::new(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "METADATA_IO")
}
