use nochange::model::{FollowUpState, MessageFlags};
use nochange::state::{
    StateDatabase, StateError, StoredFolder, StoredMessage, get_account_lock_path,
};
use std::path::Path;
use tempfile::TempDir;

fn build_folder(id: &str, path: &str) -> StoredFolder {
    StoredFolder {
        id: id.into(),
        parent_id: None,
        display_name: path.into(),
        remote_path: path.into(),
        local_path: path.into(),
        is_selected: true,
        is_hidden: false,
        total_item_count: 42,
        message_delta_link: None,
    }
}

fn build_message(id: &str, folder_id: &str) -> StoredMessage {
    StoredMessage {
        id: id.into(),
        folder_id: folder_id.into(),
        maildir_key: format!("key-{id}"),
        relative_path: format!("Inbox/cur/key-{id}:2,S"),
        mime_hash: format!("hash-{id}"),
        remote_version: format!("version-{id}"),
        internet_message_id: Some(format!("<{id}@example.com>")),
        flags: MessageFlags {
            is_read: true,
            follow_up: FollowUpState::Flagged,
        },
    }
}

#[test]
fn creates_a_private_versioned_wal_database() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let path = temp.path().join("state/nochange.sqlite3");

    let database = StateDatabase::open(&path).expect("database should be created");

    assert_eq!(
        database.get_schema_version().expect("version should load"),
        2
    );
    assert_eq!(
        database
            .get_journal_mode()
            .expect("journal mode should load"),
        "wal"
    );
    assert_eq!(
        database
            .get_synchronous_level()
            .expect("synchronous level should load"),
        2
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path)
                .expect("database metadata should load")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn opens_a_database_without_sqlite_fsync_when_requested() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let path = temp.path().join("state/nochange.sqlite3");

    let database =
        StateDatabase::open_with_fsync(&path, false).expect("database should open without fsync");

    assert_eq!(
        database
            .get_synchronous_level()
            .expect("synchronous level should load"),
        0
    );
}

#[test]
fn migrates_version_one_folders_with_an_unknown_item_count() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let path = temp.path().join("state.sqlite3");
    let connection = rusqlite::Connection::open(&path).expect("database should be created");
    connection
        .execute_batch(
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
                is_selected INTEGER NOT NULL,
                is_hidden INTEGER NOT NULL,
                message_delta_link TEXT,
                PRIMARY KEY (account_name, id)
             );
             INSERT INTO accounts(name, user_identity)
                VALUES ('work', 'me@example.com');
             INSERT INTO folders(
                account_name, id, display_name, remote_path, local_path,
                is_selected, is_hidden
             ) VALUES ('work', 'inbox-id', 'Inbox', 'Inbox', 'Inbox', 1, 0);
             PRAGMA user_version = 1;",
        )
        .expect("version one schema should be created");
    drop(connection);

    let database = StateDatabase::open(&path).expect("version one database should migrate");
    let folder = database
        .get_folder("work", "inbox-id")
        .expect("folder should load")
        .expect("folder should exist");

    assert_eq!(
        database.get_schema_version().expect("version should load"),
        2
    );
    assert_eq!(folder.total_item_count, 0);
}

#[test]
fn pins_each_local_account_to_its_verified_identity() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let mut database =
        StateDatabase::open(&temp.path().join("state.sqlite3")).expect("database should open");

    database
        .ensure_account("work", "me@example.com")
        .expect("first identity should be recorded");
    database
        .ensure_account("work", "ME@example.com")
        .expect("identity comparison should be case-insensitive");

    assert!(matches!(
        database.ensure_account("work", "other@example.com"),
        Err(StateError::AccountIdentityMismatch { .. })
    ));
}

#[test]
fn stores_folder_metadata_and_opaque_delta_links() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let mut database =
        StateDatabase::open(&temp.path().join("state.sqlite3")).expect("database should open");
    database
        .ensure_account("work", "me@example.com")
        .expect("account should be recorded");
    let mut inbox = build_folder("inbox-id", "Inbox");
    inbox.message_delta_link = Some(
        "https://graph.microsoft.com/v1.0/me/mailFolders/inbox/messages/delta?$deltatoken=A%2fb"
            .into(),
    );

    database
        .upsert_folder("work", &inbox)
        .expect("folder should be stored");
    database
        .set_folder_delta_link(
            "work",
            "https://graph.microsoft.com/v1.0/me/mailFolders/delta?$deltatoken=opaque%2Bvalue",
        )
        .expect("folder checkpoint should be stored");

    assert_eq!(
        database
            .get_folder_delta_link("work")
            .expect("checkpoint should load")
            .as_deref(),
        Some("https://graph.microsoft.com/v1.0/me/mailFolders/delta?$deltatoken=opaque%2Bvalue")
    );
    assert_eq!(
        database
            .get_folder("work", "inbox-id")
            .expect("folder lookup should succeed"),
        Some(inbox.clone())
    );
    assert_eq!(
        database.list_folders("work").expect("folders should load"),
        [inbox]
    );
}

#[test]
fn stores_message_baselines_and_cascades_deleted_folders() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let mut database =
        StateDatabase::open(&temp.path().join("state.sqlite3")).expect("database should open");
    database
        .ensure_account("work", "me@example.com")
        .expect("account should be recorded");
    database
        .upsert_folder("work", &build_folder("inbox-id", "Inbox"))
        .expect("folder should be stored");
    let message = build_message("message-id", "inbox-id");

    database
        .upsert_message("work", &message)
        .expect("message should be stored");

    assert_eq!(
        database
            .get_message("work", "message-id")
            .expect("message lookup should succeed"),
        Some(message.clone())
    );
    assert_eq!(
        database
            .list_messages("work", "inbox-id")
            .expect("messages should load"),
        [message]
    );

    database
        .delete_folder("work", "inbox-id")
        .expect("folder should be deleted");
    assert_eq!(
        database
            .get_message("work", "message-id")
            .expect("message lookup should succeed"),
        None
    );
}

#[test]
fn deletes_individual_message_baselines_idempotently() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let mut database =
        StateDatabase::open(&temp.path().join("state.sqlite3")).expect("database should open");
    database
        .ensure_account("work", "me@example.com")
        .expect("account should be recorded");
    database
        .upsert_folder("work", &build_folder("inbox-id", "Inbox"))
        .expect("folder should be stored");
    database
        .upsert_message("work", &build_message("message-id", "inbox-id"))
        .expect("message should be stored");

    database
        .delete_message("work", "message-id")
        .expect("message should be deleted");
    database
        .delete_message("work", "message-id")
        .expect("replayed deletion should succeed");

    assert!(
        database
            .list_messages("work", "inbox-id")
            .expect("messages should load")
            .is_empty()
    );
}

#[test]
fn rejects_a_database_from_a_newer_nochange_version() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let path = temp.path().join("newer.sqlite3");
    let connection = rusqlite::Connection::open(&path).expect("database should be created");
    connection
        .pragma_update(None, "user_version", 99)
        .expect("future version should be set");
    drop(connection);

    assert!(matches!(
        StateDatabase::open(&path),
        Err(StateError::UnsupportedSchemaVersion(99))
    ));
}

#[test]
fn account_locks_are_exclusive_and_have_stable_safe_paths() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let lock_path = get_account_lock_path(temp.path(), "Work / Personal");
    assert_eq!(lock_path.parent(), Some(temp.path()));
    assert!(
        lock_path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("account-") && name.ends_with(".lock"))
    );

    let first = nochange::state::AccountLock::acquire(&lock_path)
        .expect("first account lock should be acquired");
    assert!(matches!(
        nochange::state::AccountLock::acquire(&lock_path),
        Err(StateError::AccountLocked)
    ));
    drop(first);
    nochange::state::AccountLock::acquire(&lock_path)
        .expect("lock should be released when its guard drops");
}

#[test]
fn state_path_helpers_do_not_require_existing_directories() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let state_root = temp.path().join("missing");
    let lock_path = get_account_lock_path(&state_root, "work");

    assert_eq!(lock_path.parent(), Some(Path::new(&state_root)));
}
