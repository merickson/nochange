//! Safe Maildir naming, staging, delivery, flag updates, and removal.

use crate::model::{FollowUpState, MessageFlags};
use percent_encoding::{NON_ALPHANUMERIC, percent_encode};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

const MAILDIR_SUBDIRECTORIES: [&str; 3] = ["tmp", "new", "cur"];

/// Result of committing one complete MIME download.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveredMessage {
    /// Account-root-relative final Maildir path.
    pub relative_path: String,
    /// SHA-256 digest of the exact delivered MIME bytes.
    pub mime_hash: String,
}

/// Result of replacing a tracked message with a newer cloud version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplacedMessage {
    /// Newly synchronized message location and digest.
    pub delivered: DeliveredMessage,
    /// Preserved divergent local content, when one was detected.
    pub conflict_path: Option<String>,
}

/// One managed Maildir file correlated by its deterministic message key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScannedMessage {
    /// Account-root-relative current path.
    pub relative_path: String,
    /// Supported flags parsed from the Maildir filename.
    pub flags: MessageFlags,
}

/// Filesystem adapter rooted at one configured account Maildir.
#[derive(Clone, Debug)]
pub struct MaildirStore {
    root: PathBuf,
    fsync_enabled: bool,
}

impl MaildirStore {
    /// Build a Maildir adapter without creating filesystem content.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::new_with_fsync(root, true)
    }

    /// Build a Maildir adapter with fsync explicitly enabled or disabled.
    pub fn new_with_fsync(root: impl Into<PathBuf>, fsync_enabled: bool) -> Self {
        Self {
            root: root.into(),
            fsync_enabled,
        }
    }

    /// Return whether this store synchronizes files and directories durably.
    pub fn get_fsync_enabled(&self) -> bool {
        self.fsync_enabled
    }

    /// Scan selected folders and return files whose keys are already tracked.
    pub fn scan_tracked_messages(
        &self,
        local_paths: &[String],
        tracked_keys: &BTreeSet<String>,
    ) -> Result<BTreeMap<String, Vec<ScannedMessage>>, MaildirError> {
        let mut scanned: BTreeMap<String, Vec<ScannedMessage>> = BTreeMap::new();
        for local_path in local_paths {
            for subdirectory in ["new", "cur"] {
                let directory = self.get_safe_path(local_path)?.join(subdirectory);
                for entry in fs::read_dir(&directory)? {
                    let entry = entry?;
                    if !entry.file_type()?.is_file() {
                        continue;
                    }
                    let file_name = entry
                        .file_name()
                        .into_string()
                        .map_err(|_| MaildirError::UnsafePath)?;
                    let (maildir_key, flags) = split_maildir_name(&file_name);
                    if !tracked_keys.contains(maildir_key) {
                        continue;
                    }
                    scanned
                        .entry(maildir_key.to_owned())
                        .or_default()
                        .push(ScannedMessage {
                            relative_path: format!("{local_path}/{subdirectory}/{file_name}"),
                            flags: MessageFlags {
                                is_read: flags.contains('S'),
                                follow_up: if flags.contains('F') {
                                    FollowUpState::Flagged
                                } else {
                                    FollowUpState::NotFlagged
                                },
                            },
                        });
                }
            }
        }
        for messages in scanned.values_mut() {
            messages.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        }
        Ok(scanned)
    }

    /// Hash one safely resolved managed message for local-edit detection.
    pub fn get_message_hash(&self, relative_path: &str) -> Result<String, MaildirError> {
        get_file_hash(&self.get_safe_path(relative_path)?)
    }

    /// Create one selected remote folder as a private Maildir.
    pub fn create_folder(&self, local_path: &str) -> Result<(), MaildirError> {
        let folder = self.get_safe_path(local_path)?;
        create_private_directory(&self.root)?;
        create_private_directory(&folder)?;
        for child in MAILDIR_SUBDIRECTORIES {
            create_private_directory(&folder.join(child))?;
        }
        Ok(())
    }

    /// Prepare a deterministic, managed staging pathname under Maildir `tmp`.
    pub fn prepare_download(
        &self,
        local_path: &str,
        maildir_key: &str,
    ) -> Result<PathBuf, MaildirError> {
        validate_maildir_key(maildir_key)?;
        self.create_folder(local_path)?;
        let staging = self
            .get_safe_path(local_path)?
            .join("tmp")
            .join(format!(".nochange-{maildir_key}.download"));
        match fs::remove_file(&staging) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(staging)
    }

    /// Synchronize and atomically commit a newly downloaded MIME file.
    pub fn commit_download(
        &self,
        local_path: &str,
        maildir_key: &str,
        flags: MessageFlags,
        staging: &Path,
    ) -> Result<DeliveredMessage, MaildirError> {
        validate_maildir_key(maildir_key)?;
        let expected_staging = self
            .get_safe_path(local_path)?
            .join("tmp")
            .join(format!(".nochange-{maildir_key}.download"));
        if staging != expected_staging {
            return Err(MaildirError::UnsafePath);
        }
        set_private_file_permissions(staging)?;
        if self.fsync_enabled {
            File::open(staging)?.sync_all()?;
        }
        let mime_hash = get_file_hash(staging)?;
        let relative_path = get_message_relative_path(local_path, maildir_key, flags, &[]);
        let destination = self.get_safe_path(&relative_path)?;
        if destination.exists() {
            if get_file_hash(&destination)? == mime_hash {
                fs::remove_file(staging)?;
                return Ok(DeliveredMessage {
                    relative_path,
                    mime_hash,
                });
            }
            return Err(MaildirError::DestinationCollision { relative_path });
        }
        fs::rename(staging, &destination)?;
        sync_directory(destination.parent(), self.fsync_enabled)?;
        Ok(DeliveredMessage {
            relative_path,
            mime_hash,
        })
    }

    /// Replace a tracked cloud message, preserving locally divergent MIME first.
    pub fn replace_tracked(
        &self,
        current_relative_path: &str,
        baseline_hash: &str,
        local_path: &str,
        maildir_key: &str,
        flags: MessageFlags,
        staging: &Path,
    ) -> Result<ReplacedMessage, MaildirError> {
        validate_maildir_key(maildir_key)?;
        let current = self.get_safe_path(current_relative_path)?;
        let expected_staging = self
            .get_safe_path(local_path)?
            .join("tmp")
            .join(format!(".nochange-{maildir_key}.download"));
        if staging != expected_staging || !current.is_file() {
            return Err(MaildirError::UnsafePath);
        }
        set_private_file_permissions(staging)?;
        if self.fsync_enabled {
            File::open(staging)?.sync_all()?;
        }
        let current_hash = get_file_hash(&current)?;
        let conflict_path = if current_hash == baseline_hash {
            None
        } else {
            Some(self.preserve_conflict(&current, current_relative_path, maildir_key)?)
        };
        let mime_hash = get_file_hash(staging)?;
        let preserved_flags = get_preserved_flags(&current)?;
        let destination_relative =
            get_message_relative_path(local_path, maildir_key, flags, &preserved_flags);
        let destination = self.get_safe_path(&destination_relative)?;

        if current_hash == mime_hash {
            fs::remove_file(staging)?;
            if destination != current {
                if destination.exists() {
                    return Err(MaildirError::DestinationCollision {
                        relative_path: destination_relative,
                    });
                }
                fs::rename(&current, &destination)?;
                sync_directory(current.parent(), self.fsync_enabled)?;
                sync_directory(destination.parent(), self.fsync_enabled)?;
            }
            return Ok(ReplacedMessage {
                delivered: DeliveredMessage {
                    relative_path: destination_relative,
                    mime_hash,
                },
                conflict_path,
            });
        }
        if destination != current && destination.exists() {
            return Err(MaildirError::DestinationCollision {
                relative_path: destination_relative,
            });
        }
        fs::rename(staging, &destination)?;
        if destination != current {
            fs::remove_file(&current)?;
        }
        sync_directory(destination.parent(), self.fsync_enabled)?;
        Ok(ReplacedMessage {
            delivered: DeliveredMessage {
                relative_path: destination_relative,
                mime_hash,
            },
            conflict_path,
        })
    }

    /// Apply synchronized `S` and `F` flags while preserving `D`, `P`, and `R`.
    pub fn set_flags(
        &self,
        current_relative_path: &str,
        flags: MessageFlags,
    ) -> Result<String, MaildirError> {
        let current = self.get_safe_path(current_relative_path)?;
        let file_name = current
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or(MaildirError::UnsafePath)?;
        let (maildir_key, existing_flags) = split_maildir_name(file_name);
        validate_maildir_key(maildir_key)?;
        let folder = Path::new(current_relative_path)
            .parent()
            .and_then(Path::parent)
            .and_then(Path::to_str)
            .ok_or(MaildirError::UnsafePath)?;
        let preserved: Vec<char> = existing_flags
            .chars()
            .filter(|flag| matches!(flag, 'D' | 'P' | 'R'))
            .collect();
        let destination_relative =
            get_message_relative_path(folder, maildir_key, flags, &preserved);
        if destination_relative == current_relative_path {
            return Ok(destination_relative);
        }
        let destination = self.get_safe_path(&destination_relative)?;
        if destination.exists() {
            return Err(MaildirError::DestinationCollision {
                relative_path: destination_relative,
            });
        }
        fs::rename(&current, &destination)?;
        sync_directory(destination.parent(), self.fsync_enabled)?;
        Ok(destination_relative)
    }

    /// Remove one tracked message idempotently without permitting path escape.
    pub fn remove_message(&self, relative_path: &str) -> Result<(), MaildirError> {
        let path = self.get_safe_path(relative_path)?;
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Remove a tracked cloud message, preserving locally divergent MIME first.
    pub fn remove_tracked(
        &self,
        relative_path: &str,
        baseline_hash: &str,
        maildir_key: &str,
    ) -> Result<Option<String>, MaildirError> {
        validate_maildir_key(maildir_key)?;
        let path = self.get_safe_path(relative_path)?;
        if !path.exists() {
            return Ok(None);
        }
        if !path.is_file() {
            return Err(MaildirError::UnsafePath);
        }
        let conflict_path = if get_file_hash(&path)? == baseline_hash {
            None
        } else {
            Some(self.preserve_conflict(&path, relative_path, maildir_key)?)
        };
        fs::remove_file(path)?;
        Ok(conflict_path)
    }

    /// Resolve a stored relative path below this account root.
    pub fn get_message_path(&self, relative_path: &str) -> Result<PathBuf, MaildirError> {
        self.get_safe_path(relative_path)
    }

    fn get_safe_path(&self, relative_path: &str) -> Result<PathBuf, MaildirError> {
        let path = Path::new(relative_path);
        if path.as_os_str().is_empty()
            || path.is_absolute()
            || path
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(MaildirError::UnsafePath);
        }
        Ok(self.root.join(path))
    }

    fn preserve_conflict(
        &self,
        current: &Path,
        current_relative_path: &str,
        maildir_key: &str,
    ) -> Result<String, MaildirError> {
        self.create_folder(".nochange-conflicts")?;
        let existing_flags = current_relative_path
            .rsplit_once(":2,")
            .map_or("", |(_, flags)| flags);
        for sequence in 1_u32..=u32::MAX {
            let suffix = if existing_flags.is_empty() {
                String::new()
            } else {
                format!(":2,{existing_flags}")
            };
            let relative_path =
                format!(".nochange-conflicts/cur/{maildir_key}.conflict-{sequence}{suffix}");
            let destination = self.get_safe_path(&relative_path)?;
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut output = match options.open(&destination) {
                Ok(output) => output,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            };
            let mut input = File::open(current)?;
            std::io::copy(&mut input, &mut output)?;
            output.flush()?;
            if self.fsync_enabled {
                output.sync_all()?;
            }
            sync_directory(destination.parent(), self.fsync_enabled)?;
            return Ok(relative_path);
        }
        Err(MaildirError::ConflictExhausted)
    }
}

/// Encode a `/`-delimited remote path into one flattened Maildir name.
pub fn get_encoded_folder_path(
    remote_path: &str,
    folder_separator: char,
) -> Result<String, MaildirError> {
    if remote_path.is_empty() {
        return Err(MaildirError::InvalidFolderPath);
    }
    let mut encoded_components = Vec::new();
    for component in remote_path.split('/') {
        if component.is_empty() {
            return Err(MaildirError::InvalidFolderPath);
        }
        encoded_components.push(encode_folder_component(component, folder_separator));
    }
    Ok(encoded_components.join(&folder_separator.to_string()))
}

/// Derive a deterministic key that does not reveal the Graph message ID.
pub fn get_maildir_key(account_identity: &str, message_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(account_identity.as_bytes());
    digest.update([0]);
    digest.update(message_id.as_bytes());
    hex::encode(digest.finalize())
}

/// Maildir path, collision, and filesystem failures.
#[derive(Debug, Error)]
pub enum MaildirError {
    /// A remote folder path could not be represented safely.
    #[error("remote mail folder path is invalid")]
    InvalidFolderPath,
    /// A caller supplied an absolute or parent-traversing path.
    #[error("Maildir path escaped the configured account root")]
    UnsafePath,
    /// A different file already occupied a deterministic destination.
    #[error("Maildir destination '{relative_path}' already contains different content")]
    DestinationCollision {
        /// Account-root-relative colliding path.
        relative_path: String,
    },
    /// No unused conflict filename remained in the supported sequence range.
    #[error("could not allocate a unique Maildir conflict filename")]
    ConflictExhausted,
    /// A Maildir directory or message operation failed.
    #[error("could not update the configured Maildir")]
    Io(#[from] std::io::Error),
}

fn encode_folder_component(component: &str, folder_separator: char) -> String {
    let mut encoded = String::new();
    let mut characters = component.chars().peekable();
    while let Some(character) = characters.next() {
        let is_unsafe_ending = characters.peek().is_none() && matches!(character, ' ' | '.');
        let requires_encoding = character == folder_separator
            || character == '%'
            || character.is_control()
            || is_unsafe_ending
            || matches!(
                character,
                '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*'
            );
        if requires_encoding {
            let mut bytes = [0; 4];
            let value = character.encode_utf8(&mut bytes);
            encoded.push_str(&percent_encode(value.as_bytes(), NON_ALPHANUMERIC).to_string());
        } else {
            encoded.push(character);
        }
    }
    encoded
}

fn validate_maildir_key(maildir_key: &str) -> Result<(), MaildirError> {
    let valid = !maildir_key.is_empty()
        && maildir_key
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'));
    if valid {
        Ok(())
    } else {
        Err(MaildirError::UnsafePath)
    }
}

fn get_message_relative_path(
    local_path: &str,
    maildir_key: &str,
    flags: MessageFlags,
    preserved_flags: &[char],
) -> String {
    let mut maildir_flags: BTreeSet<char> = preserved_flags.iter().copied().collect();
    if flags.follow_up == FollowUpState::Flagged {
        maildir_flags.insert('F');
    }
    if flags.is_read {
        maildir_flags.insert('S');
    }
    if maildir_flags.is_empty() {
        format!("{local_path}/new/{maildir_key}")
    } else {
        let suffix: String = maildir_flags.into_iter().collect();
        format!("{local_path}/cur/{maildir_key}:2,{suffix}")
    }
}

fn split_maildir_name(file_name: &str) -> (&str, &str) {
    file_name
        .split_once(":2,")
        .map_or((file_name, ""), |(key, flags)| (key, flags))
}

/// Return unsupported Maildir flags that cloud synchronization must preserve.
fn get_preserved_flags(path: &Path) -> Result<Vec<char>, MaildirError> {
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or(MaildirError::UnsafePath)?;
    let (_, existing_flags) = split_maildir_name(file_name);
    Ok(existing_flags
        .chars()
        .filter(|flag| matches!(flag, 'D' | 'P' | 'R'))
        .collect())
}

fn get_file_hash(path: &Path) -> Result<String, MaildirError> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn create_private_directory(path: &Path) -> Result<(), MaildirError> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn set_private_file_permissions(path: &Path) -> Result<(), MaildirError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        if !path.is_file() {
            return Err(MaildirError::UnsafePath);
        }
    }
    Ok(())
}

fn sync_directory(path: Option<&Path>, fsync_enabled: bool) -> Result<(), MaildirError> {
    let path = path.ok_or(MaildirError::UnsafePath)?;
    #[cfg(unix)]
    {
        if fsync_enabled {
            File::open(path)?.sync_all()?;
        }
    }
    #[cfg(not(unix))]
    let _ = fsync_enabled;
    Ok(())
}
