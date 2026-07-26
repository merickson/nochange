//! Replay-safe cloud-to-local mailbox synchronization.

use crate::config::AccountConfig;
use crate::graph::{GraphApi, GraphError};
use crate::maildir::{MaildirError, MaildirStore, get_encoded_folder_path, get_maildir_key};
use crate::model::{DeltaChange, RemoteFolderMetadata, RemoteMessage};
use crate::state::{PendingFlagOperation, StateDatabase, StateError, StoredFolder, StoredMessage};
use futures_util::future::join_all;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs;
use std::path::PathBuf;
use thiserror::Error;

/// Maximum number of MIME responses transferred before ordered local delivery.
const MAX_CONCURRENT_DOWNLOADS: usize = 4;

/// Counts of cloud changes applied or planned for one account.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SyncSummary {
    /// Number of selected folders examined.
    pub folders: usize,
    /// Number of newly discovered messages.
    pub created: usize,
    /// Number of changed messages.
    pub updated: usize,
    /// Number of removed messages.
    pub deleted: usize,
    /// Number of divergent local messages preserved as conflict copies.
    pub conflicted: usize,
    /// Number of local supported flag changes submitted or planned for Graph.
    pub local_flag_updates: usize,
    /// Number of local missing, moved, duplicated, or edited tracked files deferred.
    pub local_ignored: usize,
}

/// High-level action kind reported without exposing a remote message ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncActionKind {
    /// Create, refresh, move, or update flags for a message.
    Upsert,
    /// Remove a message that no longer exists in the selected cloud folder.
    Delete,
}

/// Safe synchronization progress that excludes message identifiers and content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncProgress {
    /// Folder enumeration is starting from an initial or saved checkpoint.
    FolderEnumerationStarted {
        /// Whether a saved delta checkpoint is being resumed.
        resumed: bool,
    },
    /// A folder delta page request is about to start.
    FolderPageStarted {
        /// One-based page number in this delta round.
        page: usize,
    },
    /// A folder delta page was validated.
    FolderPageCompleted {
        /// One-based page number in this delta round.
        page: usize,
        /// Number of changes on this page.
        changes: usize,
        /// Whether this page completed the delta round.
        complete: bool,
    },
    /// Folder paths and configuration filters have been resolved.
    FolderEnumerationCompleted {
        /// Total non-deleted folders currently known.
        discovered: usize,
        /// Number of folders selected for message enumeration.
        selected: usize,
    },
    /// Local tracked-message scanning is starting.
    LocalScanStarted {
        /// Number of selected managed folders.
        folders: usize,
        /// Number of tracked message baselines.
        tracked: usize,
    },
    /// Local tracked-message scanning completed.
    LocalScanCompleted {
        /// Supported flag mutations discovered.
        flag_updates: usize,
        /// Tracked messages found in a different managed folder.
        moved: usize,
        /// Tracked messages absent from managed folders.
        missing: usize,
        /// Tracked keys with multiple managed files.
        duplicates: usize,
        /// Flag-change candidates whose MIME diverged locally.
        edited: usize,
    },
    /// Journaled local flag mutations are being submitted.
    LocalFlagApplyStarted {
        /// Number of Graph mutations to submit in this run.
        total: usize,
    },
    /// One journaled local flag mutation is being submitted.
    LocalFlagApplyProgress {
        /// One-based mutation position.
        position: usize,
        /// Total mutations submitted in this run.
        total: usize,
    },
    /// Message enumeration is starting for one selected folder.
    MessageFolderStarted {
        /// User-visible remote folder path.
        folder: String,
        /// One-based selected-folder position.
        position: usize,
        /// Total selected folders.
        total: usize,
        /// Whether this folder has a saved message checkpoint.
        resumed: bool,
        /// Initial-sync estimate of all Outlook items in the folder.
        estimated_total: Option<u32>,
    },
    /// A message delta page request is about to start.
    MessagePageStarted {
        /// User-visible remote folder path.
        folder: String,
        /// One-based page number in this folder's delta round.
        page: usize,
    },
    /// A message delta page was validated.
    MessagePageCompleted {
        /// User-visible remote folder path.
        folder: String,
        /// One-based page number in this folder's delta round.
        page: usize,
        /// Number of changes on this page.
        changes: usize,
        /// Unique changes accumulated for this folder.
        accumulated: usize,
        /// Whether this page completed the folder's delta round.
        complete: bool,
        /// Initial-sync estimate of all Outlook items in the folder.
        estimated_total: Option<u32>,
    },
    /// One folder's complete message delta round has been enumerated.
    MessageFolderCompleted {
        /// User-visible remote folder path.
        folder: String,
        /// Unique changes accumulated for this folder.
        changes: usize,
    },
    /// Cloud-origin message actions are about to be applied locally.
    MessageApplyStarted {
        /// Total collapsed message actions.
        total: usize,
    },
    /// One cloud-origin message action is about to be applied.
    MessageApplyProgress {
        /// One-based action position.
        position: usize,
        /// Total collapsed message actions.
        total: usize,
        /// Safe action classification.
        action: SyncActionKind,
    },
}

/// Receives safe synchronization progress for status output or diagnostics.
pub trait SyncProgressReporter: Send + Sync {
    /// Record or display one progress transition.
    fn report(&self, progress: SyncProgress);
}

struct NoopProgressReporter;

impl SyncProgressReporter for NoopProgressReporter {
    fn report(&self, _progress: SyncProgress) {}
}

static NOOP_PROGRESS_REPORTER: NoopProgressReporter = NoopProgressReporter;

/// Coordinates Graph delta rounds, Maildir delivery, and durable checkpoints.
pub struct CloudSynchronizer<'a, G> {
    graph: &'a G,
    reporter: &'a dyn SyncProgressReporter,
}

impl<'a, G> CloudSynchronizer<'a, G>
where
    G: GraphApi,
{
    /// Build a cloud synchronizer around an authenticated Graph adapter.
    pub fn new(graph: &'a G) -> Self {
        Self {
            graph,
            reporter: &NOOP_PROGRESS_REPORTER,
        }
    }

    /// Build a cloud synchronizer that reports safe progress transitions.
    pub fn new_with_reporter(graph: &'a G, reporter: &'a dyn SyncProgressReporter) -> Self {
        Self { graph, reporter }
    }

    /// Synchronize one configured account, or plan it without local mutations.
    pub async fn sync_account(
        &self,
        account: &AccountConfig,
        state: &mut StateDatabase,
        maildir: &MaildirStore,
        dry_run: bool,
    ) -> Result<SyncSummary, SyncError> {
        let folder_checkpoint = state.get_folder_delta_link(&account.name)?;
        self.reporter
            .report(SyncProgress::FolderEnumerationStarted {
                resumed: folder_checkpoint.is_some(),
            });
        let (folder_changes, folder_delta_link) = self
            .collect_folder_changes(folder_checkpoint.as_deref())
            .await?;
        let folders = build_folder_plan(account, state, folder_changes)?;
        let selected_count = folders.values().filter(|folder| folder.is_selected).count();
        self.reporter
            .report(SyncProgress::FolderEnumerationCompleted {
                discovered: folders.len(),
                selected: selected_count,
            });

        if dry_run {
            return self
                .get_dry_run_summary(account, state, maildir, &folders)
                .await;
        }

        state.ensure_account(&account.name, &account.user)?;
        let mut summary = SyncSummary::default();
        apply_folder_plan(account, state, maildir, &folders, &mut summary)?;
        state.set_folder_delta_link(&account.name, &folder_delta_link)?;

        let selected: Vec<StoredFolder> = folders
            .values()
            .filter(|folder| folder.is_selected)
            .cloned()
            .collect();
        summary.folders = selected.len();
        let local_plan = self.build_local_flag_plan(account, state, maildir, &selected)?;
        summary.local_ignored = local_plan.get_ignored_count();
        self.apply_local_flag_plan(account, state, local_plan, &mut summary)
            .await?;
        let rounds = self.collect_message_rounds(&selected).await?;
        let mut changes = collapse_message_changes(&rounds);
        self.suppress_local_flag_echoes(account, state, maildir, &mut changes)?;
        self.apply_message_changes(account, state, maildir, changes, &mut summary)
            .await?;
        for round in rounds {
            state.set_message_delta_link(&account.name, &round.folder_id, &round.delta_link)?;
        }
        Ok(summary)
    }

    fn build_local_flag_plan(
        &self,
        account: &AccountConfig,
        state: &StateDatabase,
        maildir: &MaildirStore,
        folders: &[StoredFolder],
    ) -> Result<LocalFlagPlan, SyncError> {
        let mut messages = Vec::new();
        let mut folder_paths = HashMap::new();
        for folder in folders {
            folder_paths.insert(folder.id.clone(), folder.local_path.clone());
            messages.extend(state.list_messages(&account.name, &folder.id)?);
        }
        self.reporter.report(SyncProgress::LocalScanStarted {
            folders: folders.len(),
            tracked: messages.len(),
        });
        if messages.is_empty() {
            let plan = LocalFlagPlan::default();
            self.report_local_scan(&plan);
            return Ok(plan);
        }
        let tracked_keys: BTreeSet<String> = messages
            .iter()
            .map(|message| message.maildir_key.clone())
            .collect();
        let local_paths: Vec<String> = folders
            .iter()
            .map(|folder| folder.local_path.clone())
            .collect();
        let scanned = maildir.scan_tracked_messages(&local_paths, &tracked_keys)?;
        let pending: HashMap<String, PendingFlagOperation> = state
            .list_pending_flag_operations(&account.name)?
            .into_iter()
            .map(|operation| (operation.message_id.clone(), operation))
            .collect();
        let mut plan = LocalFlagPlan::default();
        for message in messages {
            let matches = scanned
                .get(&message.maildir_key)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let [local] = matches else {
                if matches.is_empty() {
                    plan.missing += 1;
                } else {
                    plan.duplicates += 1;
                }
                continue;
            };
            let expected_folder = folder_paths
                .get(&message.folder_id)
                .ok_or_else(|| StateError::UnknownFolder(message.folder_id.clone()))?;
            if !local
                .relative_path
                .strip_prefix(expected_folder)
                .is_some_and(|suffix| suffix.starts_with('/'))
            {
                plan.moved += 1;
                continue;
            }
            if local.flags == message.flags {
                continue;
            }
            if maildir.get_message_hash(&local.relative_path)? != message.mime_hash {
                plan.edited += 1;
                continue;
            }
            let operation = PendingFlagOperation {
                message_id: message.id.clone(),
                flags: local.flags,
                relative_path: local.relative_path.clone(),
                submitted: false,
            };
            let needs_journal = pending.get(&message.id).is_none_or(|existing| {
                existing.flags != operation.flags
                    || existing.relative_path != operation.relative_path
            });
            plan.flag_updates.push(PlannedFlagUpdate {
                operation,
                needs_journal,
            });
        }
        self.report_local_scan(&plan);
        Ok(plan)
    }

    fn report_local_scan(&self, plan: &LocalFlagPlan) {
        self.reporter.report(SyncProgress::LocalScanCompleted {
            flag_updates: plan.flag_updates.len(),
            moved: plan.moved,
            missing: plan.missing,
            duplicates: plan.duplicates,
            edited: plan.edited,
        });
    }

    async fn apply_local_flag_plan(
        &self,
        account: &AccountConfig,
        state: &mut StateDatabase,
        plan: LocalFlagPlan,
        summary: &mut SyncSummary,
    ) -> Result<(), SyncError> {
        for planned in plan.flag_updates {
            if planned.needs_journal {
                state.upsert_pending_flag_operation(&account.name, &planned.operation)?;
            }
        }
        let pending: Vec<PendingFlagOperation> = state
            .list_pending_flag_operations(&account.name)?
            .into_iter()
            .filter(|operation| !operation.submitted)
            .collect();
        summary.local_flag_updates = pending.len();
        self.reporter.report(SyncProgress::LocalFlagApplyStarted {
            total: pending.len(),
        });
        for (index, operation) in pending.into_iter().enumerate() {
            self.reporter.report(SyncProgress::LocalFlagApplyProgress {
                position: index + 1,
                total: summary.local_flag_updates,
            });
            self.graph
                .update_message_flags(&operation.message_id, operation.flags)
                .await?;
            state.mark_pending_flag_operation_submitted(&account.name, &operation.message_id)?;
        }
        Ok(())
    }

    fn suppress_local_flag_echoes(
        &self,
        account: &AccountConfig,
        state: &mut StateDatabase,
        maildir: &MaildirStore,
        changes: &mut BTreeMap<String, DeltaChange<RemoteMessage>>,
    ) -> Result<(), SyncError> {
        let pending = state.list_pending_flag_operations(&account.name)?;
        for operation in pending {
            if !operation.submitted {
                continue;
            }
            let Some(DeltaChange::Upsert(remote)) = changes.get(&operation.message_id) else {
                continue;
            };
            let Some(existing) = state.get_message(&account.name, &operation.message_id)? else {
                continue;
            };
            if remote.folder_id != existing.folder_id
                || remote.flags != operation.flags
                || !maildir
                    .get_message_path(&operation.relative_path)?
                    .is_file()
                || maildir.get_message_hash(&operation.relative_path)? != existing.mime_hash
            {
                continue;
            }
            state.upsert_message(
                &account.name,
                &build_stored_message(
                    remote,
                    existing.maildir_key,
                    operation.relative_path,
                    existing.mime_hash,
                ),
            )?;
            state.delete_pending_flag_operation(&account.name, &operation.message_id)?;
            changes.remove(&operation.message_id);
        }
        Ok(())
    }

    async fn collect_folder_changes(
        &self,
        checkpoint: Option<&str>,
    ) -> Result<(BTreeMap<String, DeltaChange<RemoteFolderMetadata>>, String), SyncError> {
        let mut next = checkpoint.map(str::to_owned);
        let mut changes = BTreeMap::new();
        let mut page_number = 1;
        loop {
            self.reporter
                .report(SyncProgress::FolderPageStarted { page: page_number });
            let page = self.graph.get_folder_delta_page(next.as_deref()).await?;
            let page_change_count = page.changes.len();
            for change in page.changes {
                changes.insert(get_folder_change_id(&change).to_owned(), change);
            }
            self.reporter.report(SyncProgress::FolderPageCompleted {
                page: page_number,
                changes: page_change_count,
                complete: page.delta_link.is_some(),
            });
            match (page.next_link, page.delta_link) {
                (Some(next_link), None) => {
                    next = Some(next_link);
                    page_number += 1;
                }
                (None, Some(delta_link)) => return Ok((changes, delta_link)),
                _ => return Err(SyncError::InvalidDeltaPage),
            }
        }
    }

    async fn collect_message_rounds(
        &self,
        folders: &[StoredFolder],
    ) -> Result<Vec<MessageRound>, SyncError> {
        let mut rounds = Vec::with_capacity(folders.len());
        for (folder_index, folder) in folders.iter().enumerate() {
            let estimated_total = folder
                .message_delta_link
                .is_none()
                .then_some(folder.total_item_count);
            self.reporter.report(SyncProgress::MessageFolderStarted {
                folder: folder.remote_path.clone(),
                position: folder_index + 1,
                total: folders.len(),
                resumed: folder.message_delta_link.is_some(),
                estimated_total,
            });
            let mut next = folder.message_delta_link.clone();
            let mut changes = BTreeMap::new();
            let mut page_number = 1;
            loop {
                self.reporter.report(SyncProgress::MessagePageStarted {
                    folder: folder.remote_path.clone(),
                    page: page_number,
                });
                let page = self
                    .graph
                    .get_message_delta_page(&folder.id, next.as_deref())
                    .await?;
                let page_change_count = page.changes.len();
                for change in page.changes {
                    changes.insert(get_message_change_id(&change).to_owned(), change);
                }
                self.reporter.report(SyncProgress::MessagePageCompleted {
                    folder: folder.remote_path.clone(),
                    page: page_number,
                    changes: page_change_count,
                    accumulated: changes.len(),
                    complete: page.delta_link.is_some(),
                    estimated_total,
                });
                match (page.next_link, page.delta_link) {
                    (Some(next_link), None) => {
                        next = Some(next_link);
                        page_number += 1;
                    }
                    (None, Some(delta_link)) => {
                        self.reporter.report(SyncProgress::MessageFolderCompleted {
                            folder: folder.remote_path.clone(),
                            changes: changes.len(),
                        });
                        rounds.push(MessageRound {
                            folder_id: folder.id.clone(),
                            delta_link,
                            changes,
                        });
                        break;
                    }
                    _ => return Err(SyncError::InvalidDeltaPage),
                }
            }
        }
        Ok(rounds)
    }

    async fn apply_message_changes(
        &self,
        account: &AccountConfig,
        state: &mut StateDatabase,
        maildir: &MaildirStore,
        changes: BTreeMap<String, DeltaChange<RemoteMessage>>,
        summary: &mut SyncSummary,
    ) -> Result<(), SyncError> {
        let total = changes.len();
        self.reporter
            .report(SyncProgress::MessageApplyStarted { total });
        let mut changes = changes.into_values().enumerate().peekable();
        while changes.peek().is_some() {
            let mut downloads = Vec::with_capacity(MAX_CONCURRENT_DOWNLOADS);
            for (index, change) in changes.by_ref().take(MAX_CONCURRENT_DOWNLOADS) {
                let action = match &change {
                    DeltaChange::Upsert(_) => SyncActionKind::Upsert,
                    DeltaChange::Delete { .. } => SyncActionKind::Delete,
                };
                self.reporter.report(SyncProgress::MessageApplyProgress {
                    position: index + 1,
                    total,
                    action,
                });
                match change {
                    DeltaChange::Delete { id } => {
                        if let Some(existing) = state.get_message(&account.name, &id)? {
                            if maildir
                                .remove_tracked(
                                    &existing.relative_path,
                                    &existing.mime_hash,
                                    &existing.maildir_key,
                                )?
                                .is_some()
                            {
                                summary.conflicted += 1;
                            }
                            state.delete_message(&account.name, &id)?;
                            summary.deleted += 1;
                        }
                    }
                    DeltaChange::Upsert(remote) => {
                        if let Some(download) =
                            self.prepare_message_upsert(account, state, maildir, remote, summary)?
                        {
                            downloads.push(download);
                        }
                    }
                }
            }
            if downloads.is_empty() {
                continue;
            }
            let completed = join_all(downloads.into_iter().map(|download| async move {
                let result = self
                    .graph
                    .download_message(&download.remote.id, &download.staging)
                    .await;
                (download, result)
            }))
            .await;
            self.commit_message_downloads(account, state, maildir, completed.into(), summary)?;
        }
        Ok(())
    }

    /// Apply metadata-only work and prepare staging for an upsert that needs MIME.
    fn prepare_message_upsert(
        &self,
        account: &AccountConfig,
        state: &mut StateDatabase,
        maildir: &MaildirStore,
        remote: RemoteMessage,
        summary: &mut SyncSummary,
    ) -> Result<Option<MessageDownload>, SyncError> {
        let target = state
            .get_folder(&account.name, &remote.folder_id)?
            .filter(|folder| folder.is_selected);
        let existing = state.get_message(&account.name, &remote.id)?;
        let Some(target) = target else {
            if let Some(existing) = existing {
                if maildir
                    .remove_tracked(
                        &existing.relative_path,
                        &existing.mime_hash,
                        &existing.maildir_key,
                    )?
                    .is_some()
                {
                    summary.conflicted += 1;
                }
                state.delete_message(&account.name, &remote.id)?;
                summary.deleted += 1;
            }
            return Ok(None);
        };

        if let Some(existing) = existing.as_ref() {
            let current_exists = maildir.get_message_path(&existing.relative_path)?.is_file();
            if current_exists
                && existing.remote_version == remote.remote_version
                && existing.folder_id == remote.folder_id
            {
                if existing.flags == remote.flags {
                    return Ok(None);
                }
                let relative_path = maildir.set_flags(&existing.relative_path, remote.flags)?;
                state.upsert_message(
                    &account.name,
                    &build_stored_message(
                        &remote,
                        existing.maildir_key.clone(),
                        relative_path,
                        existing.mime_hash.clone(),
                    ),
                )?;
                summary.updated += 1;
                return Ok(None);
            }
        }

        let maildir_key = existing.as_ref().map_or_else(
            || get_maildir_key(&account.user, &remote.id),
            |value| value.maildir_key.clone(),
        );
        let staging = maildir.prepare_download(&target.local_path, &maildir_key)?;
        Ok(Some(MessageDownload {
            remote,
            target_local_path: target.local_path,
            existing,
            maildir_key,
            staging,
        }))
    }

    /// Commit a completed transfer batch in source order and clean up on failure.
    fn commit_message_downloads(
        &self,
        account: &AccountConfig,
        state: &mut StateDatabase,
        maildir: &MaildirStore,
        mut completed: VecDeque<(MessageDownload, Result<(), GraphError>)>,
        summary: &mut SyncSummary,
    ) -> Result<(), SyncError> {
        while let Some((download, result)) = completed.pop_front() {
            if let Err(error) = result {
                remove_staging_file(&download.staging)?;
                remove_pending_staging_files(&completed)?;
                return Err(error.into());
            }
            let staging = download.staging.clone();
            if let Err(error) =
                self.commit_message_download(account, state, maildir, download, summary)
            {
                remove_staging_file(&staging)?;
                remove_pending_staging_files(&completed)?;
                return Err(error);
            }
        }
        Ok(())
    }

    /// Atomically deliver one staged MIME response and persist its new baseline.
    fn commit_message_download(
        &self,
        account: &AccountConfig,
        state: &mut StateDatabase,
        maildir: &MaildirStore,
        download: MessageDownload,
        summary: &mut SyncSummary,
    ) -> Result<(), SyncError> {
        let delivered = match download.existing.as_ref() {
            Some(existing) if maildir.get_message_path(&existing.relative_path)?.is_file() => {
                let replaced = maildir.replace_tracked(
                    &existing.relative_path,
                    &existing.mime_hash,
                    &download.target_local_path,
                    &download.maildir_key,
                    download.remote.flags,
                    &download.staging,
                )?;
                if replaced.conflict_path.is_some() {
                    summary.conflicted += 1;
                }
                replaced.delivered
            }
            _ => maildir.commit_download(
                &download.target_local_path,
                &download.maildir_key,
                download.remote.flags,
                &download.staging,
            )?,
        };
        state.upsert_message(
            &account.name,
            &build_stored_message(
                &download.remote,
                download.maildir_key,
                delivered.relative_path,
                delivered.mime_hash,
            ),
        )?;
        if download.existing.is_some() {
            summary.updated += 1;
        } else {
            summary.created += 1;
        }
        Ok(())
    }

    async fn get_dry_run_summary(
        &self,
        account: &AccountConfig,
        state: &StateDatabase,
        maildir: &MaildirStore,
        folders: &BTreeMap<String, StoredFolder>,
    ) -> Result<SyncSummary, SyncError> {
        let selected: Vec<StoredFolder> = folders
            .values()
            .filter(|folder| folder.is_selected)
            .cloned()
            .collect();
        let rounds = self.collect_message_rounds(&selected).await?;
        let changes = collapse_message_changes(&rounds);
        let mut summary = SyncSummary {
            folders: selected.len(),
            ..SyncSummary::default()
        };
        let local_plan = self.build_local_flag_plan(account, state, maildir, &selected)?;
        summary.local_flag_updates = local_plan.flag_updates.len()
            + state
                .list_pending_flag_operations(&account.name)?
                .iter()
                .filter(|operation| !operation.submitted)
                .filter(|operation| {
                    !local_plan
                        .flag_updates
                        .iter()
                        .any(|planned| planned.operation.message_id == operation.message_id)
                })
                .count();
        summary.local_ignored = local_plan.get_ignored_count();
        for change in changes.into_values() {
            match change {
                DeltaChange::Delete { id } => {
                    if state.get_message(&account.name, &id)?.is_some() {
                        summary.deleted += 1;
                    }
                }
                DeltaChange::Upsert(remote) => {
                    match state.get_message(&account.name, &remote.id)? {
                        None => summary.created += 1,
                        Some(existing)
                            if existing.remote_version != remote.remote_version
                                || existing.folder_id != remote.folder_id
                                || existing.flags != remote.flags =>
                        {
                            summary.updated += 1;
                        }
                        Some(_) => {}
                    }
                }
            }
        }
        Ok(summary)
    }
}

#[derive(Debug)]
struct MessageRound {
    folder_id: String,
    delta_link: String,
    changes: BTreeMap<String, DeltaChange<RemoteMessage>>,
}

#[derive(Debug, Default)]
struct LocalFlagPlan {
    flag_updates: Vec<PlannedFlagUpdate>,
    moved: usize,
    missing: usize,
    duplicates: usize,
    edited: usize,
}

impl LocalFlagPlan {
    fn get_ignored_count(&self) -> usize {
        self.moved + self.missing + self.duplicates + self.edited
    }
}

#[derive(Debug)]
struct PlannedFlagUpdate {
    operation: PendingFlagOperation,
    needs_journal: bool,
}

#[derive(Debug)]
/// An upsert whose metadata checks determined that MIME transfer is required.
struct MessageDownload {
    remote: RemoteMessage,
    target_local_path: String,
    existing: Option<StoredMessage>,
    maildir_key: String,
    staging: PathBuf,
}

/// Failures that prevent a complete synchronization round.
#[derive(Debug, Error)]
pub enum SyncError {
    /// Microsoft Graph could not complete an operation.
    #[error(transparent)]
    Graph(#[from] GraphError),
    /// Durable synchronization state could not be read or updated.
    #[error(transparent)]
    State(#[from] StateError),
    /// A Maildir operation could not be completed safely.
    #[error(transparent)]
    Maildir(#[from] MaildirError),
    /// A delta page did not contain exactly one continuation or final link.
    #[error("Microsoft Graph returned an incomplete delta page")]
    InvalidDeltaPage,
    /// Remote folder ancestry contained a cycle.
    #[error("Microsoft Graph returned a cyclic mail-folder hierarchy")]
    CyclicFolderHierarchy,
    /// A folder rename cannot yet move existing tracked messages safely.
    #[error("mail folder '{0}' changed path while it still contains synchronized messages")]
    FolderPathChanged(String),
    /// A staging file could not be removed after a failed download.
    #[error("could not clean up an incomplete MIME download")]
    StagingCleanup(#[source] std::io::Error),
    /// The authenticated mailbox did not match the configured identity.
    #[error(
        "account '{account}' expected Microsoft 365 user '{expected}', but signed in as '{actual}'"
    )]
    IdentityMismatch {
        /// Selected local account.
        account: String,
        /// Configured Microsoft 365 identity.
        expected: String,
        /// Identity returned by Graph.
        actual: String,
    },
}

fn build_folder_plan(
    account: &AccountConfig,
    state: &StateDatabase,
    changes: BTreeMap<String, DeltaChange<RemoteFolderMetadata>>,
) -> Result<BTreeMap<String, StoredFolder>, SyncError> {
    let existing = state.list_folders(&account.name)?;
    let existing_by_id: HashMap<String, StoredFolder> = existing
        .iter()
        .cloned()
        .map(|folder| (folder.id.clone(), folder))
        .collect();
    let mut metadata: BTreeMap<String, RemoteFolderMetadata> = existing
        .into_iter()
        .map(|folder| {
            (
                folder.id.clone(),
                RemoteFolderMetadata {
                    id: folder.id,
                    parent_id: folder.parent_id,
                    display_name: folder.display_name,
                    is_hidden: folder.is_hidden,
                    total_item_count: folder.total_item_count,
                },
            )
        })
        .collect();
    for change in changes.into_values() {
        match change {
            DeltaChange::Upsert(folder) => {
                metadata.insert(folder.id.clone(), folder);
            }
            DeltaChange::Delete { id } => {
                metadata.remove(&id);
            }
        }
    }

    let separator = account
        .folder_separator
        .chars()
        .next()
        .ok_or(MaildirError::InvalidFolderPath)?;
    let mut resolved_paths = HashMap::new();
    let mut visiting = BTreeSet::new();
    for id in metadata.keys() {
        resolve_folder_path(id, &metadata, &mut resolved_paths, &mut visiting)?;
    }
    let mut folders = BTreeMap::new();
    for (id, folder) in metadata {
        let remote_path = resolved_paths
            .remove(&id)
            .ok_or(SyncError::CyclicFolderHierarchy)?;
        let local_path = get_encoded_folder_path(&remote_path, separator)?;
        let message_delta_link = existing_by_id
            .get(&id)
            .and_then(|existing| existing.message_delta_link.clone());
        folders.insert(
            id.clone(),
            StoredFolder {
                id,
                parent_id: folder.parent_id,
                display_name: folder.display_name,
                remote_path: remote_path.clone(),
                local_path,
                is_selected: !folder.is_hidden && account.is_folder_selected(&remote_path),
                is_hidden: folder.is_hidden,
                total_item_count: folder.total_item_count,
                message_delta_link,
            },
        );
    }
    Ok(folders)
}

fn resolve_folder_path(
    id: &str,
    metadata: &BTreeMap<String, RemoteFolderMetadata>,
    resolved: &mut HashMap<String, String>,
    visiting: &mut BTreeSet<String>,
) -> Result<String, SyncError> {
    if let Some(path) = resolved.get(id) {
        return Ok(path.clone());
    }
    if !visiting.insert(id.to_owned()) {
        return Err(SyncError::CyclicFolderHierarchy);
    }
    let folder = metadata.get(id).ok_or(SyncError::CyclicFolderHierarchy)?;
    let path = match folder
        .parent_id
        .as_deref()
        .filter(|parent| metadata.contains_key(*parent))
    {
        Some(parent) => format!(
            "{}/{}",
            resolve_folder_path(parent, metadata, resolved, visiting)?,
            folder.display_name
        ),
        None => folder.display_name.clone(),
    };
    visiting.remove(id);
    resolved.insert(id.to_owned(), path.clone());
    Ok(path)
}

fn apply_folder_plan(
    account: &AccountConfig,
    state: &mut StateDatabase,
    maildir: &MaildirStore,
    folders: &BTreeMap<String, StoredFolder>,
    summary: &mut SyncSummary,
) -> Result<(), SyncError> {
    let existing = state.list_folders(&account.name)?;
    for old in &existing {
        if let Some(new) = folders.get(&old.id)
            && old.local_path != new.local_path
            && !state.list_messages(&account.name, &old.id)?.is_empty()
        {
            return Err(SyncError::FolderPathChanged(old.remote_path.clone()));
        }
    }
    for old in existing {
        let retained = folders.get(&old.id);
        if retained.is_none() {
            remove_folder_messages(account, state, maildir, &old, summary)?;
            state.delete_folder(&account.name, &old.id)?;
        }
    }
    for folder in folders.values() {
        if folder.is_selected {
            maildir.create_folder(&folder.local_path)?;
        }
        state.upsert_folder(&account.name, folder)?;
    }
    Ok(())
}

fn remove_folder_messages(
    account: &AccountConfig,
    state: &mut StateDatabase,
    maildir: &MaildirStore,
    folder: &StoredFolder,
    summary: &mut SyncSummary,
) -> Result<(), SyncError> {
    for message in state.list_messages(&account.name, &folder.id)? {
        if maildir
            .remove_tracked(
                &message.relative_path,
                &message.mime_hash,
                &message.maildir_key,
            )?
            .is_some()
        {
            summary.conflicted += 1;
        }
        state.delete_message(&account.name, &message.id)?;
        summary.deleted += 1;
    }
    Ok(())
}

fn collapse_message_changes(
    rounds: &[MessageRound],
) -> BTreeMap<String, DeltaChange<RemoteMessage>> {
    let mut changes = BTreeMap::new();
    for round in rounds {
        for (id, change) in &round.changes {
            match change {
                DeltaChange::Upsert(_) => {
                    changes.insert(id.clone(), change.clone());
                }
                DeltaChange::Delete { .. }
                    if !matches!(changes.get(id), Some(DeltaChange::Upsert(_))) =>
                {
                    changes.insert(id.clone(), change.clone());
                }
                DeltaChange::Delete { .. } => {}
            }
        }
    }
    changes
}

fn build_stored_message(
    remote: &RemoteMessage,
    maildir_key: String,
    relative_path: String,
    mime_hash: String,
) -> StoredMessage {
    StoredMessage {
        id: remote.id.clone(),
        folder_id: remote.folder_id.clone(),
        maildir_key,
        relative_path,
        mime_hash,
        remote_version: remote.remote_version.clone(),
        internet_message_id: remote.internet_message_id.clone(),
        flags: remote.flags,
    }
}

fn get_folder_change_id(change: &DeltaChange<RemoteFolderMetadata>) -> &str {
    match change {
        DeltaChange::Upsert(folder) => &folder.id,
        DeltaChange::Delete { id } => id,
    }
}

fn get_message_change_id(change: &DeltaChange<RemoteMessage>) -> &str {
    match change {
        DeltaChange::Upsert(message) => &message.id,
        DeltaChange::Delete { id } => id,
    }
}

fn remove_staging_file(path: &std::path::Path) -> Result<(), SyncError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(SyncError::StagingCleanup(error)),
    }
}

/// Remove every uncommitted staging file left in a completed transfer batch.
fn remove_pending_staging_files(
    downloads: &VecDeque<(MessageDownload, Result<(), GraphError>)>,
) -> Result<(), SyncError> {
    for (download, _) in downloads {
        remove_staging_file(&download.staging)?;
    }
    Ok(())
}
