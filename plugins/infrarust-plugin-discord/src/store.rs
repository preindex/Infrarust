//! Link table between Mojang UUIDs and Discord user IDs, plus pending link codes.
//!
//! Invariants, all enforced under one lock:
//! - a Mojang UUID has at most one Discord link;
//! - a Discord user has at most one Mojang link;
//! - a pending code is consumed by exactly one successful redemption.
//!
//! Links are durable in `links.json`, written to a temporary file, synced and
//! renamed over the old one. A mutation is committed in memory only after the
//! write succeeds. Pending codes stay in memory: a restart invalidates them, and
//! the player gets a new code on the next join. They are never logged or written.
//!
//! Changing a link is explicit. A conflicting `/link` is refused rather than
//! replacing the existing link. The Discord owner removes their own link with
//! `/unlink`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, Notify};
use uuid::Uuid;

const FILE_NAME: &str = "links.json";
const FILE_VERSION: u32 = 1;
const CODE_LEN: usize = 6;
const CODE_SPACE: u128 = 1_000_000;

#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error("link store I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("link store is corrupt: {0}")]
    Corrupt(String),
    #[error("link store could not be serialized: {0}")]
    Serialize(String),
    #[error("this Discord account is already linked to a different Minecraft account")]
    DiscordAlreadyLinked,
    #[error("this Minecraft account is already linked to a different Discord account")]
    MinecraftAlreadyLinked,
    #[error("the link code is invalid or has expired")]
    InvalidCode,
    #[error("too many pending link codes")]
    TooManyPending,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkRecord {
    pub uuid: Uuid,
    pub discord_id: String,
    pub linked_at: u64,
}

#[derive(Serialize, Deserialize)]
struct FileFormat {
    version: u32,
    links: Vec<LinkRecord>,
}

#[derive(Clone)]
struct PendingCode {
    code: String,
    hash: [u8; 32],
    expires_at: Instant,
    linked: Arc<Notify>,
}

#[derive(Clone, Default)]
struct Book {
    by_uuid: HashMap<Uuid, LinkRecord>,
    by_discord: HashMap<String, Uuid>,
    pending_by_uuid: HashMap<Uuid, PendingCode>,
    pending_by_hash: HashMap<[u8; 32], Uuid>,
}

impl Book {
    fn insert_link(&mut self, record: LinkRecord) {
        self.by_discord
            .insert(record.discord_id.clone(), record.uuid);
        self.by_uuid.insert(record.uuid, record);
    }

    fn remove_link(&mut self, uuid: Uuid) {
        if let Some(record) = self.by_uuid.remove(&uuid) {
            self.by_discord.remove(&record.discord_id);
        }
    }

    fn remove_pending(&mut self, uuid: Uuid) {
        if let Some(pending) = self.pending_by_uuid.remove(&uuid) {
            self.pending_by_hash.remove(&pending.hash);
        }
    }

    fn prune_expired(&mut self, now: Instant) {
        let expired: Vec<Uuid> = self
            .pending_by_uuid
            .iter()
            .filter(|(_, pending)| pending.expires_at <= now)
            .map(|(uuid, _)| *uuid)
            .collect();
        for uuid in expired {
            self.remove_pending(uuid);
        }
    }

    fn file_format(&self) -> FileFormat {
        let mut links: Vec<LinkRecord> = self.by_uuid.values().cloned().collect();
        links.sort_by_key(|record| record.uuid);
        FileFormat {
            version: FILE_VERSION,
            links,
        }
    }
}

pub struct LinkBook {
    path: PathBuf,
    book: Mutex<Book>,
    code_ttl: Duration,
    max_pending: usize,
}

impl LinkBook {
    /// Opens `links.json` under `data_dir`. A missing file starts empty. A
    /// corrupt file is an error, so it is never silently overwritten.
    pub async fn open(
        data_dir: &Path,
        code_ttl: Duration,
        max_pending: usize,
    ) -> Result<Self, LinkError> {
        tokio::fs::create_dir_all(data_dir).await?;
        let path = data_dir.join(FILE_NAME);
        let book = match tokio::fs::read(&path).await {
            Ok(bytes) => load_book(&bytes)?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Book::default(),
            Err(err) => return Err(err.into()),
        };
        Ok(Self {
            path,
            book: Mutex::new(book),
            code_ttl,
            max_pending,
        })
    }

    pub async fn linked_discord(&self, uuid: Uuid) -> Option<String> {
        let book = self.book.lock().await;
        book.by_uuid
            .get(&uuid)
            .map(|record| record.discord_id.clone())
    }

    /// Discord IDs of every linked account, in no particular order.
    pub async fn linked_discord_ids(&self) -> Vec<String> {
        let book = self.book.lock().await;
        book.by_uuid
            .values()
            .map(|record| record.discord_id.clone())
            .collect()
    }

    /// Returns the Mojang UUID linked to a Discord user, if any.
    pub async fn linked_uuid(&self, discord_id: &str) -> Option<Uuid> {
        let book = self.book.lock().await;
        book.by_discord.get(discord_id).copied()
    }

    /// Returns the persisted links for the authenticated admin dashboard.
    pub async fn list_links(&self) -> Vec<LinkRecord> {
        let book = self.book.lock().await;
        let mut records = book.by_uuid.values().cloned().collect::<Vec<_>>();
        records.sort_by_key(|record| record.uuid);
        records
    }

    /// Waits for the pending code for `uuid` to become a persisted link.
    ///
    /// Registering the notification before reading the link table prevents a
    /// redemption between the read and wait from being missed. Dropping this
    /// future cancels the wait without retaining a session or lock.
    ///
    /// Returns false if the code was removed without a live link. Each pending
    /// code has its own signal so one redemption does not wake other players.
    pub async fn wait_until_linked(&self, uuid: Uuid, code: &str) -> bool {
        let linked = {
            let book = self.book.lock().await;
            if book.by_uuid.contains_key(&uuid) {
                return true;
            }
            let Some(pending) = book.pending_by_uuid.get(&uuid) else {
                return false;
            };
            if pending.code != code {
                return false;
            }
            Arc::clone(&pending.linked)
        };
        let changed = linked.notified();
        tokio::pin!(changed);
        let _ = changed.as_mut().enable();
        if self.linked_discord(uuid).await.is_some() {
            return true;
        }
        changed.await;
        self.linked_discord(uuid).await.is_some()
    }

    /// Returns the live pending code for an unlinked UUID, or issues one.
    ///
    /// Reusing the live code means repeated joins do not create a flood of
    /// codes. The `Instant` is the code's expiry.
    pub async fn pending_code(&self, uuid: Uuid) -> Result<(String, Instant), LinkError> {
        let mut book = self.book.lock().await;
        if book.by_uuid.contains_key(&uuid) {
            return Err(LinkError::MinecraftAlreadyLinked);
        }
        let now = Instant::now();
        book.prune_expired(now);
        if let Some(pending) = book.pending_by_uuid.get(&uuid) {
            return Ok((pending.code.clone(), pending.expires_at));
        }
        if book.pending_by_uuid.len() >= self.max_pending {
            return Err(LinkError::TooManyPending);
        }

        if book.pending_by_hash.len() as u128 >= CODE_SPACE {
            return Err(LinkError::TooManyPending);
        }

        // Keep the code short enough to type from the limbo display, while
        // ensuring two simultaneously pending players never share a code.
        let (code, hash) = loop {
            let code = format!("{:06}", Uuid::new_v4().as_u128() % CODE_SPACE);
            let hash = hash_code(&code);
            if !book.pending_by_hash.contains_key(&hash) {
                break (code, hash);
            }
        };
        let expires_at = now + self.code_ttl;
        book.pending_by_hash.insert(hash, uuid);
        book.pending_by_uuid.insert(
            uuid,
            PendingCode {
                code: code.clone(),
                hash,
                expires_at,
                linked: Arc::new(Notify::new()),
            },
        );
        Ok((code, expires_at))
    }

    /// Redeems a one-time code for `discord_id`, atomically.
    ///
    /// Malformed, unknown and expired codes all return `InvalidCode`. A
    /// conflicting link is refused and leaves the code unspent.
    pub async fn redeem(&self, code: &str, discord_id: &str) -> Result<Uuid, LinkError> {
        let normalized = normalize_code(code).ok_or(LinkError::InvalidCode)?;
        let hash = hash_code(&normalized);

        let mut guard = self.book.lock().await;
        guard.prune_expired(Instant::now());
        let uuid = guard
            .pending_by_hash
            .get(&hash)
            .copied()
            .ok_or(LinkError::InvalidCode)?;
        let linked = guard
            .pending_by_uuid
            .get(&uuid)
            .map(|pending| Arc::clone(&pending.linked))
            .ok_or(LinkError::InvalidCode)?;

        if let Some(existing) = guard.by_discord.get(discord_id) {
            if *existing != uuid {
                return Err(LinkError::DiscordAlreadyLinked);
            }
        }
        if let Some(record) = guard.by_uuid.get(&uuid) {
            if record.discord_id != discord_id {
                return Err(LinkError::MinecraftAlreadyLinked);
            }
        }

        let mut next = guard.clone();
        next.remove_pending(uuid);
        if !next.by_uuid.contains_key(&uuid) {
            next.insert_link(LinkRecord {
                uuid,
                discord_id: discord_id.to_owned(),
                linked_at: unix_now(),
            });
        }
        self.persist(&next).await?;
        *guard = next;
        drop(guard);
        linked.notify_waiters();
        Ok(uuid)
    }

    /// Removes the link for `discord_id` in both directions, atomically.
    pub async fn unlink(&self, discord_id: &str) -> Result<Option<Uuid>, LinkError> {
        let mut guard = self.book.lock().await;
        let Some(uuid) = guard.by_discord.get(discord_id).copied() else {
            return Ok(None);
        };
        let mut next = guard.clone();
        next.remove_link(uuid);
        self.persist(&next).await?;
        *guard = next;
        Ok(Some(uuid))
    }

    async fn persist(&self, book: &Book) -> Result<(), LinkError> {
        let bytes = serde_json::to_vec_pretty(&book.file_format())
            .map_err(|err| LinkError::Serialize(err.to_string()))?;
        let tmp = self.path.with_extension("json.tmp");
        let mut file = tokio::fs::File::create(&tmp).await?;
        file.write_all(&bytes).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&tmp, &self.path).await?;
        Ok(())
    }
}

fn load_book(bytes: &[u8]) -> Result<Book, LinkError> {
    let file: FileFormat =
        serde_json::from_slice(bytes).map_err(|err| LinkError::Corrupt(err.to_string()))?;
    if file.version != FILE_VERSION {
        return Err(LinkError::Corrupt(format!(
            "unsupported version {}",
            file.version
        )));
    }
    let mut book = Book::default();
    for record in file.links {
        if book.by_uuid.contains_key(&record.uuid)
            || book.by_discord.contains_key(&record.discord_id)
        {
            return Err(LinkError::Corrupt(
                "a UUID or Discord ID is linked more than once".into(),
            ));
        }
        book.insert_link(record);
    }
    Ok(book)
}

/// Accepts exactly six decimal digits, ignoring surrounding spaces.
fn normalize_code(code: &str) -> Option<String> {
    let trimmed = code.trim();
    (trimmed.len() == CODE_LEN && trimmed.bytes().all(|b| b.is_ascii_digit()))
        .then(|| trimmed.to_owned())
}

fn hash_code(code: &str) -> [u8; 32] {
    Sha256::digest(code.as_bytes()).into()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}
