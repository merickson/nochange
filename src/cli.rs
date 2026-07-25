//! Command-line interface definitions.

use clap::{ArgAction, Args, Parser, Subcommand};
use std::path::PathBuf;

/// Top-level command-line arguments for Nochange.
#[derive(Debug, Parser, PartialEq, Eq)]
#[command(name = "nochange", version, about)]
pub struct Cli {
    /// Read configuration from this file instead of the default XDG path.
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Enable verbose diagnostic output.
    #[arg(long, global = true)]
    pub verbose: bool,

    /// Operation to perform.
    #[command(subcommand)]
    pub command: Command,
}

/// Operations supported by the Nochange executable.
#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum Command {
    /// Authenticate accounts and verify their Microsoft 365 identities.
    Init(InitArgs),
    /// Synchronize configured accounts.
    Sync(SyncArgs),
    /// Send one RFC message read from standard input.
    Send(SendArgs),
}

/// Arguments for account initialization.
#[derive(Debug, Args, PartialEq, Eq)]
pub struct InitArgs {
    /// Authenticate only this configured account.
    #[arg(long, value_name = "NAME")]
    pub account: Option<String>,

    /// Authenticate with the device-code flow instead of a browser callback.
    #[arg(long)]
    pub device_code: bool,
}

/// Arguments for mailbox synchronization.
#[derive(Debug, Args, PartialEq, Eq)]
pub struct SyncArgs {
    /// Synchronize only this configured account.
    #[arg(long, value_name = "NAME")]
    pub account: Option<String>,

    /// Discover and print actions without changing Graph, Maildir, or state.
    #[arg(long)]
    pub dry_run: bool,

    /// Disable fsync for this run, risking corruption or data loss on interruption.
    #[arg(long)]
    pub no_fsync: bool,
}

/// Arguments for the sendmail-compatible sending interface.
#[derive(Debug, Args, PartialEq, Eq)]
#[command(args_override_self = true)]
pub struct SendArgs {
    /// Send through this configured account.
    #[arg(short = 'a', value_name = "ACCOUNT")]
    pub account: Option<String>,

    /// Set and validate the envelope sender.
    #[arg(short = 'f', value_name = "ADDRESS")]
    pub from: Option<String>,

    /// Include recipients found in the message headers.
    #[arg(short = 't')]
    pub read_recipients_from_headers: bool,

    /// Accepted sendmail compatibility option; dot handling is always disabled.
    #[arg(short = 'i', action = ArgAction::SetTrue)]
    pub ignore_dot: bool,

    /// Hidden prefix that allows Clap to recognize the grouped `-oi` spelling.
    #[arg(short = 'o', action = ArgAction::SetTrue, requires = "ignore_dot", hide = true)]
    ignore_dot_prefix: bool,

    /// Envelope recipients.
    #[arg(value_name = "RECIPIENT", trailing_var_arg = true)]
    pub recipients: Vec<String>,
}
