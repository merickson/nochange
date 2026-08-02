# Nochange

Nochange is a Rust CLI for synchronizing Microsoft 365 mailboxes to local
Maildirs and sending mail through a sendmail-compatible interface. It uses the
Microsoft Graph v1.0 REST API and delegated access to the signed-in user's
primary mailbox.

This project is inspired by [lieer](https://lieer.gaute.vetsj.com/)
for GMail, and is designed to provide capability similar to the
pairing of OfflineIMAP and msmtp. Unlike lieer, we don't integrate
into a MUA, but simply sync to a maildir.

## Using nochange

### Prerequisites

* macOS or Linux. Not tested on Windows (yet). 
* Rust 2024 or later.

### Install

Just use Cargo. From the root of the git checkout:

```console
cargo install --path=.
```

### Configuration

The default configuration path is
`$XDG_CONFIG_HOME/nochange/nochange.conf`, defaulting  to
`~/.config/nochange/nochange.conf`.

```ini
[global]
accounts = o365_1

[o365_1]
maildir = ~/maildir/o365_1
user = myuser@contoso.com
clientid = application-client-id
tenant = organizations
folderseparator = .
folderexclude = Journal,Notes,Calendar
```

* `accounts`: A comma-separated list of accounts.
* `maildir`: Per-account maildir
* `user`: Your Microsoft 365 user, of the form `<user>@<domain>`.
* `clientid`: The nochange client ID. It supports using a separate one per account so you can configure it for your Entra if necessary (see below).
* `tenant`: Leave blank if using the default Client ID (below), otherwise configure for your Entra tenant.
* `folderseparator`: defaults to `.`, matching OfflineIMAP behavior.
* `folderinclude` | `folderexclude`: Comma-separated list of folders. Either an allow list or deny list, they are mutually exclusive.

Nochange rejects client secrets, unknown settings, repeated accounts,
overlapping account Maildir roots, and unsafe folder separators.

#### ClientID

You can use the default Client ID of
`13924670-7ce9-480d-90c3-bfbaed21a227`, which is registered by Soul
Robotic (my personal domain) to 'No Change'. If you use this, do **not** specify a `tenant`.

Otherwise, please see **Microsoft Entra setup** (below).

### Log in

Ensure that you have your configuration setup.

Login with `nochange init --account <account>` for each account you have configured.

You may need to make sure your browser is already logged into the
right account if you are trying to authenticate to multiple accounts
with nochange. Simplest way forward is making sure you're logged into
the Outlook webapp with the right account when you run `nochange
init`.

Nochange prints the authorization URL before opening the system
browser. After the localhost callback then stores the refresh token in
the operating system credential store. **On macOS** you will be
prompted to allow nochange access to the keychain for the first time
that build tries to read from it. Select "Always Allowed" to not be
bothered by it in the future.

For a terminal without a usable local browser, use device authorization:

```console
nochange init --account o365_1 --device-code
```

Visit the displayed Microsoft URL and enter the displayed code. A successful
trial ends with:

```text
Account 'o365_1' authenticated as myuser@contoso.com.
```

### Synchronize mail

Preview the selected folders and message changes without downloading MIME or
changing Maildirs and checkpoints:

```console
nochange sync --dry-run
```

Then perform the cloud-to-local synchronization:

```console
nochange sync
```

You can specify `--account <account_name>` to act only on one account.

### Sending Mail

`nochange send` provides a `sendmail`-style interface to send messages
through the command line:

```console
printf 'From: myuser@contoso.com\nTo: recipient@contoso.com\nSubject: Test\n\nHello.\n' \
  | nochange send -a o365_1 -t
```

Several `sendmail` options are accepted purely for sendmail
compatibility and are not actually used.

## CLI

```text
nochange [--config PATH] [--verbose] <COMMAND>

nochange init [--account NAME] [--device-code]
nochange sync [--account NAME] [--dry-run] [--no-fsync]
nochange send [-a ACCOUNT] [-f ADDRESS] [-t] [-o] [-i] [--] [RECIPIENT...]
```

### Init

`init` authenticates and verifies the mailbox identity. See "Log In" above.

### Sync

`sync` discovers and summarizes cloud-to-local actions. See
"Synchronize Mail" above.

Synchronization writes timestamped status lines to standard error while it
authenticates, enumerates folder and message delta pages, and applies local
actions. Normal output shows every requested page and each folder's completion.
During an initial sync, it also uses Graph's current folder item count to show
an explicitly approximate per-folder percentage; the final delta link remains
the authoritative completion signal. Incremental rounds omit this estimate
because total folder size does not predict the number of changes.

Add the global `--verbose` option for returned page counts and every message
action:

```console
target/debug/nochange --verbose sync --account o365_1 --dry-run
```

Status output contains account and folder names, counts, and action kinds. It
does not contain message IDs, subjects, message bodies, delta links, or tokens.

#### Disable Fsync

For a faster initial import, explicitly disable filesystem durability barriers
for only that invocation:

```console
nochange sync --no-fsync
```

This sets SQLite synchronous mode to `OFF` and skips MIME-file and Maildir
directory `fsync` calls. A crash, power loss, forced restart, or storage failure
can therefore lose or corrupt local Maildir and synchronization-state data.
Nochange prints a warning when this mode is active. The next invocation returns
to full durability unless `--no-fsync` is supplied again.

### Send

`send` reads one complete RFC message from standard input.

The Graph API returning `202 Accepted` means Microsoft accepted the message for
processing; it does not prove final delivery. Successful sends produce no
terminal output; validation or submission failures are written to standard
error and use the documented sendmail-compatible exit codes.

Without `-a`, `send` selects the unique configured account whose `user`
matches the message's `From` and optional `Sender` address case-insensitively.
If no account matches, sending is rejected; if multiple accounts use that same
address, `-a` is required. `From` and optional `Sender` must name the selected
account's configured `user` address. Aliases and delegated send-as are not
supported yet.

With `-t`, recipients are taken from the union of `To`, `Cc`, `Bcc`, and the
command line. Without `-t`, at least one command-line recipient is required and
every header recipient must also appear on the command line. Command-line
recipients absent from the headers are added as `Bcc` before submission. The
message must include a valid header/body separator. `-o`, `-i`, grouped `-oi`,
and `-f ADDRESS` are accepted compatibility no-ops. The `-f` address does not
affect account or sender selection, and a line containing only `.` is never
treated specially.

Message validation retains only the bounded header block in memory. The input
and its base64 Graph payload are streamed through secure temporary files, and
unrelated MIME headers, attachments, transfer encodings, and body bytes are
not reserialized. An explicit throttling or transient Graph rejection is
retried. A network failure after submission begins reports `EX_TEMPFAIL` with
an unknown-result warning and is not automatically replayed, because Graph may
already have accepted the message and a replay could create a duplicate.

## Microsoft Entra setup

Create a user-owned Microsoft Entra app registration configured as a public
client:

1. In **Microsoft Entra admin center → App registrations**, create an
   application for the organization containing the mailbox. Record its
   **Application (client) ID** and **Directory (tenant) ID**.
2. Under **API permissions**, add delegated Microsoft Graph permissions
   `User.Read`, `Mail.ReadWrite`, and `Mail.Send`. Grant consent if required by
   the organization's policy.
3. Under **Authentication → Add a platform**, select **Mobile and desktop
   applications** and add the exact system-browser redirect URI
   `http://localhost`.
4. Under **Authentication → Advanced settings**, set **Allow public client
   flows** to **Yes** if device-code login will be used.
5. Do not create or configure a client secret. An installed application cannot
   keep one securely.

Microsoft documents `http://localhost` for desktop apps using a system browser
and the **Allow public client flows** setting for device authorization in its
[desktop app configuration
guide](https://learn.microsoft.com/en-us/entra/identity-platform/scenario-desktop-app-configuration).

Nochange will request `offline_access`, the two mail permissions, and the
minimum identity scopes needed to verify the signed-in account. The initial
release targets commercial Microsoft 365 only; shared mailboxes, delegated
mailboxes, aliases, and sovereign clouds are outside its initial scope.

## Sync Process

The first run creates private Maildirs under the configured account
root and downloads each selected folder's complete history. MIME
transfer uses up to four concurrent downloads, followed by
deterministic local commits. Later runs resume from opaque Microsoft
Graph delta links. A failed or interrupted round leaves its message
checkpoint unchanged, so it can be replayed without re-downloading
already committed versions.

Maildir folder names preserve ordinary spaces, Unicode, and readable
punctuation. Only hierarchy separators, ambiguous percent signs, control
characters, and filesystem-unsafe characters are escaped.

This phase synchronizes cloud-created, changed, moved, and deleted messages to
Maildir, including the `S` (read) and `F` (flagged) flags. Changes to `S` and
`F` on clean tracked messages are journaled before being submitted to Microsoft
Graph. Interrupted submissions replay idempotently, and the matching Graph
delta echo completes the journal without downloading MIME again.

Moving a clean tracked file between selected managed Maildirs moves the Graph
message without re-downloading its MIME. Adding Maildir `T`, or removing a
tracked file outside Deleted Items, moves the message to the mailbox's
well-known Deleted Items folder. Adding `T` or removing a tracked file already
in Deleted Items permanently deletes it through Graph; Microsoft 365 retention
policies still determine final server-side retention.

Move, trash, delete, and flag requests are journaled before Graph is contacted,
replay after interruptions, and complete when their matching delta changes are
observed. `sync --dry-run` reports all of these planned operations without
mutating Graph, Maildir, state, checkpoints, or the journal. Duplicate tracked
keys, locally edited MIME, moves that rewrite Nochange's deterministic key, and
untracked local messages remain deferred and are not uploaded. If cloud state
replaces or deletes locally edited tracked MIME, Nochange preserves the local
bytes in `.nochange-conflicts` before applying the cloud result.

## Safety model

Refresh tokens are stored through the operating system credential-store
adapter and replaced when Microsoft rotates them. Access tokens are cached only
in memory for their reported lifetime. Authorization sessions use PKCE and
verified callback state; device authorization is also supported by the auth
module. Tokens, authorization codes, and message content must not appear in
logs.

Graph requests use explicit connect and two-minute request timeouts, reject
redirects and non-v1.0 Graph links, and request immutable Outlook IDs. Bounded
exponential retries cover throttling, transient HTTP responses, connection and
request timeouts, token-refresh transport failures, truncated JSON responses,
and interrupted MIME streams. Partial MIME files are removed before a transfer
is retried. Permanent Graph responses, malformed successful JSON, and local
filesystem failures are not retried.

Folder and message delta requests ask Graph for up to 1,000 changes per page
and repeat that preference on continuation requests; Graph may return fewer.
Synchronization uses deterministic Maildir keys, bounded four-at-a-time MIME
transfers, serialized atomic Maildir delivery, SQLite-backed delta checkpoints,
and cloud-wins conflict handling that preserves divergent local content. Local
flag, move, trash, and delete mutations use a durable SQLite journal and are
cleared only after Graph accepts them and their matching delta change is
observed.

By default, SQLite, completed MIME files, and affected Maildir directories are
synchronized durably before progress is committed. `sync --no-fsync` explicitly
disables those guarantees for that invocation and should be treated as a
recoverable-import optimization, not the normal operating mode.
