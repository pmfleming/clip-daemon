use super::{ArtifactRecord, InlineEchoRecord, MAX_ENTRY_BYTES, StorePolicy, StoredEntry};
use crate::{
    backend::{BackendError, BackendResult, EntryTarget, invalid_data, operation_error},
    classification::{INSPECTION_LIMIT, bounded_preview},
    history::content::ResolvedContent,
    model::{EntryKind, EntrySummary, HistoryPage, HistoryQuery},
};
use futures_lite::future::block_on;
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};
use turso::{Connection, IntoParams, Row, params};
const APP_ID: i64 = 0x434c4950;
const SCHEMA_VERSION: i64 = 1;
const COLUMNS: &str = "e.id,e.revision,e.kind,e.resolved_mime,e.byte_size,e.favorite,e.preview,e.recency=(SELECT max(recency) FROM entries)";

struct Owner(File);
impl Drop for Owner {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}
pub(super) struct Database {
    conn: Connection,
    _database: turso::Database,
    _file: File,
    _owner: Owner,
    path: PathBuf,
    pub(super) blocked: bool,
}
fn private_file(path: &Path, create_new: bool) -> BackendResult<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(create_new)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)
        .map_err(operation_error)?;
    let meta = file.metadata().map_err(operation_error)?;
    if !meta.is_file() || meta.nlink() != 1 || meta.uid() != rustix::process::geteuid().as_raw() {
        return Err(invalid_data("Unsafe history file ownership or type"));
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(operation_error)?;
    Ok(file)
}
fn open_private(path: &Path) -> BackendResult<(File, bool)> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok((private_file(path, false)?, false)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((private_file(path, true)?, true)),
        Err(e) => Err(operation_error(e)),
    }
}
fn protect_sidecars(path: &Path) -> BackendResult<()> {
    for suffix in ["-wal", "-shm"] {
        let side = sidecar(path, suffix);
        match fs::symlink_metadata(&side) {
            Ok(_) => {
                private_file(&side, false)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(operation_error(error)),
        }
    }
    Ok(())
}
impl Database {
    pub(super) fn open(path: &Path) -> BackendResult<Self> {
        let root = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| invalid_data("History database needs a parent directory"))?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root)
            .map_err(operation_error)?;
        // Reject symlink components rather than trusting a writable redirected store.
        for ancestor in root.ancestors().filter(|p| !p.as_os_str().is_empty()) {
            if fs::symlink_metadata(ancestor)
                .map_err(operation_error)?
                .file_type()
                .is_symlink()
            {
                return Err(invalid_data("Symlink history directories are not allowed"));
            }
        }
        let meta = fs::symlink_metadata(root).map_err(operation_error)?;
        if !meta.is_dir() || meta.uid() != rustix::process::geteuid().as_raw() {
            return Err(invalid_data("Unsafe history directory ownership"));
        }
        fs::set_permissions(root, fs::Permissions::from_mode(0o700)).map_err(operation_error)?;
        let lock_path = sidecar(path, ".lock");
        let (lock, _) = open_private(&lock_path)?;
        lock.try_lock()
            .map_err(|_| BackendError::unavailable("History database already has an owner"))?;
        let owner = Owner(lock);
        let (file, fresh) = open_private(path)?;
        if !fresh
            && file.metadata().map_err(operation_error)?.len() == 0
            && !fs::symlink_metadata(sidecar(path, "-wal"))
                .is_ok_and(|m| m.is_file() && m.len() > 0)
        {
            return Err(invalid_data(
                "Existing empty history database without recovery WAL is not a fresh store",
            ));
        }
        protect_sidecars(path)?;
        let database = block_on(
            turso::Builder::new_local(
                path.to_str()
                    .ok_or_else(|| invalid_data("History path must be UTF-8"))?,
            )
            .with_io(turso::IoBackend::Syscall)
            .experimental_multiprocess_wal(true)
            .experimental_index_method(true)
            .build(),
        )
        .map_err(operation_error)?;
        let conn = database.connect().map_err(operation_error)?;
        let mut db = Self {
            conn,
            _database: database,
            _file: file,
            _owner: owner,
            path: path.to_owned(),
            blocked: false,
        };
        // Check identity before executing any schema or policy mutations.
        if !fresh {
            if db.scalar("PRAGMA application_id", ())? != APP_ID
                || db.scalar("PRAGMA user_version", ())? != SCHEMA_VERSION
            {
                return Err(invalid_data(
                    "Unrecognized history database identity or schema",
                ));
            }
            let rows = db.rows("SELECT identity FROM store_meta WHERE id=1", ())?;
            if rows.len() != 1 || text(&rows[0], 0)? != "clip-daemon-history-v1" {
                return Err(invalid_data("Unrecognized history database identity"));
            }
        }
        db.exec("PRAGMA foreign_keys=ON", ())?;
        db.exec("PRAGMA synchronous=FULL", ())?;
        if db.scalar("PRAGMA foreign_keys", ())? != 1 || db.scalar("PRAGMA synchronous", ())? != 2 {
            return Err(BackendError::unavailable(
                "History durability settings were not applied",
            ));
        }
        if fresh {
            db.transaction(|db| {
                block_on(db.conn.execute_batch(super::schema::SCHEMA)).map_err(operation_error)?;
                db.exec(&format!("PRAGMA application_id={APP_ID}"), ())?;
                db.exec(&format!("PRAGMA user_version={SCHEMA_VERSION}"), ())?;
                Ok(())
            })?;
        }
        db.validate_schema()?;
        // Admission is a live readiness/privacy decision, never restored on restart.
        db.transaction(|db| {
            db.exec("UPDATE effective_policy SET admission=0 WHERE id=1", ())?;
            Ok(())
        })?;
        db.policy()?;
        db.revision()?;
        protect_sidecars(path)?;
        Ok(db)
    }
    fn validate_schema(&self) -> BackendResult<()> {
        // Version tags alone are not permission to trust a partially created or
        // foreign schema. Compare application objects, including constraints and
        // indexes, to this one supported schema; there is no upgrade path.
        let mut expected = super::schema::SCHEMA
            .split(';')
            .map(str::trim)
            .filter(|sql| sql.starts_with("CREATE "))
            .map(normalize_schema)
            .collect::<HashSet<_>>();
        // Turso 0.8.3's native FTS implementation creates these two relational
        // backing objects. Validate them too, rather than accepting any prefix.
        expected.extend([
            "CREATE TABLE __turso_internal_fts_dir_search_grams(path TEXT NOT NULL,chunk_no INTEGER NOT NULL,bytes BLOB NOT NULL)",
            "CREATE INDEX IF NOT EXISTS __turso_internal_fts_dir_search_grams_key ON __turso_internal_fts_dir_search_grams USING backing_btree(path,chunk_no,bytes)",
        ].into_iter().map(normalize_schema));
        let actual = self
            .rows(
                "SELECT sql FROM sqlite_schema WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%'",
                (),
            )?
            .iter()
            .map(|row| text(row, 0).map(|sql| normalize_schema(&sql)))
            .collect::<BackendResult<HashSet<_>>>()?;
        if actual != expected {
            return Err(invalid_data("Unrecognized or incomplete history schema"));
        }
        let violations = self.rows("PRAGMA foreign_key_check", ())?;
        if !violations.is_empty() {
            return Err(invalid_data(
                "History database referential integrity check failed",
            ));
        }
        Ok(())
    }
    fn exec(&self, sql: &str, params: impl IntoParams) -> BackendResult<u64> {
        block_on(async { self.conn.prepare_cached(sql).await?.execute(params).await })
            .map_err(operation_error)
    }
    fn rows(&self, sql: &str, params: impl IntoParams) -> BackendResult<Vec<Row>> {
        block_on(async {
            let mut statement = self.conn.prepare_cached(sql).await?;
            let mut rows = statement.query(params).await?;
            let mut result = Vec::new();
            while let Some(row) = rows.next().await? {
                result.push(row);
            }
            Ok::<_, turso::Error>(result)
        })
        .map_err(operation_error)
    }
    fn scalar(&self, sql: &str, params: impl IntoParams) -> BackendResult<i64> {
        let rows = self.rows(sql, params)?;
        rows.first()
            .ok_or_else(|| invalid_data("Missing history metadata"))
            .and_then(|r| integer(r, 0))
    }
    fn transaction<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> BackendResult<T>,
    ) -> BackendResult<T> {
        if self.blocked {
            return Err(BackendError::unavailable("History storage is fenced"));
        }
        self.exec("BEGIN IMMEDIATE", ())?;
        match f(self) {
            Ok(value) => {
                if self.exec("COMMIT", ()).is_err() {
                    self.blocked = true;
                    return Err(BackendError::unavailable(
                        "History commit outcome is uncertain; restart required",
                    ));
                }
                if protect_sidecars(&self.path).is_err() {
                    self.blocked = true;
                    return Err(BackendError::unavailable(
                        "History sidecar protection failed; restart required",
                    ));
                }
                Ok(value)
            }
            Err(error) => {
                if self.exec("ROLLBACK", ()).is_err() {
                    self.blocked = true;
                    return Err(BackendError::unavailable(
                        "History rollback failed; restart required",
                    ));
                }
                Err(error)
            }
        }
    }
    fn bump(&self) -> BackendResult<u64> {
        self.exec("UPDATE store_meta SET revision=revision+1 WHERE id=1", ())?;
        self.revision()
    }
    fn sequence(&self) -> BackendResult<i64> {
        self.exec("UPDATE store_meta SET sequence=sequence+1 WHERE id=1", ())?;
        self.scalar("SELECT sequence FROM store_meta WHERE id=1", ())
    }
    pub(super) fn revision(&self) -> BackendResult<u64> {
        Ok(self.scalar("SELECT revision FROM store_meta WHERE id=1", ())? as u64)
    }
    pub(super) fn policy(&self) -> BackendResult<StorePolicy> {
        let rows=self.rows("SELECT admission,max_entries,max_favorites,max_entry_bytes,max_total_bytes FROM effective_policy WHERE id=1",())?;
        let row = rows
            .first()
            .ok_or_else(|| invalid_data("Missing history policy"))?;
        Ok(StorePolicy {
            admission: integer(row, 0)? != 0,
            max_entries: integer(row, 1)? as u32,
            max_favorites: integer(row, 2)? as u32,
            max_entry_bytes: integer(row, 3)? as u64,
            max_total_bytes: integer(row, 4)? as u64,
        })
    }
    pub(super) fn apply_policy(&mut self, policy: StorePolicy) -> BackendResult<()> {
        if policy.max_entries > 100_000
            || policy.max_favorites > 10_000
            || policy.max_entry_bytes == 0
            || policy.max_entry_bytes > MAX_ENTRY_BYTES
            || policy.max_total_bytes == 0
            || policy.max_total_bytes > i64::MAX as u64
        {
            return Err(invalid_data("Invalid history retention policy"));
        }
        self.transaction(|db| {
            if db.policy()?==policy { return Ok(()); }
            db.exec("UPDATE effective_policy SET admission=?1,max_entries=?2,max_favorites=?3,max_entry_bytes=?4,max_total_bytes=?5 WHERE id=1",params![i64::from(policy.admission),i64::from(policy.max_entries),i64::from(policy.max_favorites),policy.max_entry_bytes as i64,policy.max_total_bytes as i64])?;
            db.retain(&policy,None)?;
            db.bump()?;
            Ok(())
        })
    }
    pub(super) fn summary(&self, id: &str, expected: Option<u64>) -> BackendResult<EntrySummary> {
        let rows = self.rows(
            &format!("SELECT {COLUMNS} FROM entries e WHERE e.id=?1"),
            [id],
        )?;
        let summary = decode_summary(
            rows.first()
                .ok_or_else(|| BackendError::not_found("Clipboard entry does not exist"))?,
        )?;
        if expected.is_some_and(|r| r != summary.revision) {
            return Err(BackendError::stale("Clipboard entry revision changed"));
        }
        Ok(summary)
    }
    pub(super) fn get(&self, id: &str, expected: Option<u64>) -> BackendResult<StoredEntry> {
        let summary = self.summary(id, expected)?;
        let rows = self.rows("SELECT p.bytes,e.mime FROM payloads p JOIN entries e ON e.id=p.entry_id WHERE p.entry_id=?1", [id])?;
        let bytes = rows
            .first()
            .ok_or_else(|| invalid_data("Clipboard payload is missing"))?
            .get::<Vec<u8>>(0)
            .map_err(operation_error)?;
        if bytes.len() as u64 != summary.byte_size {
            return Err(invalid_data("Clipboard payload length mismatch"));
        }
        let mime = text(&rows[0], 1)?;
        Ok(StoredEntry {
            summary,
            mime,
            bytes,
        })
    }
    pub(super) fn query(&self, query: HistoryQuery) -> BackendResult<HistoryPage> {
        let needle = query.query.trim().to_lowercase();
        let limit = query.limit.min(crate::backend::MAX_QUERY_LIMIT);
        let offset = i64::try_from(query.offset).unwrap_or(i64::MAX);
        let mut current = self
            .rows(
                &format!("SELECT {COLUMNS} FROM entries e WHERE e.id=(SELECT CASE WHEN ?1=1 THEN coalesce(echo_source_id,id) ELSE id END FROM entries ORDER BY recency DESC,id LIMIT 1)"),
                [i64::from(query.collapse_self_echoes)],
            )?
            .first()
            .map(decode_summary)
            .transpose()?;
        if let Some(current) = &mut current {
            current.current = true;
        }
        // Native FTS trigrams narrow literal substring candidates. Very short /
        // punctuation-only queries and large unindexed documents retain exact
        // SQL instr semantics; token syntax is never interpreted from user input.
        let gram = search_gram(&needle);
        let candidates = if gram.is_some() {
            "AND e.id IN (SELECT entry_id FROM search_content WHERE fts_match(indexed_content,?5) UNION ALL SELECT entry_id FROM search_content WHERE indexed_complete=0)"
        } else {
            "AND ?5=''"
        };
        let gram = gram.unwrap_or_default();
        let filter = if needle.is_empty() {
            // Browsing never opens payload or searchable-text rows.
            "FROM entries e WHERE (?4=0 OR e.echo_source_id IS NULL) AND ?1='' AND ?5=''".to_owned()
        } else {
            format!(
                "FROM entries e JOIN search_content s ON s.entry_id=e.id WHERE instr(s.content,?1)>0 AND (?4=0 OR e.echo_source_id IS NULL) {candidates}"
            )
        };
        let parameters = params![
            needle.clone(),
            (limit + 1) as i64,
            offset,
            i64::from(query.collapse_self_echoes),
            gram.clone()
        ];
        let rows = self.rows(
            &format!("SELECT {COLUMNS} {filter} ORDER BY e.recency DESC,e.id LIMIT ?2 OFFSET ?3"),
            parameters,
        )?;
        let total = self.scalar(
            &format!("SELECT count(*) {filter} AND ?2>=0 AND ?3>=0"),
            params![
                needle,
                (limit + 1) as i64,
                offset,
                i64::from(query.collapse_self_echoes),
                gram
            ],
        )? as usize;
        let has_more = rows.len() > limit;
        let mut entries = rows
            .iter()
            .take(limit)
            .map(decode_summary)
            .collect::<BackendResult<Vec<_>>>()?;
        for entry in &mut entries {
            entry.current = current
                .as_ref()
                .is_some_and(|current| current.id == entry.id);
        }
        let next_offset = if has_more && limit > 0 {
            query.offset.checked_add(entries.len())
        } else {
            None
        };
        Ok(HistoryPage {
            revision: self.revision()?,
            total,
            generation: query.generation,
            current,
            entries,
            has_more,
            next_offset,
        })
    }
    fn allowed_payload(policy: &StorePolicy, bytes: &[u8]) -> BackendResult<()> {
        if bytes.len() as u64 > policy.max_entry_bytes
            || bytes.len() as u64 > policy.max_total_bytes
        {
            return Err(invalid_data(
                "Clipboard entry exceeds effective storage limits",
            ));
        }
        Ok(())
    }
    pub(super) fn insert(&mut self, mime: &str, bytes: &[u8]) -> BackendResult<EntrySummary> {
        self.transaction(|db| {
            let policy=db.policy()?;
            if !policy.admission { return Err(BackendError::unavailable("Clipboard history admission is closed")); }
            Self::allowed_payload(&policy,bytes)?;
            let digest=Sha256::digest(bytes).to_vec();
            // Digest narrows indexed candidates; BLOB equality authorizes deduplication.
            let candidates=db.rows("SELECT e.id FROM entries e JOIN payloads p ON p.entry_id=e.id WHERE e.digest=?1 AND e.mime=?2 AND e.byte_size=?3 AND p.bytes=?4 ORDER BY e.recency DESC LIMIT 1",params![digest.clone(),mime,bytes.len() as i64,bytes])?;
            if let Some(row)=candidates.first() {
                let existing=db.summary(&text(row,0)?,None)?;
                // Re-observing the already-current exact bytes changes nothing.
                // In particular an explicit copy must not invalidate its own
                // revision merely because our capture worker sees publication.
                if existing.current { return Ok(existing); }
            }
            let revision=db.bump()?;
            let recency=db.sequence()?;
            let id=if let Some(row)=candidates.first() {
                let id=text(row,0)?;
                db.exec("UPDATE entries SET revision=?1,recency=?2 WHERE id=?3",params![revision as i64,recency,id.clone()])?;
                id
            } else {
                if policy.max_entries==0 { return Err(invalid_data("Clipboard main history capacity is zero")); }
                let id=format!("entry-{}",uuid::Uuid::new_v4());
                let content=ResolvedContent::resolve(mime,bytes,MAX_ENTRY_BYTES);
                db.exec("INSERT INTO entries(id,revision,favorite,recency,mime,byte_size,digest,preview,kind,resolved_mime) VALUES(?1,?2,0,?3,?4,?5,?6,?7,?8,?9)",params![id.clone(),revision as i64,recency,mime,bytes.len() as i64,digest,bounded_preview(bytes,INSPECTION_LIMIT),content.kind().label(),content.mime()])?;
                db.write_content(&id,mime,bytes,&content)?;
                id
            };
            db.retain(&policy,Some(&id))?;
            db.summary(&id,None)
        })
    }
    fn write_content(
        &self,
        id: &str,
        mime: &str,
        bytes: &[u8],
        resolved: &ResolvedContent<'_>,
    ) -> BackendResult<()> {
        self.exec(
            "INSERT INTO payloads(entry_id,bytes) VALUES(?1,?2)",
            params![id, bytes],
        )?;
        // Invalid UTF-8 is not lossily searchable. MIME remains searchable for binary data.
        let content =
            format!("{}\n{}", mime, std::str::from_utf8(bytes).unwrap_or("")).to_lowercase();
        // Bound native index amplification for huge payloads. They remain fully
        // searchable by SQL verification and are always included as candidates.
        let indexed_complete = content.len() <= 64 * 1024;
        let indexed_content = if indexed_complete {
            content.as_str()
        } else {
            ""
        };
        self.exec("INSERT INTO search_content(entry_id,content,indexed_content,indexed_complete) VALUES(?1,?2,?3,?4)",params![id,content.as_str(),indexed_content,i64::from(indexed_complete)])?;
        if let Ok(text) = std::str::from_utf8(bytes) {
            let mut paths = HashSet::new();
            for line in text.lines() {
                if let Ok(url) = url::Url::parse(line.trim())
                    && let Ok(path) = url.to_file_path()
                    && let Some(path) = path.to_str()
                {
                    paths.insert(path.to_owned());
                }
            }
            for path in paths {
                self.exec(
                    "INSERT INTO entry_artifacts(entry_id,path) VALUES(?1,?2)",
                    params![id, path],
                )?;
            }
        }
        // A URI echo is hidden only after checking registered file ownership and
        // exact image identity; arbitrary user paths never become echo aliases.
        if let Some(source) = resolved.local_image()
            && let Some(path) = source.path.to_str()
            && let Some(record) = self.artifact_get(path)?
            && record.source_entry_id != id
            && self.scalar(
                "SELECT count(*) FROM entries WHERE id=?1",
                [record.source_entry_id.as_str()],
            )? == 1
            && source.path.symlink_metadata().is_ok_and(|m| {
                m.is_file()
                    && m.dev() == record.device
                    && m.ino() == record.inode
                    && m.uid() == record.owner
            })
            && source.identity().as_deref() == Some(record.image_identity.as_str())
        {
            self.exec(
                "UPDATE entries SET echo_source_id=?1 WHERE id=?2",
                params![record.source_entry_id, id],
            )?;
        }
        Ok(())
    }
    pub(super) fn replace(
        &mut self,
        id: &str,
        expected: u64,
        mime: &str,
        bytes: &[u8],
    ) -> BackendResult<EntrySummary> {
        self.transaction(|db| {
            db.summary(id,Some(expected))?;
            let policy=db.policy()?;
            Self::allowed_payload(&policy,bytes)?;
            let revision=db.bump()?;
            let content=ResolvedContent::resolve(mime,bytes,MAX_ENTRY_BYTES);
            db.exec("UPDATE entries SET revision=?1,mime=?2,byte_size=?3,digest=?4,preview=?5,kind=?6,resolved_mime=?9,echo_source_id=NULL WHERE id=?7 AND revision=?8",params![revision as i64,mime,bytes.len() as i64,Sha256::digest(bytes).to_vec(),bounded_preview(bytes,INSPECTION_LIMIT),content.kind().label(),id,expected as i64,content.mime()])?;
            db.exec("UPDATE entries SET echo_source_id=NULL WHERE echo_source_id=?1",[id])?;
            db.exec("DELETE FROM payloads WHERE entry_id=?1",[id])?;
            db.exec("DELETE FROM search_content WHERE entry_id=?1",[id])?;
            db.exec("DELETE FROM entry_artifacts WHERE entry_id=?1",[id])?;
            db.write_content(id,mime,bytes,&content)?;
            db.retain(&policy,Some(id))?;
            db.summary(id,None)
        })
    }
    pub(super) fn favorite(&mut self, id: &str, expected: u64, value: bool) -> BackendResult<()> {
        self.transaction(|db| {
            let summary = db.summary(id, Some(expected))?;
            if summary.favorite == value {
                return Ok(());
            }
            let policy = db.policy()?;
            if (value && policy.max_favorites == 0) || (!value && policy.max_entries == 0) {
                return Err(invalid_data("Requested history category has zero capacity"));
            }
            let revision = db.bump()?;
            db.exec(
                "UPDATE entries SET favorite=?1,revision=?2 WHERE id=?3 AND revision=?4",
                params![i64::from(value), revision as i64, id, expected as i64],
            )?;
            db.retain(&policy, Some(id))?;
            Ok(())
        })
    }
    pub(super) fn remove_many(&mut self, targets: &[EntryTarget]) -> BackendResult<()> {
        if targets.is_empty() {
            return Ok(());
        }
        let values = (0..targets.len())
            .map(|index| format!("(?{},?{})", index * 2 + 1, index * 2 + 2))
            .collect::<Vec<_>>()
            .join(",");
        let cte = format!("WITH targets(id,revision) AS (VALUES {values})");
        let bindings = targets
            .iter()
            .flat_map(|target| {
                [
                    turso::Value::Text(target.opaque_id.clone()),
                    turso::Value::Integer(target.expected_revision as i64),
                ]
            })
            .collect::<Vec<_>>();
        self.transaction(|db| {
            let valid=db.scalar(&format!("{cte} SELECT count(*) FROM targets t JOIN entries e ON e.id=t.id AND e.revision=t.revision"),bindings.clone())?;
            if valid!=targets.len() as i64 {
                let present=db.scalar(&format!("{cte} SELECT count(*) FROM targets t JOIN entries e ON e.id=t.id"),bindings.clone())?;
                return Err(if present!=targets.len() as i64 {BackendError::not_found("Clipboard entry does not exist")} else {BackendError::stale("Clipboard entry revision changed")});
            }
            db.exec(&format!("{cte} DELETE FROM entries WHERE id IN (SELECT id FROM targets)"),bindings)?;
            db.bump()?;
            Ok(())
        })
    }
    pub(super) fn wipe(&mut self) -> BackendResult<()> {
        self.transaction(|db| {
            db.exec("DELETE FROM entries", ())?;
            db.exec("DELETE FROM inline_echoes", ())?;
            db.bump()?;
            Ok(())
        })
    }
    // All eviction decisions are SQL metadata queries. Favorites are evicted last
    // for the global byte budget; category limits are independent. The mutation
    // target is protected, or the transaction fails rather than losing its result.
    fn retain(&self, policy: &StorePolicy, protected: Option<&str>) -> BackendResult<()> {
        let protected = protected.unwrap_or("");
        self.exec(
            "DELETE FROM entries WHERE byte_size>?1 AND id<>?2",
            params![policy.max_entry_bytes as i64, protected],
        )?;
        for (favorite, limit) in [(0, policy.max_entries), (1, policy.max_favorites)] {
            self.exec("DELETE FROM entries WHERE id IN (SELECT id FROM entries WHERE favorite=?1 ORDER BY (id=?2) DESC,recency DESC,id LIMIT -1 OFFSET ?3)",params![favorite,protected,i64::from(limit)])?;
        }
        // Running total supports set-based byte retention without payload reads.
        self.exec("DELETE FROM entries WHERE id IN (SELECT id FROM (SELECT id,sum(byte_size) OVER (ORDER BY (id=?1) DESC,favorite DESC,recency DESC,id ROWS UNBOUNDED PRECEDING) AS running FROM entries) WHERE running>?2)",params![protected,policy.max_total_bytes as i64])?;
        if !protected.is_empty() {
            self.summary(protected, None)?;
        }
        Ok(())
    }
    pub(super) fn artifacts(&self) -> BackendResult<Vec<ArtifactRecord>> {
        self.rows("SELECT path,source_entry_id,image_identity,created_at,device,inode,owner FROM artifacts ORDER BY path",())?.iter().map(decode_artifact).collect()
    }
    pub(super) fn artifact_get(&self, path: &str) -> BackendResult<Option<ArtifactRecord>> {
        self.rows("SELECT path,source_entry_id,image_identity,created_at,device,inode,owner FROM artifacts WHERE path=?1",[path])?.first().map(decode_artifact).transpose()
    }
    pub(super) fn artifact_candidates(
        &self,
        before: u64,
        limit: usize,
    ) -> BackendResult<Vec<ArtifactRecord>> {
        self.rows("SELECT a.path,a.source_entry_id,a.image_identity,a.created_at,a.device,a.inode,a.owner FROM artifacts a WHERE a.created_at<=?1 AND NOT EXISTS (SELECT 1 FROM entries e WHERE e.id=a.source_entry_id) AND NOT EXISTS (SELECT 1 FROM entry_artifacts ea WHERE ea.path=a.path) ORDER BY a.created_at,a.path LIMIT ?2",params![before.min(i64::MAX as u64) as i64,limit.min(4096) as i64])?.iter().map(decode_artifact).collect()
    }
    pub(super) fn register_artifact(&mut self, record: ArtifactRecord) -> BackendResult<()> {
        let path = record
            .path
            .to_str()
            .ok_or_else(|| invalid_data("Artifact path must be UTF-8"))?;
        if !Path::new(path).is_absolute()
            || path.len() > 32768
            || record.image_identity.len() > 256
            || record.source_entry_id.len() > 128
            || record.created_at > i64::MAX as u64
        {
            return Err(invalid_data("Invalid artifact metadata"));
        }
        self.transaction(|db| {
            if db.scalar("SELECT count(*) FROM artifacts",())?>=4096 && db.scalar("SELECT count(*) FROM artifacts WHERE path=?1",[path])?==0 { return Err(invalid_data("Artifact registry is full")); }
            db.exec("INSERT INTO artifacts(path,source_entry_id,image_identity,created_at,device,inode,owner) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(path) DO UPDATE SET source_entry_id=excluded.source_entry_id,image_identity=excluded.image_identity,created_at=excluded.created_at,device=excluded.device,inode=excluded.inode,owner=excluded.owner",params![path,record.source_entry_id,record.image_identity,record.created_at as i64,record.device as i64,record.inode as i64,i64::from(record.owner)])?; Ok(())
        })
    }
    pub(super) fn forget_artifact(&mut self, path: &Path) -> BackendResult<()> {
        let path = path
            .to_str()
            .ok_or_else(|| invalid_data("Artifact path must be UTF-8"))?;
        self.transaction(|db| {
            db.exec("DELETE FROM artifacts WHERE path=?1", [path])?;
            Ok(())
        })
    }
    pub(super) fn artifact_references(&self) -> BackendResult<HashSet<PathBuf>> {
        self.rows("SELECT a.path FROM artifacts a WHERE EXISTS (SELECT 1 FROM entries e WHERE e.id=a.source_entry_id) OR EXISTS (SELECT 1 FROM entry_artifacts ea WHERE ea.path=a.path)",())?.iter().map(|r|text(r,0).map(PathBuf::from)).collect()
    }
    pub(super) fn inline_echoes(&self) -> BackendResult<Vec<InlineEchoRecord>> {
        self.rows("SELECT source_entry_id,image_identity,created_at FROM inline_echoes ORDER BY image_identity",())?.iter().map(|r|Ok(InlineEchoRecord {source_entry_id:text(r,0)?,image_identity:text(r,1)?,created_at:integer(r,2)? as u64})).collect()
    }
    pub(super) fn register_inline_echo(&mut self, record: InlineEchoRecord) -> BackendResult<()> {
        if record.image_identity.len() > 256
            || record.source_entry_id.len() > 128
            || record.created_at > i64::MAX as u64
        {
            return Err(invalid_data("Invalid inline echo metadata"));
        }
        self.transaction(|db|{
            db.exec("INSERT INTO inline_echoes(image_identity,source_entry_id,created_at) VALUES(?1,?2,?3) ON CONFLICT(image_identity) DO UPDATE SET source_entry_id=excluded.source_entry_id,created_at=excluded.created_at",params![record.image_identity,record.source_entry_id,record.created_at as i64])?;
            db.exec("DELETE FROM inline_echoes WHERE image_identity IN (SELECT image_identity FROM inline_echoes ORDER BY created_at DESC,image_identity LIMIT -1 OFFSET 4096)",())?; Ok(())
        })
    }
    pub(super) fn clear_inline_echoes(&mut self) -> BackendResult<()> {
        self.transaction(|db| {
            db.exec("DELETE FROM inline_echoes", ())?;
            Ok(())
        })
    }
    pub(super) fn shutdown(&mut self) -> BackendResult<()> {
        if self.blocked {
            return Err(BackendError::unavailable(
                "Fenced history storage cannot confirm clean shutdown",
            ));
        }
        self.transaction(|db| {
            db.exec("UPDATE effective_policy SET admission=0 WHERE id=1", ())?;
            Ok(())
        })?;
        let rows = self.rows("PRAGMA wal_checkpoint(TRUNCATE)", ())?;
        let row = rows
            .first()
            .ok_or_else(|| BackendError::unavailable("History checkpoint returned no status"))?;
        if integer(row, 0)? != 0 || integer(row, 1)? != integer(row, 2)? {
            return Err(BackendError::unavailable(
                "History checkpoint was incomplete",
            ));
        }
        protect_sidecars(&self.path)
    }
}
fn search_gram(needle: &str) -> Option<String> {
    // Every selected trigram is necessary for a literal match. Intersect a
    // bounded set, rather than picking a common prefix that selects the entire
    // catalog. Only generated, quoted alphanumeric terms reach the FTS parser.
    let mut terms = std::collections::BTreeSet::new();
    for word in needle.split(|c: char| !c.is_alphanumeric()) {
        let chars: Vec<_> = word.chars().collect();
        for gram in chars.windows(3) {
            terms.insert(gram.iter().collect::<String>());
            if terms.len() == 32 {
                break;
            }
        }
        if terms.len() == 32 {
            break;
        }
    }
    (!terms.is_empty()).then(|| {
        terms
            .into_iter()
            .map(|term| format!("\"{term}\""))
            .collect::<Vec<_>>()
            .join(" AND ")
    })
}
fn normalize_schema(sql: &str) -> String {
    let mut quoted = false;
    sql.trim()
        .trim_end_matches(';')
        .chars()
        .filter(|&c| {
            if c == '\'' {
                quoted = !quoted;
            }
            quoted || !c.is_whitespace()
        })
        .collect()
}
fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}
fn text(row: &Row, index: usize) -> BackendResult<String> {
    row.get(index).map_err(operation_error)
}
fn integer(row: &Row, index: usize) -> BackendResult<i64> {
    row.get(index).map_err(operation_error)
}
fn decode_artifact(row: &Row) -> BackendResult<ArtifactRecord> {
    Ok(ArtifactRecord {
        path: PathBuf::from(text(row, 0)?),
        source_entry_id: text(row, 1)?,
        image_identity: text(row, 2)?,
        created_at: integer(row, 3)? as u64,
        device: integer(row, 4)? as u64,
        inode: integer(row, 5)? as u64,
        owner: integer(row, 6)? as u32,
    })
}
fn decode_summary(row: &Row) -> BackendResult<EntrySummary> {
    let kind = match text(row, 2)?.as_str() {
        "Text" => EntryKind::Text,
        "Link" => EntryKind::Link,
        "Image" => EntryKind::Image,
        "Files" => EntryKind::Files,
        "Html" => EntryKind::Html,
        "Json" => EntryKind::Json,
        "Color" => EntryKind::Color,
        "Binary" => EntryKind::Binary,
        _ => return Err(invalid_data("Invalid stored entry kind")),
    };
    Ok(EntrySummary {
        id: text(row, 0)?,
        revision: integer(row, 1)? as u64,
        kind,
        mime: text(row, 3)?,
        byte_size: integer(row, 4)? as u64,
        favorite: integer(row, 5)? != 0,
        preview: text(row, 6)?,
        current: integer(row, 7)? != 0,
    })
}

#[cfg(test)]
mod failure_tests {
    use super::*;
    #[test]
    fn dangling_sidecar_after_commit_fences_storage() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.db");
        let mut db = Database::open(&path).unwrap();
        std::os::unix::fs::symlink("missing", sidecar(&path, "-shm")).unwrap();
        assert!(db.transaction(|_| Ok(())).is_err());
        assert!(db.blocked);
    }
    #[test]
    fn ambiguous_commit_fences_but_recovery_observes_durable_revision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.db");
        let mut db = Database::open(&path).unwrap();
        // Commit reaches the real engine, then the outer commit loses its valid
        // transaction: models an acknowledged storage write with a lost result.
        let result = db.transaction(|db| {
            db.bump()?;
            db.exec("COMMIT", ())?;
            Ok(())
        });
        assert!(result.is_err());
        assert!(db.blocked);
        assert!(db.apply_policy(StorePolicy::default()).is_err());
        drop(db);
        let mut db = Database::open(&path).unwrap();
        assert_eq!(db.revision().unwrap(), 1);
        db.shutdown().unwrap();
    }
    #[test]
    fn failed_rollback_fences_and_does_not_publish_uncommitted_revision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.db");
        let mut db = Database::open(&path).unwrap();
        let result: BackendResult<()> = db.transaction(|db| {
            db.bump()?;
            db.exec("ROLLBACK", ())?;
            Err(invalid_data("injected mutation failure"))
        });
        assert!(result.is_err());
        assert!(db.blocked);
        drop(db);
        let mut db = Database::open(&path).unwrap();
        assert_eq!(db.revision().unwrap(), 0);
        db.shutdown().unwrap();
    }
    #[test]
    fn rollback_restores_payload_search_index_and_catalog_revision() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Database::open(&dir.path().join("history.db")).unwrap();
        db.apply_policy(StorePolicy {
            admission: true,
            ..StorePolicy::default()
        })
        .unwrap();
        let entry = db
            .insert("text/plain", b"searchable rollback needle")
            .unwrap();
        let revision = db.revision().unwrap();
        let result: BackendResult<()> = db.transaction(|db| {
            db.exec("DELETE FROM entries WHERE id=?1", [entry.id.as_str()])?;
            db.bump()?;
            Err(invalid_data("injected failure after indexed delete"))
        });
        assert!(result.is_err());
        assert!(!db.blocked);
        assert_eq!(db.revision().unwrap(), revision);
        assert_eq!(
            db.get(&entry.id, None).unwrap().bytes,
            b"searchable rollback needle"
        );
        let page = db
            .query(HistoryQuery {
                query: "needle".into(),
                generation: 0,
                offset: 0,
                limit: 10,
                collapse_self_echoes: false,
            })
            .unwrap();
        assert_eq!(page.total, 1);
        db.shutdown().unwrap();
    }
    #[test]
    fn constraints_and_index_plans_are_enforced_by_real_engine() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.db");
        let mut db = Database::open(&path).unwrap();
        assert!(
            db.exec("UPDATE effective_policy SET max_entry_bytes=67108865", ())
                .is_err()
        );
        assert!(
            db.exec("INSERT INTO payloads VALUES('missing',x'00')", ())
                .is_err()
        );
        let plan = db
            .rows(
                "EXPLAIN QUERY PLAN SELECT id FROM entries ORDER BY recency DESC,id LIMIT 10",
                (),
            )
            .unwrap();
        assert!(
            plan.iter()
                .any(|r| text(r, 3).unwrap().contains("entries_order"))
        );
        let plan=db.rows("EXPLAIN QUERY PLAN SELECT entry_id FROM search_content WHERE fts_match(indexed_content,'uni')",()).unwrap();
        let details = plan.iter().map(|r| text(r, 3).unwrap()).collect::<Vec<_>>();
        assert!(
            details.iter().any(|p| p.contains("QUERY INDEX METHOD fts")),
            "{details:?}"
        );
        db.shutdown().unwrap();
    }
}
