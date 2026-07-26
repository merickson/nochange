use nochange::maildir::{MaildirError, MaildirStore, get_encoded_folder_path, get_maildir_key};
use nochange::model::{FollowUpState, MessageFlags};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn unread_flags() -> MessageFlags {
    MessageFlags::default()
}

fn read_flagged_flags() -> MessageFlags {
    MessageFlags {
        is_read: true,
        follow_up: FollowUpState::Flagged,
    }
}

#[test]
fn encodes_remote_folder_components_for_the_configured_separator() {
    assert_eq!(
        get_encoded_folder_path("Projects/One.Two/50% *", '.').expect("folder path should encode"),
        "Projects.One%2ETwo.50%25 %2A"
    );
    assert_eq!(
        get_encoded_folder_path("Réunions/正常", '.').expect("Unicode path should encode"),
        "Réunions.正常"
    );
    assert_eq!(
        get_encoded_folder_path("Sent Items/Family & Friends (2026)", '.')
            .expect("readable punctuation should remain"),
        "Sent Items.Family & Friends (2026)"
    );
    assert_eq!(
        get_encoded_folder_path("Trailing /Dot.", '_').expect("unsafe endings should encode"),
        "Trailing%20_Dot%2E"
    );
    assert!(get_encoded_folder_path("", '.').is_err());
    assert!(get_encoded_folder_path("Inbox//Nested", '.').is_err());
}

#[test]
fn derives_stable_maildir_keys_without_exposing_graph_identifiers() {
    let first = get_maildir_key("me@example.com", "graph-message-secret");
    let repeated = get_maildir_key("me@example.com", "graph-message-secret");
    let other = get_maildir_key("other@example.com", "graph-message-secret");

    assert_eq!(first, repeated);
    assert_ne!(first, other);
    assert_eq!(first.len(), 64);
    assert!(first.chars().all(|character| character.is_ascii_hexdigit()));
    assert!(!first.contains("graph-message-secret"));
}

#[test]
fn creates_private_maildir_directories_and_stages_downloads_in_tmp() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let store = MaildirStore::new(temp.path());

    let staging = store
        .prepare_download("Inbox", "message-key")
        .expect("download path should be prepared");

    assert_eq!(
        staging,
        temp.path().join("Inbox/tmp/.nochange-message-key.download")
    );
    for directory in ["tmp", "new", "cur"] {
        assert!(temp.path().join("Inbox").join(directory).is_dir());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(temp.path().join("Inbox"))
                .expect("folder metadata should load")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}

#[test]
fn configures_maildir_fsync_for_one_store() {
    let temp = TempDir::new().expect("temporary directory should be created");

    let durable = MaildirStore::new(temp.path());
    let buffered = MaildirStore::new_with_fsync(temp.path(), false);

    assert!(durable.get_fsync_enabled());
    assert!(!buffered.get_fsync_enabled());
}

#[test]
fn scans_only_known_message_keys_and_decodes_supported_flags() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let store = MaildirStore::new(temp.path());
    store
        .create_folder("Inbox")
        .expect("Inbox should be created");
    store
        .create_folder("Archive")
        .expect("Archive should be created");
    fs::write(temp.path().join("Inbox/cur/tracked-key:2,DFPS"), b"tracked")
        .expect("tracked message should be created");
    fs::write(temp.path().join("Archive/new/moved-key"), b"moved")
        .expect("moved message should be created");
    fs::write(temp.path().join("Inbox/new/untracked-key"), b"untracked")
        .expect("untracked message should be created");

    let scanned = store
        .scan_tracked_messages(
            &["Inbox".into(), "Archive".into()],
            &BTreeSet::from(["tracked-key".into(), "moved-key".into()]),
        )
        .expect("managed folders should scan");

    assert_eq!(scanned["tracked-key"].len(), 1);
    assert_eq!(
        scanned["tracked-key"][0].relative_path,
        "Inbox/cur/tracked-key:2,DFPS"
    );
    assert_eq!(
        scanned["tracked-key"][0].flags,
        MessageFlags {
            is_read: true,
            follow_up: FollowUpState::Flagged,
        }
    );
    assert_eq!(
        scanned["moved-key"][0].relative_path,
        "Archive/new/moved-key"
    );
    assert!(!scanned.contains_key("untracked-key"));
}

#[test]
fn commits_unread_and_flagged_messages_to_the_correct_maildir_locations() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let store = MaildirStore::new(temp.path());
    let unread_staging = store
        .prepare_download("Inbox", "unread-key")
        .expect("unread download should prepare");
    fs::write(&unread_staging, b"Subject: unread\r\n\r\nBody\r\n")
        .expect("staging file should be writable");
    let flagged_staging = store
        .prepare_download("Inbox", "flagged-key")
        .expect("flagged download should prepare");
    fs::write(&flagged_staging, b"Subject: flagged\r\n\r\nBody\r\n")
        .expect("staging file should be writable");

    let unread = store
        .commit_download("Inbox", "unread-key", unread_flags(), &unread_staging)
        .expect("unread message should commit");
    let flagged = store
        .commit_download(
            "Inbox",
            "flagged-key",
            read_flagged_flags(),
            &flagged_staging,
        )
        .expect("flagged message should commit");

    assert_eq!(unread.relative_path, "Inbox/new/unread-key");
    assert_eq!(flagged.relative_path, "Inbox/cur/flagged-key:2,FS");
    assert_eq!(unread.mime_hash.len(), 64);
    assert_eq!(
        fs::read(temp.path().join(&unread.relative_path))
            .expect("delivered message should be readable"),
        b"Subject: unread\r\n\r\nBody\r\n"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(temp.path().join(&unread.relative_path))
                .expect("message metadata should load")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn treats_an_identical_delivery_as_an_idempotent_replay() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let store = MaildirStore::new(temp.path());
    let first_staging = store
        .prepare_download("Inbox", "message-key")
        .expect("first download should prepare");
    fs::write(&first_staging, b"same bytes").expect("staging file should be writable");
    let first = store
        .commit_download("Inbox", "message-key", unread_flags(), &first_staging)
        .expect("first delivery should commit");
    let replay_staging = store
        .prepare_download("Inbox", "message-key")
        .expect("replay download should prepare");
    fs::write(&replay_staging, b"same bytes").expect("staging file should be writable");

    let replay = store
        .commit_download("Inbox", "message-key", unread_flags(), &replay_staging)
        .expect("identical replay should succeed");

    assert_eq!(replay, first);
    assert!(!replay_staging.exists());
}

#[test]
fn refuses_to_replace_a_colliding_untracked_message() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let store = MaildirStore::new(temp.path());
    let first_staging = store
        .prepare_download("Inbox", "message-key")
        .expect("first download should prepare");
    fs::write(&first_staging, b"original").expect("staging file should be writable");
    store
        .commit_download("Inbox", "message-key", unread_flags(), &first_staging)
        .expect("first delivery should commit");
    let collision_staging = store
        .prepare_download("Inbox", "message-key")
        .expect("collision download should prepare");
    fs::write(&collision_staging, b"different").expect("staging file should be writable");

    assert!(matches!(
        store.commit_download("Inbox", "message-key", unread_flags(), &collision_staging),
        Err(MaildirError::DestinationCollision { .. })
    ));
    assert_eq!(
        fs::read(temp.path().join("Inbox/new/message-key"))
            .expect("original message should remain readable"),
        b"original"
    );
}

#[test]
fn updates_supported_flags_while_preserving_unsupported_flags() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let store = MaildirStore::new(temp.path());
    store
        .create_folder("Inbox")
        .expect("maildir should be created");
    let old_relative = "Inbox/cur/message-key:2,DPR";
    fs::write(temp.path().join(old_relative), b"message").expect("message should be writable");

    let renamed = store
        .set_flags(old_relative, read_flagged_flags())
        .expect("flags should update");

    assert_eq!(renamed, "Inbox/cur/message-key:2,DFPRS");
    assert!(temp.path().join(&renamed).is_file());
    assert!(!temp.path().join(old_relative).exists());
}

#[test]
fn removes_tracked_paths_idempotently_and_rejects_path_escape() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let store = MaildirStore::new(temp.path());
    store
        .create_folder("Inbox")
        .expect("maildir should be created");
    fs::write(temp.path().join("Inbox/new/message-key"), b"message")
        .expect("message should be writable");

    store
        .remove_message("Inbox/new/message-key")
        .expect("message should be removed");
    store
        .remove_message("Inbox/new/message-key")
        .expect("replayed removal should succeed");

    for unsafe_path in ["/tmp/message", "../message", "Inbox/../../message"] {
        assert!(matches!(
            store.remove_message(unsafe_path),
            Err(MaildirError::UnsafePath)
        ));
    }
    assert!(
        !Path::new(temp.path())
            .join("Inbox/new/message-key")
            .exists()
    );
}

#[test]
fn atomically_replaces_a_clean_tracked_message() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let store = MaildirStore::new(temp.path());
    let original_staging = store
        .prepare_download("Inbox", "message-key")
        .expect("original download should prepare");
    fs::write(&original_staging, b"original bytes").expect("staging should be writable");
    let original = store
        .commit_download("Inbox", "message-key", unread_flags(), &original_staging)
        .expect("original should commit");
    let update_staging = store
        .prepare_download("Inbox", "message-key")
        .expect("update should prepare");
    fs::write(&update_staging, b"updated bytes").expect("staging should be writable");

    let update = store
        .replace_tracked(
            &original.relative_path,
            &original.mime_hash,
            "Inbox",
            "message-key",
            read_flagged_flags(),
            &update_staging,
        )
        .expect("clean tracked update should replace");

    assert_eq!(update.delivered.relative_path, "Inbox/cur/message-key:2,FS");
    assert_eq!(update.conflict_path, None);
    assert_eq!(
        fs::read(temp.path().join(&update.delivered.relative_path))
            .expect("updated message should be readable"),
        b"updated bytes"
    );
    assert!(!temp.path().join(&original.relative_path).exists());
}

#[test]
fn moves_identical_mime_to_its_new_remote_folder() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let store = MaildirStore::new(temp.path());
    let original_staging = store
        .prepare_download("Inbox", "message-key")
        .expect("original download should prepare");
    fs::write(&original_staging, b"unchanged bytes").expect("staging should be writable");
    let original = store
        .commit_download("Inbox", "message-key", unread_flags(), &original_staging)
        .expect("original should commit");
    let moved_staging = store
        .prepare_download("Deleted Items", "message-key")
        .expect("moved download should prepare");
    fs::write(&moved_staging, b"unchanged bytes").expect("staging should be writable");

    let moved = store
        .replace_tracked(
            &original.relative_path,
            &original.mime_hash,
            "Deleted Items",
            "message-key",
            unread_flags(),
            &moved_staging,
        )
        .expect("identical MIME should move folders");

    assert_eq!(
        moved.delivered.relative_path,
        "Deleted Items/new/message-key"
    );
    assert!(!temp.path().join(original.relative_path).exists());
    assert!(temp.path().join("Deleted Items/new/message-key").is_file());
}

#[test]
fn preserves_a_divergent_local_message_before_cloud_replacement() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let store = MaildirStore::new(temp.path());
    let original_staging = store
        .prepare_download("Inbox", "message-key")
        .expect("original download should prepare");
    fs::write(&original_staging, b"cloud baseline").expect("staging should be writable");
    let original = store
        .commit_download("Inbox", "message-key", unread_flags(), &original_staging)
        .expect("original should commit");
    fs::write(
        temp.path().join(&original.relative_path),
        b"locally edited bytes",
    )
    .expect("tracked file should be editable");
    let update_staging = store
        .prepare_download("Inbox", "message-key")
        .expect("update should prepare");
    fs::write(&update_staging, b"new cloud bytes").expect("staging should be writable");

    let update = store
        .replace_tracked(
            &original.relative_path,
            &original.mime_hash,
            "Inbox",
            "message-key",
            unread_flags(),
            &update_staging,
        )
        .expect("cloud update should preserve divergence");

    let conflict = update
        .conflict_path
        .expect("divergent content should produce a conflict copy");
    assert!(conflict.starts_with(".nochange-conflicts/cur/message-key.conflict-"));
    assert_eq!(
        fs::read(temp.path().join(conflict)).expect("conflict should be readable"),
        b"locally edited bytes"
    );
    assert_eq!(
        fs::read(temp.path().join(update.delivered.relative_path))
            .expect("cloud message should be readable"),
        b"new cloud bytes"
    );
}

#[test]
fn preserves_a_divergent_local_message_before_cloud_deletion() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let store = MaildirStore::new(temp.path());
    let staging = store
        .prepare_download("Inbox", "message-key")
        .expect("download should prepare");
    fs::write(&staging, b"cloud baseline").expect("staging should be writable");
    let delivered = store
        .commit_download("Inbox", "message-key", unread_flags(), &staging)
        .expect("message should commit");
    fs::write(
        temp.path().join(&delivered.relative_path),
        b"locally edited bytes",
    )
    .expect("tracked file should be editable");

    let conflict = store
        .remove_tracked(
            &delivered.relative_path,
            &delivered.mime_hash,
            "message-key",
        )
        .expect("cloud deletion should preserve divergence")
        .expect("divergent content should produce a conflict copy");

    assert!(!temp.path().join(delivered.relative_path).exists());
    assert_eq!(
        fs::read(temp.path().join(conflict)).expect("conflict should be readable"),
        b"locally edited bytes"
    );
}
