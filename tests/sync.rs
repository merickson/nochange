use async_trait::async_trait;
use nochange::config::{AccountConfig, FolderFilter};
use nochange::graph::{GraphApi, GraphError};
use nochange::maildir::MaildirStore;
use nochange::model::{
    DeltaChange, DeltaPage, FollowUpState, MessageFlags, RemoteFolderMetadata, RemoteMessage,
};
use nochange::state::StateDatabase;
use nochange::sync::{
    CloudSynchronizer, SyncError, SyncProgress, SyncProgressReporter, SyncSummary,
};
use std::collections::{HashMap, VecDeque};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tempfile::TempDir;

type MessagePages = HashMap<String, VecDeque<Result<DeltaPage<RemoteMessage>, GraphError>>>;
type MimeResults = HashMap<String, VecDeque<Result<Vec<u8>, GraphError>>>;

#[derive(Default)]
struct RecordingProgressReporter {
    events: Mutex<Vec<SyncProgress>>,
}

impl SyncProgressReporter for RecordingProgressReporter {
    fn report(&self, progress: SyncProgress) {
        if let Ok(mut events) = self.events.lock() {
            events.push(progress);
        }
    }
}

fn build_account(root: &Path) -> AccountConfig {
    AccountConfig {
        name: "work".into(),
        maildir: root.join("mail"),
        user: "me@example.com".into(),
        client_id: "client-id".into(),
        tenant: "organizations".into(),
        folder_separator: ".".into(),
        folder_filter: FolderFilter::All,
    }
}

fn build_folder(id: &str, parent_id: Option<&str>, display_name: &str) -> RemoteFolderMetadata {
    RemoteFolderMetadata {
        id: id.into(),
        parent_id: parent_id.map(str::to_owned),
        display_name: display_name.into(),
        is_hidden: false,
        total_item_count: 1,
    }
}

fn build_message(id: &str, folder_id: &str, version: &str, flags: MessageFlags) -> RemoteMessage {
    RemoteMessage {
        id: id.into(),
        folder_id: folder_id.into(),
        internet_message_id: Some(format!("<{id}@example.com>")),
        remote_version: version.into(),
        flags,
    }
}

fn final_page<T>(changes: Vec<DeltaChange<T>>, delta_link: &str) -> DeltaPage<T> {
    DeltaPage {
        changes,
        next_link: None,
        delta_link: Some(delta_link.into()),
    }
}

fn next_page<T>(changes: Vec<DeltaChange<T>>, next_link: &str) -> DeltaPage<T> {
    DeltaPage {
        changes,
        next_link: Some(next_link.into()),
        delta_link: None,
    }
}

struct FakeGraph {
    folder_pages: Mutex<VecDeque<Result<DeltaPage<RemoteFolderMetadata>, GraphError>>>,
    message_pages: Mutex<MessagePages>,
    mime_results: Mutex<MimeResults>,
    folder_checkpoints: Mutex<Vec<Option<String>>>,
    message_checkpoints: Mutex<Vec<(String, Option<String>)>>,
    downloads: Mutex<Vec<String>>,
    download_delay: Duration,
    active_downloads: AtomicUsize,
    max_active_downloads: AtomicUsize,
}

impl FakeGraph {
    fn new(
        folder_pages: impl IntoIterator<Item = DeltaPage<RemoteFolderMetadata>>,
        message_pages: HashMap<String, Vec<DeltaPage<RemoteMessage>>>,
        mime_results: HashMap<String, Vec<Result<Vec<u8>, GraphError>>>,
    ) -> Self {
        Self {
            folder_pages: Mutex::new(folder_pages.into_iter().map(Ok).collect()),
            message_pages: Mutex::new(
                message_pages
                    .into_iter()
                    .map(|(folder, pages)| (folder, pages.into_iter().map(Ok).collect()))
                    .collect(),
            ),
            mime_results: Mutex::new(
                mime_results
                    .into_iter()
                    .map(|(message, results)| (message, results.into_iter().collect()))
                    .collect(),
            ),
            folder_checkpoints: Mutex::default(),
            message_checkpoints: Mutex::default(),
            downloads: Mutex::default(),
            download_delay: Duration::ZERO,
            active_downloads: AtomicUsize::default(),
            max_active_downloads: AtomicUsize::default(),
        }
    }

    fn with_download_delay(mut self, delay: Duration) -> Self {
        self.download_delay = delay;
        self
    }
}

#[async_trait]
impl GraphApi for FakeGraph {
    async fn get_folder_delta_page(
        &self,
        checkpoint: Option<&str>,
    ) -> Result<DeltaPage<RemoteFolderMetadata>, GraphError> {
        self.folder_checkpoints
            .lock()
            .map_err(|_| GraphError::Request)?
            .push(checkpoint.map(str::to_owned));
        self.folder_pages
            .lock()
            .map_err(|_| GraphError::Request)?
            .pop_front()
            .ok_or(GraphError::Request)?
    }

    async fn get_message_delta_page(
        &self,
        folder_id: &str,
        checkpoint: Option<&str>,
    ) -> Result<DeltaPage<RemoteMessage>, GraphError> {
        self.message_checkpoints
            .lock()
            .map_err(|_| GraphError::Request)?
            .push((folder_id.to_owned(), checkpoint.map(str::to_owned)));
        self.message_pages
            .lock()
            .map_err(|_| GraphError::Request)?
            .get_mut(folder_id)
            .and_then(VecDeque::pop_front)
            .ok_or(GraphError::Request)?
    }

    async fn download_message(
        &self,
        message_id: &str,
        destination: &Path,
    ) -> Result<(), GraphError> {
        self.downloads
            .lock()
            .map_err(|_| GraphError::Request)?
            .push(message_id.to_owned());
        let result = self
            .mime_results
            .lock()
            .map_err(|_| GraphError::Request)?
            .get_mut(message_id)
            .and_then(VecDeque::pop_front)
            .ok_or(GraphError::Request)?;
        let bytes = result?;
        let active = self.active_downloads.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active_downloads
            .fetch_max(active, Ordering::SeqCst);
        tokio::time::sleep(self.download_delay).await;
        let write_result = (|| {
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(destination)
                .map_err(|_| GraphError::OutputFile)?;
            output
                .write_all(&bytes)
                .map_err(|_| GraphError::OutputFile)?;
            output.sync_all().map_err(|_| GraphError::OutputFile)
        })();
        self.active_downloads.fetch_sub(1, Ordering::SeqCst);
        write_result
    }
}

#[tokio::test]
async fn downloads_a_complete_multipage_folder_and_message_baseline() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let account = build_account(temp.path());
    let mut state = StateDatabase::open(&temp.path().join("state.sqlite3"))
        .expect("state database should open");
    let graph = FakeGraph::new(
        [
            next_page(
                vec![DeltaChange::Upsert(build_folder("inbox-id", None, "Inbox"))],
                "https://graph.microsoft.com/v1.0/me/mailFolders/delta?$skiptoken=folder-next",
            ),
            final_page(
                vec![DeltaChange::Upsert(build_folder(
                    "projects-id",
                    Some("inbox-id"),
                    "Projects",
                ))],
                "https://graph.microsoft.com/v1.0/me/mailFolders/delta?$deltatoken=folder-final",
            ),
        ],
        HashMap::from([
            (
                "inbox-id".into(),
                vec![
                    next_page(
                        vec![DeltaChange::Upsert(build_message(
                            "message-1",
                            "inbox-id",
                            "version-1",
                            MessageFlags::default(),
                        ))],
                        "https://graph.microsoft.com/v1.0/me/mailFolders/inbox/messages/delta?$skiptoken=message-next",
                    ),
                    final_page(
                        vec![DeltaChange::Upsert(build_message(
                            "message-2",
                            "inbox-id",
                            "version-2",
                            MessageFlags {
                                is_read: true,
                                follow_up: FollowUpState::Flagged,
                            },
                        ))],
                        "https://graph.microsoft.com/v1.0/me/mailFolders/inbox/messages/delta?$deltatoken=message-final",
                    ),
                ],
            ),
            (
                "projects-id".into(),
                vec![final_page(
                    vec![],
                    "https://graph.microsoft.com/v1.0/me/mailFolders/projects/messages/delta?$deltatoken=projects-final",
                )],
            ),
        ]),
        HashMap::from([
            (
                "message-1".into(),
                vec![Ok(b"Subject: One\r\n\r\nFirst\r\n".to_vec())],
            ),
            (
                "message-2".into(),
                vec![Ok(b"Subject: Two\r\n\r\nSecond\r\n".to_vec())],
            ),
        ]),
    );
    let maildir = MaildirStore::new(&account.maildir);
    let synchronizer = CloudSynchronizer::new(&graph);

    let summary = synchronizer
        .sync_account(&account, &mut state, &maildir, false)
        .await
        .expect("initial synchronization should succeed");

    assert_eq!(
        summary,
        SyncSummary {
            folders: 2,
            created: 2,
            updated: 0,
            deleted: 0,
            conflicted: 0,
        }
    );
    let first = state
        .get_message("work", "message-1")
        .expect("message lookup should succeed")
        .expect("first message should be stored");
    let second = state
        .get_message("work", "message-2")
        .expect("message lookup should succeed")
        .expect("second message should be stored");
    assert!(account.maildir.join(first.relative_path).is_file());
    assert!(account.maildir.join(second.relative_path).is_file());
    assert_eq!(
        state
            .get_folder_delta_link("work")
            .expect("folder checkpoint should load")
            .as_deref(),
        Some("https://graph.microsoft.com/v1.0/me/mailFolders/delta?$deltatoken=folder-final")
    );
    assert_eq!(
        state
            .get_folder("work", "inbox-id")
            .expect("folder should load")
            .expect("inbox should exist")
            .message_delta_link
            .as_deref(),
        Some(
            "https://graph.microsoft.com/v1.0/me/mailFolders/inbox/messages/delta?$deltatoken=message-final"
        )
    );
}

#[tokio::test]
async fn downloads_mime_with_a_maximum_concurrency_of_four() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let account = build_account(temp.path());
    let mut state = StateDatabase::open(&temp.path().join("state.sqlite3"))
        .expect("state database should open");
    let message_changes: Vec<_> = (0..8)
        .map(|index| {
            DeltaChange::Upsert(build_message(
                &format!("message-{index}"),
                "inbox-id",
                "version-1",
                MessageFlags::default(),
            ))
        })
        .collect();
    let mime_results = (0..8)
        .map(|index| {
            (
                format!("message-{index}"),
                vec![Ok(format!("message {index}").into_bytes())],
            )
        })
        .collect();
    let graph = FakeGraph::new(
        [final_page(
            vec![DeltaChange::Upsert(build_folder("inbox-id", None, "Inbox"))],
            "folder-delta",
        )],
        HashMap::from([(
            "inbox-id".into(),
            vec![final_page(message_changes, "message-delta")],
        )]),
        mime_results,
    )
    .with_download_delay(Duration::from_millis(20));

    let summary = CloudSynchronizer::new(&graph)
        .sync_account(
            &account,
            &mut state,
            &MaildirStore::new(&account.maildir),
            false,
        )
        .await
        .expect("concurrent synchronization should succeed");

    assert_eq!(summary.created, 8);
    assert_eq!(graph.max_active_downloads.load(Ordering::SeqCst), 4);
    assert_eq!(graph.active_downloads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn removes_all_staging_files_when_a_concurrent_download_fails() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let account = build_account(temp.path());
    let mut state = StateDatabase::open(&temp.path().join("state.sqlite3"))
        .expect("state database should open");
    let message_changes: Vec<_> = (0..4)
        .map(|index| {
            DeltaChange::Upsert(build_message(
                &format!("message-{index}"),
                "inbox-id",
                "version-1",
                MessageFlags::default(),
            ))
        })
        .collect();
    let graph = FakeGraph::new(
        [final_page(
            vec![DeltaChange::Upsert(build_folder("inbox-id", None, "Inbox"))],
            "folder-delta",
        )],
        HashMap::from([(
            "inbox-id".into(),
            vec![final_page(message_changes, "message-delta")],
        )]),
        HashMap::from([
            ("message-0".into(), vec![Err(GraphError::Request)]),
            ("message-1".into(), vec![Ok(b"message 1".to_vec())]),
            ("message-2".into(), vec![Ok(b"message 2".to_vec())]),
            ("message-3".into(), vec![Ok(b"message 3".to_vec())]),
        ]),
    )
    .with_download_delay(Duration::from_millis(20));

    let result = CloudSynchronizer::new(&graph)
        .sync_account(
            &account,
            &mut state,
            &MaildirStore::new(&account.maildir),
            false,
        )
        .await;

    assert!(matches!(result, Err(SyncError::Graph(GraphError::Request))));
    let staging_files: Vec<_> = std::fs::read_dir(account.maildir.join("Inbox/tmp"))
        .expect("staging directory should exist")
        .collect();
    assert!(staging_files.is_empty());
}

#[tokio::test]
async fn resumes_an_interrupted_round_without_redownloading_committed_messages() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let account = build_account(temp.path());
    let mut state = StateDatabase::open(&temp.path().join("state.sqlite3"))
        .expect("state database should open");
    let folder_delta = "https://graph.microsoft.com/v1.0/me/mailFolders/delta?$deltatoken=folder";
    let message_delta =
        "https://graph.microsoft.com/v1.0/me/mailFolders/inbox/messages/delta?$deltatoken=messages";
    let messages = vec![
        DeltaChange::Upsert(build_message(
            "message-1",
            "inbox-id",
            "version-1",
            MessageFlags::default(),
        )),
        DeltaChange::Upsert(build_message(
            "message-2",
            "inbox-id",
            "version-2",
            MessageFlags::default(),
        )),
    ];
    let first_graph = FakeGraph::new(
        [final_page(
            vec![DeltaChange::Upsert(build_folder("inbox-id", None, "Inbox"))],
            folder_delta,
        )],
        HashMap::from([(
            "inbox-id".into(),
            vec![final_page(messages.clone(), message_delta)],
        )]),
        HashMap::from([
            ("message-1".into(), vec![Ok(b"first".to_vec())]),
            ("message-2".into(), vec![Err(GraphError::Request)]),
        ]),
    );
    let maildir = MaildirStore::new(&account.maildir);

    let first_result = CloudSynchronizer::new(&first_graph)
        .sync_account(&account, &mut state, &maildir, false)
        .await;

    assert!(matches!(
        first_result,
        Err(SyncError::Graph(GraphError::Request))
    ));
    assert!(
        state
            .get_message("work", "message-1")
            .expect("message should load")
            .is_some()
    );
    assert_eq!(
        state
            .get_folder("work", "inbox-id")
            .expect("folder should load")
            .expect("inbox should exist")
            .message_delta_link,
        None
    );

    let second_graph = FakeGraph::new(
        [final_page(vec![], folder_delta)],
        HashMap::from([("inbox-id".into(), vec![final_page(messages, message_delta)])]),
        HashMap::from([("message-2".into(), vec![Ok(b"second".to_vec())])]),
    );
    CloudSynchronizer::new(&second_graph)
        .sync_account(&account, &mut state, &maildir, false)
        .await
        .expect("replayed synchronization should complete");

    assert_eq!(
        *second_graph
            .downloads
            .lock()
            .expect("downloads should be readable"),
        ["message-2"]
    );
    assert!(
        state
            .get_message("work", "message-2")
            .expect("message should load")
            .is_some()
    );
}

#[tokio::test]
async fn applies_incremental_updates_deletions_and_conflict_preservation() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let account = build_account(temp.path());
    let mut state = StateDatabase::open(&temp.path().join("state.sqlite3"))
        .expect("state database should open");
    let maildir = MaildirStore::new(&account.maildir);
    let initial_graph = FakeGraph::new(
        [final_page(
            vec![DeltaChange::Upsert(build_folder("inbox-id", None, "Inbox"))],
            "https://graph.microsoft.com/v1.0/folders/delta?$deltatoken=one",
        )],
        HashMap::from([(
            "inbox-id".into(),
            vec![final_page(
                vec![
                    DeltaChange::Upsert(build_message(
                        "updated-id",
                        "inbox-id",
                        "version-1",
                        MessageFlags::default(),
                    )),
                    DeltaChange::Upsert(build_message(
                        "deleted-id",
                        "inbox-id",
                        "version-1",
                        MessageFlags::default(),
                    )),
                ],
                "https://graph.microsoft.com/v1.0/messages/delta?$deltatoken=one",
            )],
        )]),
        HashMap::from([
            ("updated-id".into(), vec![Ok(b"baseline".to_vec())]),
            ("deleted-id".into(), vec![Ok(b"delete me".to_vec())]),
        ]),
    );
    CloudSynchronizer::new(&initial_graph)
        .sync_account(&account, &mut state, &maildir, false)
        .await
        .expect("initial sync should succeed");
    let updated = state
        .get_message("work", "updated-id")
        .expect("message should load")
        .expect("updated message should exist");
    std::fs::write(
        account.maildir.join(&updated.relative_path),
        b"local divergence",
    )
    .expect("tracked message should be editable");

    let incremental_graph = FakeGraph::new(
        [final_page(
            vec![],
            "https://graph.microsoft.com/v1.0/folders/delta?$deltatoken=two",
        )],
        HashMap::from([(
            "inbox-id".into(),
            vec![final_page(
                vec![
                    DeltaChange::Upsert(build_message(
                        "updated-id",
                        "inbox-id",
                        "version-2",
                        MessageFlags {
                            is_read: true,
                            follow_up: FollowUpState::NotFlagged,
                        },
                    )),
                    DeltaChange::Delete {
                        id: "deleted-id".into(),
                    },
                ],
                "https://graph.microsoft.com/v1.0/messages/delta?$deltatoken=two",
            )],
        )]),
        HashMap::from([("updated-id".into(), vec![Ok(b"cloud update".to_vec())])]),
    );

    let summary = CloudSynchronizer::new(&incremental_graph)
        .sync_account(&account, &mut state, &maildir, false)
        .await
        .expect("incremental sync should succeed");

    assert_eq!(summary.updated, 1);
    assert_eq!(summary.deleted, 1);
    assert_eq!(summary.conflicted, 1);
    assert!(
        state
            .get_message("work", "deleted-id")
            .expect("message should load")
            .is_none()
    );
    let conflicts: Vec<PathBuf> =
        std::fs::read_dir(account.maildir.join(".nochange-conflicts/cur"))
            .expect("conflict directory should exist")
            .map(|entry| entry.expect("entry should load").path())
            .collect();
    assert_eq!(conflicts.len(), 1);
}

#[tokio::test]
async fn dry_run_plans_changes_without_downloading_or_mutating_local_state() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let account = build_account(temp.path());
    let mut state = StateDatabase::open(&temp.path().join("state.sqlite3"))
        .expect("state database should open");
    let graph = FakeGraph::new(
        [final_page(
            vec![DeltaChange::Upsert(build_folder("inbox-id", None, "Inbox"))],
            "https://graph.microsoft.com/v1.0/folders/delta?$deltatoken=dry",
        )],
        HashMap::from([(
            "inbox-id".into(),
            vec![final_page(
                vec![DeltaChange::Upsert(build_message(
                    "message-id",
                    "inbox-id",
                    "version-1",
                    MessageFlags::default(),
                ))],
                "https://graph.microsoft.com/v1.0/messages/delta?$deltatoken=dry",
            )],
        )]),
        HashMap::new(),
    );

    let summary = CloudSynchronizer::new(&graph)
        .sync_account(
            &account,
            &mut state,
            &MaildirStore::new(&account.maildir),
            true,
        )
        .await
        .expect("dry run should plan without MIME downloads");

    assert_eq!(
        summary,
        SyncSummary {
            folders: 1,
            created: 1,
            updated: 0,
            deleted: 0,
            conflicted: 0,
        }
    );
    assert!(
        graph
            .downloads
            .lock()
            .expect("downloads should be readable")
            .is_empty()
    );
    assert!(!account.maildir.exists());
    assert!(
        state
            .list_folders("work")
            .expect("folders should load")
            .is_empty()
    );
    assert_eq!(
        state
            .get_folder_delta_link("work")
            .expect("checkpoint should load"),
        None
    );
}

#[tokio::test]
async fn newly_excluded_folders_leave_existing_mail_unmanaged_and_untouched() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let mut account = build_account(temp.path());
    let mut state = StateDatabase::open(&temp.path().join("state.sqlite3"))
        .expect("state database should open");
    let initial_graph = FakeGraph::new(
        [final_page(
            vec![DeltaChange::Upsert(build_folder("inbox-id", None, "Inbox"))],
            "https://graph.microsoft.com/v1.0/folders/delta?$deltatoken=one",
        )],
        HashMap::from([(
            "inbox-id".into(),
            vec![final_page(
                vec![DeltaChange::Upsert(build_message(
                    "message-id",
                    "inbox-id",
                    "version-1",
                    MessageFlags::default(),
                ))],
                "https://graph.microsoft.com/v1.0/messages/delta?$deltatoken=one",
            )],
        )]),
        HashMap::from([("message-id".into(), vec![Ok(b"keep me".to_vec())])]),
    );
    let maildir = MaildirStore::new(&account.maildir);
    CloudSynchronizer::new(&initial_graph)
        .sync_account(&account, &mut state, &maildir, false)
        .await
        .expect("initial sync should succeed");
    let message = state
        .get_message("work", "message-id")
        .expect("message should load")
        .expect("message should exist");
    account.folder_filter = FolderFilter::Exclude(vec!["inbox".into()]);
    let exclusion_graph = FakeGraph::new(
        [final_page(
            vec![],
            "https://graph.microsoft.com/v1.0/folders/delta?$deltatoken=two",
        )],
        HashMap::new(),
        HashMap::new(),
    );

    let summary = CloudSynchronizer::new(&exclusion_graph)
        .sync_account(&account, &mut state, &maildir, false)
        .await
        .expect("folder exclusion should succeed");

    assert_eq!(summary.deleted, 0);
    assert!(account.maildir.join(&message.relative_path).is_file());
    assert!(
        state
            .get_message("work", "message-id")
            .expect("message should load")
            .is_some()
    );
    assert!(
        !state
            .get_folder("work", "inbox-id")
            .expect("folder should load")
            .expect("folder should remain tracked")
            .is_selected
    );
}

#[tokio::test]
async fn reports_folder_message_page_and_apply_progress_without_remote_identifiers() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let account = build_account(temp.path());
    let mut state = StateDatabase::open(&temp.path().join("state.sqlite3"))
        .expect("state database should open");
    let graph = FakeGraph::new(
        [final_page(
            vec![DeltaChange::Upsert(build_folder(
                "private-folder-id",
                None,
                "Inbox",
            ))],
            "https://graph.microsoft.com/v1.0/folders/delta?$deltatoken=progress",
        )],
        HashMap::from([(
            "private-folder-id".into(),
            vec![final_page(
                vec![DeltaChange::Upsert(build_message(
                    "private-message-id",
                    "private-folder-id",
                    "version-1",
                    MessageFlags::default(),
                ))],
                "https://graph.microsoft.com/v1.0/messages/delta?$deltatoken=progress",
            )],
        )]),
        HashMap::from([("private-message-id".into(), vec![Ok(b"message".to_vec())])]),
    );
    let reporter = RecordingProgressReporter::default();

    CloudSynchronizer::new_with_reporter(&graph, &reporter)
        .sync_account(
            &account,
            &mut state,
            &MaildirStore::new(&account.maildir),
            false,
        )
        .await
        .expect("synchronization should succeed");

    let events = reporter
        .events
        .lock()
        .expect("progress events should be readable");
    assert!(events.contains(&SyncProgress::FolderPageStarted { page: 1 }));
    assert!(events.contains(&SyncProgress::FolderEnumerationCompleted {
        discovered: 1,
        selected: 1,
    }));
    assert!(events.contains(&SyncProgress::MessageFolderStarted {
        folder: "Inbox".into(),
        position: 1,
        total: 1,
        resumed: false,
        estimated_total: Some(1),
    }));
    assert!(events.contains(&SyncProgress::MessagePageCompleted {
        folder: "Inbox".into(),
        page: 1,
        changes: 1,
        accumulated: 1,
        complete: true,
        estimated_total: Some(1),
    }));
    assert!(events.contains(&SyncProgress::MessageApplyStarted { total: 1 }));
    assert!(events.contains(&SyncProgress::MessageApplyProgress {
        position: 1,
        total: 1,
        action: nochange::sync::SyncActionKind::Upsert,
    }));
    let diagnostics = format!("{events:?}");
    assert!(!diagnostics.contains("private-folder-id"));
    assert!(!diagnostics.contains("private-message-id"));
}
