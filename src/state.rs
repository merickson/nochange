//! Schema-versioned SQLite synchronization state and per-account locks.

use crate::model::{FollowUpState, MessageFlags};
use fs4::{FileExt, TryLockError};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use thiserror::Error;

const SCHEMA_VERSION: u32 = 3;

/// Persisted remote-folder baseline and checkpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredFolder {
    /// Immutable Microsoft Graph folder ID.
    pub id: String,
    /// Immutable parent folder ID, when the folder is nested.
    pub parent_id: Option<String>,
    /// Current Graph display name.
    pub display_name: String,
    /// Complete `/`-delimited remote path.
    pub remote_path: String,
    /// Account-root-relative encoded Maildir path.
    pub local_path: String,
    /// Whether this folder is selected by configuration.
    pub is_selected: bool,
    /// Whether Graph marks this as a hidden folder.
    pub is_hidden: bool,
    /// Last observed count of all Outlook items directly in the folder.
    pub total_item_count: u32,
    /// Last completely applied message delta URL.
    pub message_delta_link: Option<String>,
}

/// Persisted message baseline used for replay and conflict detection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredMessage {
    /// Immutable Microsoft Graph message ID.
    pub id: String,
    /// Immutable containing folder ID.
    pub folder_id: String,
    /// Deterministic filesystem-safe Maildir basename.
    pub maildir_key: String,
    /// Account-root-relative path to the delivered message.
    pub relative_path: String,
    /// SHA-256 digest of the synchronized MIME bytes.
    pub mime_hash: String,
    /// Opaque Graph modification value used to recognize delta replays.
    pub remote_version: String,
    /// RFC Internet Message-ID, when Graph supplied one.
    pub internet_message_id: Option<String>,
    /// Last synchronized flags.
    pub flags: MessageFlags,
}

/// Durable local flag mutation awaiting submission or its matching delta echo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingFlagOperation {
    /// Immutable Microsoft Graph message ID.
    pub message_id: String,
    /// Desired supported Graph flags.
    pub flags: MessageFlags,
    /// Current local Maildir path containing those flags.
    pub relative_path: String,
    /// Whether Graph accepted the mutation request.
    pub submitted: bool,
}

/// Open, migrated synchronization-state database.
pub struct StateDatabase {
    connection: Connection,
}

impl StateDatabase {
    /// Open or create a private SQLite database and apply known migrations.
    pub fn open(path: &Path) -> Result<Self, StateError> {
        Self::open_with_fsync(path, true)
    }

    /// Open the state database with full or explicitly disabled fsync durability.
    pub fn open_with_fsync(path: &Path, fsync_enabled: bool) -> Result<Self, StateError> {
        let parent = path.parent().ok_or(StateError::InvalidPath)?;
        fs::create_dir_all(parent)?;
        create_private_file(path)?;
        let mut connection = Connection::open(path)?;
        connection.pragma_update(None, "foreign_keys", true)?;
        let journal_mode: String =
            connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") {
            connection.pragma_update(None, "journal_mode", "WAL")?;
        }
        let synchronous = if fsync_enabled { "FULL" } else { "OFF" };
        connection.pragma_update(None, "synchronous", synchronous)?;
        migrate_database(&mut connection)?;
        Ok(Self { connection })
    }

    /// Return the schema version understood by the open database.
    pub fn get_schema_version(&self) -> Result<u32, StateError> {
        self.connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(StateError::from)
    }

    /// Return SQLite's active journal mode for validation and diagnostics.
    pub fn get_journal_mode(&self) -> Result<String, StateError> {
        self.connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .map_err(StateError::from)
    }

    /// Return SQLite's numeric synchronous level for diagnostics.
    pub fn get_synchronous_level(&self) -> Result<u32, StateError> {
        self.connection
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .map_err(StateError::from)
    }

    /// Create an account baseline or verify its case-insensitive user identity.
    pub fn ensure_account(&mut self, account: &str, user: &str) -> Result<(), StateError> {
        let transaction = self.connection.transaction()?;
        let existing: Option<String> = transaction
            .query_row(
                "SELECT user_identity FROM accounts WHERE name = ?1",
                [account],
                |row| row.get(0),
            )
            .optional()?;
        match existing {
            Some(identity) if !identity.eq_ignore_ascii_case(user) => {
                return Err(StateError::AccountIdentityMismatch {
                    account: account.to_owned(),
                    expected: identity,
                    actual: user.to_owned(),
                });
            }
            Some(_) => {}
            None => {
                transaction.execute(
                    "INSERT INTO accounts(name, user_identity) VALUES (?1, ?2)",
                    params![account, user],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Load the last completely applied mailbox-folder delta URL.
    pub fn get_folder_delta_link(&self, account: &str) -> Result<Option<String>, StateError> {
        self.connection
            .query_row(
                "SELECT folder_delta_link FROM accounts WHERE name = ?1",
                [account],
                |row| row.get(0),
            )
            .optional()
            .map(|value| value.flatten())
            .map_err(StateError::from)
    }

    /// Replace the mailbox-folder checkpoint after a complete applied round.
    pub fn set_folder_delta_link(
        &mut self,
        account: &str,
        delta_link: &str,
    ) -> Result<(), StateError> {
        let changed = self.connection.execute(
            "UPDATE accounts SET folder_delta_link = ?2 WHERE name = ?1",
            params![account, delta_link],
        )?;
        if changed == 1 {
            Ok(())
        } else {
            Err(StateError::UnknownAccount(account.to_owned()))
        }
    }

    /// Insert or replace a folder baseline without interpreting its paths.
    pub fn upsert_folder(
        &mut self,
        account: &str,
        folder: &StoredFolder,
    ) -> Result<(), StateError> {
        self.connection.execute(
            "INSERT INTO folders(
                account_name, id, parent_id, display_name, remote_path, local_path,
                is_selected, is_hidden, total_item_count, message_delta_link
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(account_name, id) DO UPDATE SET
                parent_id = excluded.parent_id,
                display_name = excluded.display_name,
                remote_path = excluded.remote_path,
                local_path = excluded.local_path,
                is_selected = excluded.is_selected,
                is_hidden = excluded.is_hidden,
                total_item_count = excluded.total_item_count,
                message_delta_link = excluded.message_delta_link",
            params![
                account,
                folder.id,
                folder.parent_id,
                folder.display_name,
                folder.remote_path,
                folder.local_path,
                folder.is_selected,
                folder.is_hidden,
                folder.total_item_count,
                folder.message_delta_link,
            ],
        )?;
        Ok(())
    }

    /// Load one folder baseline.
    pub fn get_folder(
        &self,
        account: &str,
        folder_id: &str,
    ) -> Result<Option<StoredFolder>, StateError> {
        self.connection
            .query_row(
                "SELECT id, parent_id, display_name, remote_path, local_path,
                        is_selected, is_hidden, total_item_count, message_delta_link
                 FROM folders WHERE account_name = ?1 AND id = ?2",
                params![account, folder_id],
                map_stored_folder,
            )
            .optional()
            .map_err(StateError::from)
    }

    /// List folder baselines in deterministic remote-path order.
    pub fn list_folders(&self, account: &str) -> Result<Vec<StoredFolder>, StateError> {
        let mut statement = self.connection.prepare(
            "SELECT id, parent_id, display_name, remote_path, local_path,
                    is_selected, is_hidden, total_item_count, message_delta_link
             FROM folders WHERE account_name = ?1 ORDER BY remote_path, id",
        )?;
        let folders = statement
            .query_map([account], map_stored_folder)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(folders)
    }

    /// Delete a folder baseline and its message baselines idempotently.
    pub fn delete_folder(&mut self, account: &str, folder_id: &str) -> Result<(), StateError> {
        self.connection.execute(
            "DELETE FROM folders WHERE account_name = ?1 AND id = ?2",
            params![account, folder_id],
        )?;
        Ok(())
    }

    /// Replace one folder's message checkpoint without changing other metadata.
    pub fn set_message_delta_link(
        &mut self,
        account: &str,
        folder_id: &str,
        delta_link: &str,
    ) -> Result<(), StateError> {
        let changed = self.connection.execute(
            "UPDATE folders SET message_delta_link = ?3
             WHERE account_name = ?1 AND id = ?2",
            params![account, folder_id, delta_link],
        )?;
        if changed == 1 {
            Ok(())
        } else {
            Err(StateError::UnknownFolder(folder_id.to_owned()))
        }
    }

    /// Insert or replace a synchronized message baseline.
    pub fn upsert_message(
        &mut self,
        account: &str,
        message: &StoredMessage,
    ) -> Result<(), StateError> {
        self.connection.execute(
            "INSERT INTO messages(
                account_name, id, folder_id, maildir_key, relative_path, mime_hash,
                remote_version, internet_message_id, is_read, is_flagged
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(account_name, id) DO UPDATE SET
                folder_id = excluded.folder_id,
                maildir_key = excluded.maildir_key,
                relative_path = excluded.relative_path,
                mime_hash = excluded.mime_hash,
                remote_version = excluded.remote_version,
                internet_message_id = excluded.internet_message_id,
                is_read = excluded.is_read,
                is_flagged = excluded.is_flagged",
            params![
                account,
                message.id,
                message.folder_id,
                message.maildir_key,
                message.relative_path,
                message.mime_hash,
                message.remote_version,
                message.internet_message_id,
                message.flags.is_read,
                message.flags.follow_up == FollowUpState::Flagged,
            ],
        )?;
        Ok(())
    }

    /// Load one message baseline by immutable ID.
    pub fn get_message(
        &self,
        account: &str,
        message_id: &str,
    ) -> Result<Option<StoredMessage>, StateError> {
        self.connection
            .query_row(
                "SELECT id, folder_id, maildir_key, relative_path, mime_hash,
                        remote_version, internet_message_id, is_read, is_flagged
                 FROM messages WHERE account_name = ?1 AND id = ?2",
                params![account, message_id],
                map_stored_message,
            )
            .optional()
            .map_err(StateError::from)
    }

    /// List message baselines for a folder in deterministic ID order.
    pub fn list_messages(
        &self,
        account: &str,
        folder_id: &str,
    ) -> Result<Vec<StoredMessage>, StateError> {
        let mut statement = self.connection.prepare(
            "SELECT id, folder_id, maildir_key, relative_path, mime_hash,
                    remote_version, internet_message_id, is_read, is_flagged
             FROM messages
             WHERE account_name = ?1 AND folder_id = ?2
             ORDER BY id",
        )?;
        let messages = statement
            .query_map(params![account, folder_id], map_stored_message)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(messages)
    }

    /// Delete one message baseline idempotently.
    pub fn delete_message(&mut self, account: &str, message_id: &str) -> Result<(), StateError> {
        self.connection.execute(
            "DELETE FROM messages WHERE account_name = ?1 AND id = ?2",
            params![account, message_id],
        )?;
        Ok(())
    }

    /// Insert or replace a desired local flag mutation before contacting Graph.
    pub fn upsert_pending_flag_operation(
        &mut self,
        account: &str,
        operation: &PendingFlagOperation,
    ) -> Result<(), StateError> {
        self.connection.execute(
            "INSERT INTO pending_operations(
                account_name, message_id, operation_kind, desired_is_read,
                desired_is_flagged, local_relative_path, submitted
             ) VALUES (?1, ?2, 'flags', ?3, ?4, ?5, ?6)
             ON CONFLICT(account_name, message_id, operation_kind) DO UPDATE SET
                desired_is_read = excluded.desired_is_read,
                desired_is_flagged = excluded.desired_is_flagged,
                local_relative_path = excluded.local_relative_path,
                submitted = excluded.submitted",
            params![
                account,
                operation.message_id,
                operation.flags.is_read,
                operation.flags.follow_up == FollowUpState::Flagged,
                operation.relative_path,
                operation.submitted,
            ],
        )?;
        Ok(())
    }

    /// List pending flag mutations in deterministic message-ID order.
    pub fn list_pending_flag_operations(
        &self,
        account: &str,
    ) -> Result<Vec<PendingFlagOperation>, StateError> {
        let mut statement = self.connection.prepare(
            "SELECT message_id, desired_is_read, desired_is_flagged,
                    local_relative_path, submitted
             FROM pending_operations
             WHERE account_name = ?1 AND operation_kind = 'flags'
             ORDER BY message_id",
        )?;
        let operations = statement
            .query_map([account], |row| {
                let is_flagged: bool = row.get(2)?;
                Ok(PendingFlagOperation {
                    message_id: row.get(0)?,
                    flags: MessageFlags {
                        is_read: row.get(1)?,
                        follow_up: if is_flagged {
                            FollowUpState::Flagged
                        } else {
                            FollowUpState::NotFlagged
                        },
                    },
                    relative_path: row.get(3)?,
                    submitted: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(operations)
    }

    /// Mark a journaled flag mutation as accepted by Graph.
    pub fn mark_pending_flag_operation_submitted(
        &mut self,
        account: &str,
        message_id: &str,
    ) -> Result<(), StateError> {
        let changed = self.connection.execute(
            "UPDATE pending_operations SET submitted = 1
             WHERE account_name = ?1 AND message_id = ?2 AND operation_kind = 'flags'",
            params![account, message_id],
        )?;
        if changed == 1 {
            Ok(())
        } else {
            Err(StateError::UnknownPendingOperation)
        }
    }

    /// Delete a completed flag mutation idempotently.
    pub fn delete_pending_flag_operation(
        &mut self,
        account: &str,
        message_id: &str,
    ) -> Result<(), StateError> {
        self.connection.execute(
            "DELETE FROM pending_operations
             WHERE account_name = ?1 AND message_id = ?2 AND operation_kind = 'flags'",
            params![account, message_id],
        )?;
        Ok(())
    }
}

/// Exclusive per-account interprocess lock guard.
pub struct AccountLock {
    _file: File,
}

impl AccountLock {
    /// Create the lock file privately and acquire it without blocking.
    pub fn acquire(path: &Path) -> Result<Self, StateError> {
        let parent = path.parent().ok_or(StateError::InvalidPath)?;
        fs::create_dir_all(parent)?;
        let file = open_private_lock_file(path)?;
        match FileExt::try_lock(&file) {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => Err(StateError::AccountLocked),
            Err(TryLockError::Error(error)) => Err(StateError::Io(error)),
        }
    }
}

/// Return a stable lock path without exposing the configured account name.
pub fn get_account_lock_path(state_root: &Path, account: &str) -> PathBuf {
    let digest = Sha256::digest(account.as_bytes());
    state_root.join(format!("account-{}.lock", hex::encode(digest)))
}

/// Synchronization-state and lock failures.
#[derive(Debug, Error)]
pub enum StateError {
    /// Filesystem state could not be created or inspected.
    #[error("could not access synchronization state")]
    Io(#[from] std::io::Error),
    /// SQLite rejected a schema or data operation.
    #[error("could not update synchronization state")]
    Database(#[from] rusqlite::Error),
    /// The state path did not have a usable parent directory.
    #[error("synchronization state path is invalid")]
    InvalidPath,
    /// A newer application created a schema this build cannot safely read.
    #[error("state database schema version {0} is newer than this Nochange build")]
    UnsupportedSchemaVersion(u32),
    /// A configured account name was absent from state.
    #[error("account '{0}' has not been initialized in synchronization state")]
    UnknownAccount(String),
    /// A Graph folder was absent from state.
    #[error("mail folder '{0}' has not been initialized in synchronization state")]
    UnknownFolder(String),
    /// Existing state belongs to a different Microsoft 365 identity.
    #[error("account '{account}' state belongs to '{expected}', not configured user '{actual}'")]
    AccountIdentityMismatch {
        /// Local account name.
        account: String,
        /// Identity pinned in state.
        expected: String,
        /// Current configured identity.
        actual: String,
    },
    /// Another process currently holds the selected account lock.
    #[error("another Nochange process is already synchronizing this account")]
    AccountLocked,
    /// A pending operation disappeared before its lifecycle update.
    #[error("a pending synchronization operation was not found")]
    UnknownPendingOperation,
}

fn create_private_file(path: &Path) -> Result<(), StateError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(0o600);
        let file = options.open(path)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        let _file = options.open(path)?;
    }
    Ok(())
}

fn open_private_lock_file(path: &Path) -> Result<File, StateError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(StateError::from)
}

fn migrate_database(connection: &mut Connection) -> Result<(), StateError> {
    let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(StateError::UnsupportedSchemaVersion(version));
    }
    if version == 0 {
        let transaction = connection.transaction()?;
        transaction.execute_batch(
            "CREATE TABLE accounts (
                name TEXT PRIMARY KEY NOT NULL,
                user_identity TEXT NOT NULL,
                folder_delta_link TEXT
             );
             CREATE TABLE folders (
                account_name TEXT NOT NULL,
                id TEXT NOT NULL,
                parent_id TEXT,
                display_name TEXT NOT NULL,
                remote_path TEXT NOT NULL,
                local_path TEXT NOT NULL,
                is_selected INTEGER NOT NULL CHECK (is_selected IN (0, 1)),
                is_hidden INTEGER NOT NULL CHECK (is_hidden IN (0, 1)),
                total_item_count INTEGER NOT NULL CHECK (total_item_count >= 0),
                message_delta_link TEXT,
                PRIMARY KEY (account_name, id),
                FOREIGN KEY (account_name) REFERENCES accounts(name) ON DELETE CASCADE
             );
             CREATE TABLE messages (
                account_name TEXT NOT NULL,
                id TEXT NOT NULL,
                folder_id TEXT NOT NULL,
                maildir_key TEXT NOT NULL,
                relative_path TEXT NOT NULL,
                mime_hash TEXT NOT NULL,
                remote_version TEXT NOT NULL,
                internet_message_id TEXT,
                is_read INTEGER NOT NULL CHECK (is_read IN (0, 1)),
                is_flagged INTEGER NOT NULL CHECK (is_flagged IN (0, 1)),
                PRIMARY KEY (account_name, id),
                UNIQUE (account_name, maildir_key),
                FOREIGN KEY (account_name, folder_id)
                    REFERENCES folders(account_name, id) ON DELETE CASCADE
             );
             CREATE TABLE pending_operations (
                account_name TEXT NOT NULL,
                message_id TEXT NOT NULL,
                operation_kind TEXT NOT NULL CHECK (operation_kind = 'flags'),
                desired_is_read INTEGER NOT NULL CHECK (desired_is_read IN (0, 1)),
                desired_is_flagged INTEGER NOT NULL CHECK (desired_is_flagged IN (0, 1)),
                local_relative_path TEXT NOT NULL,
                submitted INTEGER NOT NULL CHECK (submitted IN (0, 1)),
                PRIMARY KEY (account_name, message_id, operation_kind),
                FOREIGN KEY (account_name, message_id)
                    REFERENCES messages(account_name, id) ON DELETE CASCADE
             );
             PRAGMA user_version = 3;",
        )?;
        transaction.commit()?;
    } else if version == 1 {
        let transaction = connection.transaction()?;
        transaction.execute_batch(
            "ALTER TABLE folders
                ADD COLUMN total_item_count INTEGER NOT NULL DEFAULT 0
                CHECK (total_item_count >= 0);
             CREATE TABLE pending_operations (
                account_name TEXT NOT NULL,
                message_id TEXT NOT NULL,
                operation_kind TEXT NOT NULL CHECK (operation_kind = 'flags'),
                desired_is_read INTEGER NOT NULL CHECK (desired_is_read IN (0, 1)),
                desired_is_flagged INTEGER NOT NULL CHECK (desired_is_flagged IN (0, 1)),
                local_relative_path TEXT NOT NULL,
                submitted INTEGER NOT NULL CHECK (submitted IN (0, 1)),
                PRIMARY KEY (account_name, message_id, operation_kind),
                FOREIGN KEY (account_name, message_id)
                    REFERENCES messages(account_name, id) ON DELETE CASCADE
             );
             PRAGMA user_version = 3;",
        )?;
        transaction.commit()?;
    } else if version == 2 {
        let transaction = connection.transaction()?;
        transaction.execute_batch(
            "CREATE TABLE pending_operations (
                account_name TEXT NOT NULL,
                message_id TEXT NOT NULL,
                operation_kind TEXT NOT NULL CHECK (operation_kind = 'flags'),
                desired_is_read INTEGER NOT NULL CHECK (desired_is_read IN (0, 1)),
                desired_is_flagged INTEGER NOT NULL CHECK (desired_is_flagged IN (0, 1)),
                local_relative_path TEXT NOT NULL,
                submitted INTEGER NOT NULL CHECK (submitted IN (0, 1)),
                PRIMARY KEY (account_name, message_id, operation_kind),
                FOREIGN KEY (account_name, message_id)
                    REFERENCES messages(account_name, id) ON DELETE CASCADE
             );
             PRAGMA user_version = 3;",
        )?;
        transaction.commit()?;
    }
    Ok(())
}

fn map_stored_folder(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredFolder> {
    Ok(StoredFolder {
        id: row.get(0)?,
        parent_id: row.get(1)?,
        display_name: row.get(2)?,
        remote_path: row.get(3)?,
        local_path: row.get(4)?,
        is_selected: row.get(5)?,
        is_hidden: row.get(6)?,
        total_item_count: row.get(7)?,
        message_delta_link: row.get(8)?,
    })
}

fn map_stored_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredMessage> {
    let is_flagged: bool = row.get(8)?;
    Ok(StoredMessage {
        id: row.get(0)?,
        folder_id: row.get(1)?,
        maildir_key: row.get(2)?,
        relative_path: row.get(3)?,
        mime_hash: row.get(4)?,
        remote_version: row.get(5)?,
        internet_message_id: row.get(6)?,
        flags: MessageFlags {
            is_read: row.get(7)?,
            follow_up: if is_flagged {
                FollowUpState::Flagged
            } else {
                FollowUpState::NotFlagged
            },
        },
    })
}
