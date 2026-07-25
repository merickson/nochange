use nochange::config::{AccountSelection, AppConfig, AppPaths, ConfigError, FolderFilter};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn write_config(temp_dir: &TempDir, contents: &str) -> PathBuf {
    let path = temp_dir.path().join("nochange.conf");
    fs::write(&path, contents).expect("test configuration should be writable");
    path
}

fn valid_config(maildir: &Path) -> String {
    format!(
        "[global]\naccounts = work\n\n[work]\nmaildir = {}\nuser = me@example.com\nclientid = client-id\nfolderexclude = Journal, Archive/Old\n",
        maildir.display()
    )
}

#[test]
fn example_configuration_matches_the_supported_contract() {
    let temp_dir = TempDir::new().expect("temporary directory should be created");
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("nochange.conf");

    let config = AppConfig::load_from(&example, temp_dir.path())
        .expect("the distributed example configuration should remain valid");

    assert_eq!(config.account_names(), ["o365_1"]);
}

#[test]
fn builds_config_and_state_paths_from_xdg_roots() {
    let paths = AppPaths::build_from_roots(Path::new("/config"), Path::new("/state"));

    assert_eq!(
        paths.config_file,
        PathBuf::from("/config/nochange/nochange.conf")
    );
    assert_eq!(
        paths.state_database,
        PathBuf::from("/state/nochange/state.sqlite3")
    );
}

#[test]
fn builds_paths_from_xdg_values_and_home_fallbacks() {
    let explicit = AppPaths::build_from_environment(
        Some(OsStr::new("/xdg-config")),
        Some(OsStr::new("/xdg-state")),
        Path::new("/home/user"),
    );
    let fallback = AppPaths::build_from_environment(None, None, Path::new("/home/user"));

    assert_eq!(
        explicit,
        AppPaths::build_from_roots(Path::new("/xdg-config"), Path::new("/xdg-state"))
    );
    assert_eq!(
        fallback,
        AppPaths::build_from_roots(
            Path::new("/home/user/.config"),
            Path::new("/home/user/.local/state")
        )
    );
    assert!(AppPaths::discover().is_ok());
}

#[test]
fn loads_valid_configuration_with_defaults_and_normalized_filters() {
    let temp_dir = TempDir::new().expect("temporary directory should be created");
    let maildir = temp_dir.path().join("mail");
    let path = write_config(&temp_dir, &valid_config(&maildir));

    let config =
        AppConfig::load_from(&path, temp_dir.path()).expect("valid configuration should load");
    let account = config
        .get_account("work")
        .expect("configured account should exist");

    assert_eq!(config.account_names(), ["work"]);
    assert_eq!(
        account.maildir,
        temp_dir
            .path()
            .canonicalize()
            .expect("temporary root should be canonicalizable")
            .join("mail")
    );
    assert_eq!(account.user, "me@example.com");
    assert_eq!(account.client_id, "client-id");
    assert_eq!(account.tenant, "organizations");
    assert_eq!(account.folder_separator, ".");
    assert_eq!(
        account.folder_filter,
        FolderFilter::Exclude(vec!["journal".into(), "archive/old".into()])
    );
    assert!(!account.is_folder_selected("JOURNAL/2025"));
    assert!(!account.is_folder_selected("archive/OLD/receipts"));
    assert!(account.is_folder_selected("Archive"));
    assert!(account.is_folder_selected("Journalism"));
}

#[test]
fn expands_home_and_normalizes_nonexistent_maildir_paths() {
    let temp_dir = TempDir::new().expect("temporary directory should be created");
    let path = write_config(
        &temp_dir,
        "[global]\naccounts = work\n[work]\nmaildir = ~/mail/../mail/work\nuser = me@example.com\nclientid = client-id\n",
    );

    let config =
        AppConfig::load_from(&path, temp_dir.path()).expect("home-relative path should load");

    assert_eq!(
        config
            .get_account("work")
            .expect("account should exist")
            .maildir,
        temp_dir
            .path()
            .canonicalize()
            .expect("temporary root should be canonicalizable")
            .join("mail/work")
    );
}

#[test]
fn applies_include_filters_to_complete_subtrees() {
    let temp_dir = TempDir::new().expect("temporary directory should be created");
    let maildir = temp_dir.path().join("mail");
    let contents = format!(
        "[global]\naccounts = work\n[work]\nmaildir = {}\nuser = me@example.com\nclientid = client-id\ntenant = common\nfolderseparator = _\nfolderinclude = Inbox, Projects/One\n",
        maildir.display()
    );
    let path = write_config(&temp_dir, &contents);

    let config = AppConfig::load_from(&path, temp_dir.path()).expect("include filter should load");
    let account = config.get_account("work").expect("account should exist");

    assert_eq!(account.tenant, "common");
    assert_eq!(account.folder_separator, "_");
    assert!(account.is_folder_selected("inbox/subfolder"));
    assert!(account.is_folder_selected("PROJECTS/ONE"));
    assert!(!account.is_folder_selected("Projects"));
    assert!(!account.is_folder_selected("InboxOther"));
}

#[test]
fn selects_all_accounts_or_one_named_account() {
    let temp_dir = TempDir::new().expect("temporary directory should be created");
    let first = temp_dir.path().join("first");
    let second = temp_dir.path().join("second");
    let contents = format!(
        "[global]\naccounts = first, second\n[first]\nmaildir = {}\nuser = first@example.com\nclientid = first-id\n[second]\nmaildir = {}\nuser = second@example.com\nclientid = second-id\n",
        first.display(),
        second.display()
    );
    let path = write_config(&temp_dir, &contents);
    let config = AppConfig::load_from(&path, temp_dir.path())
        .expect("multi-account configuration should load");

    assert_eq!(
        config
            .get_selected_accounts(AccountSelection::All)
            .expect("all accounts should resolve")
            .len(),
        2
    );
    assert_eq!(
        config
            .get_selected_accounts(AccountSelection::Named("second"))
            .expect("known account should resolve")[0]
            .name,
        "second"
    );
    assert!(
        config
            .get_selected_accounts(AccountSelection::Named("missing"))
            .is_err()
    );
}

#[test]
fn rejects_invalid_configuration_contracts() {
    let temp_dir = TempDir::new().expect("temporary directory should be created");
    let cases = [
        (
            "missing required account value",
            "[global]\naccounts = work\n[work]\nmaildir = /tmp/work\nclientid = id\n",
        ),
        (
            "client secrets",
            "[global]\naccounts = work\n[work]\nmaildir = /tmp/work\nuser = me@example.com\nclientid = id\nclientsecret = secret\n",
        ),
        (
            "include and exclude together",
            "[global]\naccounts = work\n[work]\nmaildir = /tmp/work\nuser = me@example.com\nclientid = id\nfolderinclude = Inbox\nfolderexclude = Spam\n",
        ),
        (
            "duplicate accounts",
            "[global]\naccounts = work,work\n[work]\nmaildir = /tmp/work\nuser = me@example.com\nclientid = id\n",
        ),
        (
            "unknown global keys",
            "[global]\naccounts = work\nsurprise = yes\n[work]\nmaildir = /tmp/work\nuser = me@example.com\nclientid = id\n",
        ),
        (
            "unknown account keys",
            "[global]\naccounts = work\n[work]\nmaildir = /tmp/work\nuser = me@example.com\nclientid = id\nsurprise = yes\n",
        ),
        (
            "unknown sections",
            "[global]\naccounts = work\n[work]\nmaildir = /tmp/work\nuser = me@example.com\nclientid = id\n[extra]\nfoo = bar\n",
        ),
        (
            "unsafe slash separator",
            "[global]\naccounts = work\n[work]\nmaildir = /tmp/work\nuser = me@example.com\nclientid = id\nfolderseparator = /\n",
        ),
        (
            "multi-character separator",
            "[global]\naccounts = work\n[work]\nmaildir = /tmp/work\nuser = me@example.com\nclientid = id\nfolderseparator = ab\n",
        ),
    ];

    for (name, contents) in cases {
        let path = write_config(&temp_dir, contents);
        assert!(
            AppConfig::load_from(&path, temp_dir.path()).is_err(),
            "case should be rejected: {name}"
        );
    }
}

#[test]
fn rejects_duplicate_or_overlapping_maildir_roots() {
    let temp_dir = TempDir::new().expect("temporary directory should be created");
    let root = temp_dir.path().join("mail");
    let child = root.join("nested");
    let contents = format!(
        "[global]\naccounts = first, second\n[first]\nmaildir = {}\nuser = first@example.com\nclientid = first-id\n[second]\nmaildir = {}\nuser = second@example.com\nclientid = second-id\n",
        root.display(),
        child.display()
    );
    let path = write_config(&temp_dir, &contents);

    assert!(AppConfig::load_from(&path, temp_dir.path()).is_err());
}

#[test]
fn loads_exact_home_and_relative_maildir_paths() {
    let temp_dir = TempDir::new().expect("temporary directory should be created");
    let home_path = write_config(
        &temp_dir,
        "[global]\naccounts = home\n[home]\nmaildir = ~\nuser = me@example.com\nclientid = id\n",
    );
    let home_config =
        AppConfig::load_from(&home_path, temp_dir.path()).expect("an exact home path should load");
    assert_eq!(
        home_config
            .get_account("home")
            .expect("home account should exist")
            .maildir,
        temp_dir
            .path()
            .canonicalize()
            .expect("temporary root should be canonicalizable")
    );

    let relative_path = write_config(
        &temp_dir,
        "[global]\naccounts = relative\n[relative]\nmaildir = local-mail\nuser = me@example.com\nclientid = id\n",
    );
    let relative_config = AppConfig::load_from(&relative_path, temp_dir.path())
        .expect("a relative path should resolve from the configuration directory");
    assert_eq!(
        relative_config
            .get_account("relative")
            .expect("relative account should exist")
            .maildir,
        temp_dir
            .path()
            .canonicalize()
            .expect("temporary root should be canonicalizable")
            .join("local-mail")
    );
}

#[test]
fn selects_every_folder_without_a_filter() {
    let temp_dir = TempDir::new().expect("temporary directory should be created");
    let path = write_config(
        &temp_dir,
        "[global]\naccounts = work\n[work]\nmaildir = ~/mail\nuser = me@example.com\nclientid = id\n",
    );
    let config = AppConfig::load_from(&path, temp_dir.path())
        .expect("configuration without a filter should load");

    assert!(
        config
            .get_account("work")
            .expect("account should exist")
            .is_folder_selected("Any/Folder")
    );
}

#[test]
fn reports_specific_structural_and_path_errors() {
    let temp_dir = TempDir::new().expect("temporary directory should be created");
    let missing_file = temp_dir.path().join("missing.conf");
    assert!(matches!(
        AppConfig::load_from(&missing_file, temp_dir.path()),
        Err(ConfigError::Parse { .. })
    ));

    let cases = [
        (
            "missing global",
            "[work]\nmaildir = /tmp/work\nuser = me@example.com\nclientid = id\n",
            "missing_section",
        ),
        (
            "empty accounts",
            "[global]\naccounts = work,\n[work]\nmaildir = /tmp/work\nuser = me@example.com\nclientid = id\n",
            "missing_key",
        ),
        ("missing accounts key", "[global]\n", "missing_key"),
        (
            "missing declared section",
            "[global]\naccounts = work\n",
            "missing_section",
        ),
        (
            "missing maildir",
            "[global]\naccounts = work\n[work]\nuser = me@example.com\nclientid = id\n",
            "missing_key",
        ),
        (
            "missing client id",
            "[global]\naccounts = work\n[work]\nmaildir = /tmp/work\nuser = me@example.com\n",
            "missing_key",
        ),
        (
            "unsupported named home",
            "[global]\naccounts = work\n[work]\nmaildir = ~someone/mail\nuser = me@example.com\nclientid = id\n",
            "invalid_path",
        ),
    ];

    for (name, contents, expected) in cases {
        let path = write_config(&temp_dir, contents);
        let error = AppConfig::load_from(&path, temp_dir.path())
            .expect_err("invalid configuration should fail");
        let actual = match error {
            ConfigError::MissingSection(_) => "missing_section",
            ConfigError::MissingKey { .. } => "missing_key",
            ConfigError::InvalidPath { .. } => "invalid_path",
            other => panic!("unexpected error for {name}: {other}"),
        };
        assert_eq!(actual, expected, "wrong error for {name}");
    }
}
