//! Top-level command dispatch and concrete adapter wiring.

use crate::auth::{
    EntraEndpoints, EntraTokenExchange, KeyringCredentialStore, SystemBrowserLauncher, TokenManager,
};
use crate::cli::{Cli, Command, InitArgs, SendArgs, SyncArgs};
use crate::config::{AccountSelection, AppConfig, AppPaths, ConfigError};
use crate::error::AppError;
use crate::graph::{GraphError, GraphTransport, TokioSleeper};
use crate::init::{
    ConsoleLoginPrompter, EntraAccountAuthenticator, GraphProfileVerifier, InitRunner, LoginMethod,
    ProfileVerifier,
};
use crate::maildir::MaildirStore;
use crate::send::{SendError, SendOptions, prepare_message};
use crate::state::{AccountLock, StateDatabase, StateError, get_account_lock_path};
use crate::sync::{
    CloudSynchronizer, LocalLocationActionKind, SyncActionKind, SyncError, SyncProgress,
    SyncProgressReporter, SyncSummary,
};
use directories::BaseDirs;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

/// Execute a parsed command with production adapters.
pub async fn run(cli: Cli) -> Result<(), AppError> {
    let Cli {
        config,
        verbose,
        command,
    } = cli;
    match command {
        Command::Init(arguments) => run_init(config.as_deref(), &arguments).await,
        Command::Sync(arguments) => run_sync(config.as_deref(), &arguments, verbose).await,
        Command::Send(arguments) => run_send(config.as_deref(), &arguments).await,
    }
}

async fn run_send(config_override: Option<&Path>, arguments: &SendArgs) -> Result<(), AppError> {
    let paths = AppPaths::discover()?;
    let config_path = config_override.unwrap_or(&paths.config_file);
    let home = BaseDirs::new().ok_or(ConfigError::HomeDirectoryUnavailable)?;
    let config = AppConfig::load_from(config_path, home.home_dir())?;
    let account = match arguments.account.as_deref() {
        Some(name) => config
            .get_selected_accounts(AccountSelection::Named(name))?
            .into_iter()
            .next()
            .ok_or_else(|| AppError::Software("selected account disappeared".into()))?,
        None if config.account_names().len() == 1 => config
            .get_selected_accounts(AccountSelection::All)?
            .into_iter()
            .next()
            .ok_or_else(|| AppError::Software("configured account disappeared".into()))?,
        None => {
            return Err(AppError::Usage(
                "multiple accounts are configured; select one with -a ACCOUNT".into(),
            ));
        }
    };
    let configured_sender = account.user.clone();
    let envelope_sender = arguments.from.clone();
    let read_recipients_from_headers = arguments.read_recipients_from_headers;
    let envelope_recipients = arguments.recipients.clone();
    let message = tokio::task::spawn_blocking(move || {
        prepare_message(
            std::io::stdin().lock(),
            &SendOptions {
                configured_sender: &configured_sender,
                envelope_sender: envelope_sender.as_deref(),
                read_recipients_from_headers,
                envelope_recipients: &envelope_recipients,
            },
        )
    })
    .await
    .map_err(|_| AppError::Software("message preparation task failed".into()))??;

    let endpoints = EntraEndpoints::build(&account.tenant)
        .map_err(GraphError::from)
        .map_err(SendError::from)?;
    let exchange = Arc::new(
        EntraTokenExchange::build(&account.client_id, &endpoints)
            .map_err(GraphError::from)
            .map_err(SendError::from)?,
    );
    let tokens = Arc::new(TokenManager::new(
        account.name.clone(),
        Arc::new(KeyringCredentialStore),
        exchange,
    ));
    let access_token = tokens
        .get_access_token(false)
        .await
        .map_err(GraphError::from)
        .map_err(SendError::from)?;
    let identity = GraphProfileVerifier
        .get_profile(&access_token)
        .await
        .map_err(SendError::from)?;
    if !identity
        .user_principal_name
        .eq_ignore_ascii_case(&account.user)
    {
        return Err(SendError::IdentityMismatch.into());
    }
    let graph = GraphTransport::build(tokens, Arc::new(TokioSleeper)).map_err(SendError::from)?;
    graph
        .send_mime_file(message.get_encoded_path())
        .await
        .map_err(SendError::from)?;
    eprintln!("nochange: message accepted for processing");
    Ok(())
}

async fn run_init(config_override: Option<&Path>, arguments: &InitArgs) -> Result<(), AppError> {
    let paths = AppPaths::discover()?;
    let config_path = config_override.unwrap_or(&paths.config_file);
    let home = BaseDirs::new().ok_or(ConfigError::HomeDirectoryUnavailable)?;
    let config = AppConfig::load_from(config_path, home.home_dir())?;
    let selection = arguments
        .account
        .as_deref()
        .map_or(AccountSelection::All, AccountSelection::Named);
    let accounts = config.get_selected_accounts(selection)?;
    let credentials = Arc::new(KeyringCredentialStore);
    let authenticator = Arc::new(EntraAccountAuthenticator::new(SystemBrowserLauncher));
    let profiles = Arc::new(GraphProfileVerifier);
    let prompter = Arc::new(ConsoleLoginPrompter);
    let runner = InitRunner::new(credentials, authenticator, profiles, prompter);
    let method = if arguments.device_code {
        LoginMethod::DeviceCode
    } else {
        LoginMethod::Browser
    };

    for account in accounts {
        runner.initialize_account(account, method).await?;
    }
    Ok(())
}

async fn run_sync(
    config_override: Option<&Path>,
    arguments: &SyncArgs,
    verbose: bool,
) -> Result<(), AppError> {
    let paths = AppPaths::discover()?;
    let config_path = config_override.unwrap_or(&paths.config_file);
    let home = BaseDirs::new().ok_or(ConfigError::HomeDirectoryUnavailable)?;
    let config = AppConfig::load_from(config_path, home.home_dir())?;
    let selection = arguments
        .account
        .as_deref()
        .map_or(AccountSelection::All, AccountSelection::Named);
    let accounts = config.get_selected_accounts(selection)?;
    let state_root = paths
        .state_database
        .parent()
        .ok_or(StateError::InvalidPath)
        .map_err(SyncError::from)?;
    let fsync_enabled = !arguments.no_fsync;
    let mut state = StateDatabase::open_with_fsync(&paths.state_database, fsync_enabled)
        .map_err(SyncError::from)?;
    let credentials = Arc::new(KeyringCredentialStore);
    let profiles = GraphProfileVerifier;
    let mut failures = Vec::new();

    for account in accounts {
        let reporter = ConsoleSyncReporter::new(&account.name, verbose);
        if arguments.no_fsync {
            reporter.show_status(
                "WARNING: fsync is disabled; interruption may corrupt or lose local sync data",
            );
        }
        reporter.show_status("acquiring account synchronization lock");
        let result = async {
            let lock_path = get_account_lock_path(state_root, &account.name);
            let _lock = AccountLock::acquire(&lock_path)?;
            reporter.show_status("refreshing credentials and verifying mailbox identity");
            let endpoints = EntraEndpoints::build(&account.tenant).map_err(GraphError::from)?;
            let exchange = Arc::new(
                EntraTokenExchange::build(&account.client_id, &endpoints)
                    .map_err(GraphError::from)?,
            );
            let tokens = Arc::new(TokenManager::new(
                account.name.clone(),
                Arc::clone(&credentials),
                exchange,
            ));
            let access_token = tokens
                .get_access_token(false)
                .await
                .map_err(GraphError::from)?;
            let identity = profiles.get_profile(&access_token).await?;
            if !identity
                .user_principal_name
                .eq_ignore_ascii_case(&account.user)
            {
                return Err(SyncError::IdentityMismatch {
                    account: account.name.clone(),
                    expected: account.user.clone(),
                    actual: identity.user_principal_name,
                });
            }
            reporter.show_status("mailbox identity verified");
            let graph =
                GraphTransport::build_with_fsync(tokens, Arc::new(TokioSleeper), fsync_enabled)?;
            let maildir = MaildirStore::new_with_fsync(&account.maildir, fsync_enabled);
            reporter.show_status(if arguments.dry_run {
                "starting synchronization dry-run"
            } else {
                "starting synchronization"
            });
            CloudSynchronizer::new_with_reporter(&graph, &reporter)
                .sync_account(account, &mut state, &maildir, arguments.dry_run)
                .await
        }
        .await;

        match result {
            Ok(summary) => show_sync_summary(&account.name, summary, arguments.dry_run),
            Err(error) => {
                reporter.show_status("synchronization failed");
                failures.push(format!("account '{}': {error}", account.name));
            }
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(AppError::Temporary(failures.join("; ")))
    }
}

struct ConsoleSyncReporter {
    account: String,
    verbose: bool,
    started: Instant,
}

impl ConsoleSyncReporter {
    fn new(account: &str, verbose: bool) -> Self {
        Self {
            account: get_safe_log_value(account),
            verbose,
            started: Instant::now(),
        }
    }

    fn show_status(&self, message: &str) {
        eprintln!(
            "nochange: [{} +{:.1}s] {message}",
            self.account,
            self.started.elapsed().as_secs_f64(),
        );
    }
}

impl SyncProgressReporter for ConsoleSyncReporter {
    fn report(&self, progress: SyncProgress) {
        if let Some(message) = get_sync_progress_message(&progress, self.verbose) {
            self.show_status(&message);
        }
    }
}

fn get_sync_progress_message(progress: &SyncProgress, verbose: bool) -> Option<String> {
    match progress {
        SyncProgress::FolderEnumerationStarted { resumed } => Some(format!(
            "enumerating mail folders ({})",
            get_round_kind(*resumed)
        )),
        SyncProgress::FolderPageStarted { page } => {
            Some(format!("requesting mail-folder delta page {page}"))
        }
        SyncProgress::FolderPageCompleted {
            page,
            changes,
            complete,
        } if verbose => Some(format!(
            "mail-folder page {page} returned {changes} changes ({})",
            get_page_state(*complete)
        )),
        SyncProgress::FolderPageCompleted { .. } => None,
        SyncProgress::FolderEnumerationCompleted {
            discovered,
            selected,
        } => Some(format!(
            "folder enumeration complete: {discovered} discovered, {selected} selected"
        )),
        SyncProgress::LocalScanStarted { folders, tracked } => Some(format!(
            "scanning {tracked} tracked messages in {folders} managed folders"
        )),
        SyncProgress::LocalScanCompleted {
            flag_updates,
            moves,
            trashed,
            deleted,
            duplicates,
            edited,
        } => Some(format!(
            "local scan complete: {flag_updates} flag updates, {moves} moves, {trashed} trash, {deleted} permanent deletes, {duplicates} duplicate, {edited} edited"
        )),
        SyncProgress::LocalLocationApplyStarted { total } => {
            Some(format!("submitting {total} local move/trash/delete operations"))
        }
        SyncProgress::LocalLocationApplyProgress {
            position,
            total,
            action,
        } if verbose || *position == 1 || *position == *total || *position % 100 == 0 => {
            let action = match action {
                LocalLocationActionKind::Move => "move",
                LocalLocationActionKind::Trash => "trash",
                LocalLocationActionKind::Delete => "delete",
            };
            Some(format!(
                "submitting local {action} operation {position}/{total}"
            ))
        }
        SyncProgress::LocalLocationApplyProgress { .. } => None,
        SyncProgress::LocalFlagApplyStarted { total } => {
            Some(format!("submitting {total} local flag updates"))
        }
        SyncProgress::LocalFlagApplyProgress { position, total }
            if verbose || *position == 1 || *position == *total || *position % 100 == 0 =>
        {
            Some(format!("submitting local flag update {position}/{total}"))
        }
        SyncProgress::LocalFlagApplyProgress { .. } => None,
        SyncProgress::MessageFolderStarted {
            folder,
            position,
            total,
            resumed,
            estimated_total,
        } => {
            let round = match estimated_total {
                Some(estimated_total) => {
                    format!("initial, approximately {estimated_total} items")
                }
                None => get_round_kind(*resumed).to_owned(),
            };
            Some(format!(
                "enumerating folder {position}/{total} '{}' ({round})",
                get_safe_log_value(folder),
            ))
        }
        SyncProgress::MessagePageStarted { folder, page } => Some(format!(
            "folder '{}': requesting message delta page {page}",
            get_safe_log_value(folder),
        )),
        SyncProgress::MessagePageCompleted {
            folder,
            page,
            changes,
            accumulated,
            complete,
            estimated_total,
        } if verbose => {
            let estimate = estimated_total
                .and_then(|total| get_estimated_percentage(*accumulated, total))
                .map_or_else(String::new, |percentage| {
                    format!(", approximately {percentage:.1}%")
                });
            Some(format!(
                "folder '{}': page {page} returned {changes} changes, {accumulated} accumulated{estimate} ({})",
                get_safe_log_value(folder),
                get_page_state(*complete),
            ))
        }
        SyncProgress::MessagePageCompleted {
            folder,
            accumulated,
            complete: false,
            estimated_total: Some(estimated_total),
            ..
        } => get_estimated_percentage(*accumulated, *estimated_total).map(|percentage| {
            format!(
                "folder '{}': approximately {percentage:.1}% enumerated ({accumulated}/~{estimated_total} items)",
                get_safe_log_value(folder),
            )
        }),
        SyncProgress::MessagePageCompleted { .. } => None,
        SyncProgress::MessageFolderCompleted { folder, changes } => Some(format!(
            "folder '{}' enumeration complete: {changes} changes",
            get_safe_log_value(folder),
        )),
        SyncProgress::MessageApplyStarted { total } => {
            Some(format!("applying {total} collapsed cloud message actions"))
        }
        SyncProgress::MessageApplyProgress {
            position,
            total,
            action,
        } if verbose || *position == 1 || *position == *total || *position % 100 == 0 => {
            let action = match action {
                SyncActionKind::Upsert => "upsert",
                SyncActionKind::Delete => "delete",
            };
            Some(format!(
                "applying message action {position}/{total} ({action})"
            ))
        }
        SyncProgress::MessageApplyProgress { .. } => None,
    }
}

fn get_round_kind(resumed: bool) -> &'static str {
    if resumed { "incremental" } else { "initial" }
}

fn get_page_state(complete: bool) -> &'static str {
    if complete {
        "round complete"
    } else {
        "more pages"
    }
}

fn get_estimated_percentage(accumulated: usize, estimated_total: u32) -> Option<f64> {
    if estimated_total == 0 {
        None
    } else {
        Some((accumulated as f64 / estimated_total as f64) * 100.0)
    }
}

fn get_safe_log_value(value: &str) -> String {
    const MAX_CHARACTERS: usize = 120;
    let mut safe = String::new();
    let mut characters = value.chars();
    for character in characters.by_ref().take(MAX_CHARACTERS) {
        safe.extend(character.escape_default());
    }
    if characters.next().is_some() {
        safe.push('…');
    }
    safe
}

fn show_sync_summary(account: &str, summary: SyncSummary, dry_run: bool) {
    let qualifier = if dry_run {
        "would synchronize"
    } else {
        "synchronized"
    };
    println!(
        "Account '{account}' {qualifier} {} folders: {} created, {} updated, {} deleted, {} conflicts, {} local moves, {} local trash, {} local permanent deletes, {} local flag updates, {} local changes deferred.",
        summary.folders,
        summary.created,
        summary.updated,
        summary.deleted,
        summary.conflicted,
        summary.local_moves,
        summary.local_trashed,
        summary.local_deleted,
        summary.local_flag_updates,
        summary.local_ignored,
    );
}

#[cfg(test)]
mod tests {
    use super::{get_safe_log_value, get_sync_progress_message};
    use crate::sync::{LocalLocationActionKind, SyncProgress};

    #[test]
    fn renders_normal_and_verbose_progress_at_the_expected_detail_levels() {
        assert_eq!(
            get_sync_progress_message(
                &SyncProgress::MessageFolderStarted {
                    folder: "Inbox".into(),
                    position: 3,
                    total: 12,
                    resumed: false,
                    estimated_total: Some(500),
                },
                false,
            )
            .as_deref(),
            Some("enumerating folder 3/12 'Inbox' (initial, approximately 500 items)")
        );
        assert_eq!(
            get_sync_progress_message(
                &SyncProgress::MessagePageStarted {
                    folder: "Inbox".into(),
                    page: 2,
                },
                false,
            )
            .as_deref(),
            Some("folder 'Inbox': requesting message delta page 2")
        );
        let completed = SyncProgress::MessagePageCompleted {
            folder: "Inbox".into(),
            page: 2,
            changes: 25,
            accumulated: 125,
            complete: false,
            estimated_total: Some(500),
        };
        assert_eq!(
            get_sync_progress_message(&completed, false).as_deref(),
            Some("folder 'Inbox': approximately 25.0% enumerated (125/~500 items)")
        );
        assert_eq!(
            get_sync_progress_message(&completed, true).as_deref(),
            Some(
                "folder 'Inbox': page 2 returned 25 changes, 125 accumulated, approximately 25.0% (more pages)"
            )
        );
        assert_eq!(
            get_sync_progress_message(
                &SyncProgress::LocalScanCompleted {
                    flag_updates: 4,
                    moves: 2,
                    trashed: 1,
                    deleted: 1,
                    duplicates: 0,
                    edited: 3,
                },
                false,
            )
            .as_deref(),
            Some(
                "local scan complete: 4 flag updates, 2 moves, 1 trash, 1 permanent deletes, 0 duplicate, 3 edited"
            )
        );
        assert_eq!(
            get_sync_progress_message(
                &SyncProgress::LocalLocationApplyProgress {
                    position: 1,
                    total: 3,
                    action: LocalLocationActionKind::Trash,
                },
                false,
            )
            .as_deref(),
            Some("submitting local trash operation 1/3")
        );
        assert!(
            get_sync_progress_message(
                &SyncProgress::LocalFlagApplyProgress {
                    position: 2,
                    total: 4,
                },
                false,
            )
            .is_none()
        );
        assert_eq!(
            get_sync_progress_message(
                &SyncProgress::LocalFlagApplyProgress {
                    position: 2,
                    total: 4,
                },
                true,
            )
            .as_deref(),
            Some("submitting local flag update 2/4")
        );
    }

    #[test]
    fn escapes_control_characters_and_truncates_status_values() {
        let unsafe_value = format!("Inbox\nforged-log\r{}", "x".repeat(150));
        let safe = get_safe_log_value(&unsafe_value);

        assert!(safe.contains("\\n"));
        assert!(safe.contains("\\r"));
        assert!(!safe.contains('\n'));
        assert!(safe.ends_with('…'));
    }
}
