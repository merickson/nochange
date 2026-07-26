# Nochange

Nochange is a Rust CLI for synchronizing Microsoft 365 mailboxes to local
Maildirs and sending mail through a sendmail-compatible interface. It uses the
Microsoft Graph v1.0 REST API and delegated access to the signed-in user's
primary mailbox.

The Graph-based rewrite is under active development. `nochange init`
authenticates and verifies the configured Microsoft 365 identity, and
`nochange sync` now performs incremental cloud-to-local synchronization.
Local read/follow-up flags, managed-folder moves, trash, and permanent deletion
from Deleted Items synchronize back to Graph. Sending is not operational yet.

## Build

Nochange uses the Rust 2024 edition. Build and test it with Cargo:

```console
cargo build
cargo test --workspace --all-features
```

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

## Configuration

The default configuration path is
`$XDG_CONFIG_HOME/nochange/nochange.conf`, falling back to
`~/.config/nochange/nochange.conf`. Synchronization state is stored at
`$XDG_STATE_HOME/nochange/state.sqlite3`, falling back to
`~/.local/state/nochange/state.sqlite3`.

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

`accounts`, `maildir`, `user`, and `clientid` are required. `tenant` defaults
to `organizations`, and `folderseparator` defaults to `.`. Folder filters are
case-insensitive, use `/`-delimited full remote paths, and select or exclude
the named folder's complete subtree. `folderinclude` and `folderexclude` cannot
be used together.

Nochange rejects client secrets, unknown settings, repeated accounts,
overlapping account Maildir roots, and unsafe folder separators.

## Log in

Replace `user`, `clientid`, and `tenant` in `nochange.conf`. `user` must be the
mailbox's Microsoft Entra user principal name, and `tenant` may be the recorded
Directory (tenant) ID.

Build and start browser login:

```console
cargo build
target/debug/nochange --config ./nochange.conf init --account o365_1
```

Nochange prints the authorization URL before opening the system browser. After
the localhost callback, it calls Microsoft Graph `/me`, confirms that the
returned user principal name matches `user`, and only then stores the refresh
token in the operating system credential store.

For a terminal without a usable local browser, use device authorization:

```console
target/debug/nochange --config ./nochange.conf init --account o365_1 --device-code
```

Visit the displayed Microsoft URL and enter the displayed code. A successful
trial ends with:

```text
Account 'o365_1' authenticated as myuser@contoso.com.
```

## Synchronize mail

Preview the selected folders and message changes without downloading MIME or
changing Maildirs and checkpoints:

```console
target/debug/nochange --config ./nochange.conf sync --account o365_1 --dry-run
```

Then perform the cloud-to-local synchronization:

```console
target/debug/nochange --config ./nochange.conf sync --account o365_1
```

For a faster initial import, explicitly disable filesystem durability barriers
for only that invocation:

```console
target/debug/nochange --config ./nochange.conf sync --account o365_1 --no-fsync
```

This sets SQLite synchronous mode to `OFF` and skips MIME-file and Maildir
directory `fsync` calls. A crash, power loss, forced restart, or storage failure
can therefore lose or corrupt local Maildir and synchronization-state data.
Nochange prints a warning when this mode is active. The next invocation returns
to full durability unless `--no-fsync` is supplied again.

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

Omit `--account` to process all configured accounts serially. The first run
creates private Maildirs under the configured account root and downloads each
selected folder's complete history. MIME transfer uses up to four concurrent
downloads, followed by deterministic local commits. Later runs resume from
opaque Microsoft Graph delta links. A failed or interrupted round leaves its
message checkpoint unchanged, so it can be replayed without re-downloading
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

## CLI

```text
nochange [--config PATH] [--verbose] <COMMAND>

nochange init [--account NAME] [--device-code]
nochange sync [--account NAME] [--dry-run] [--no-fsync]
nochange send [-a ACCOUNT] [-f ADDRESS] [-t] [-i|-oi] [--] [RECIPIENT...]
```

`init` authenticates and verifies the mailbox identity. `sync --dry-run`
discovers and summarizes cloud-to-local actions without downloading MIME or
mutating Maildirs and synchronization checkpoints.

`send` reads one complete RFC message from standard input. For example:

```console
printf 'From: myuser@contoso.com\nTo: recipient@contoso.com\nSubject: Test\n\nHello.\n' \
  | nochange send -a o365_1 -t
```

The Graph API returning `202 Accepted` means Microsoft accepted the message for
processing; it does not prove final delivery.

## Safety model

Refresh tokens are stored through the operating system credential-store
adapter and replaced when Microsoft rotates them. Access tokens are cached only
in memory for their reported lifetime. Authorization sessions use PKCE and
verified callback state; device authorization is also supported by the auth
module. Tokens, authorization codes, and message content must not appear in
logs.

Graph requests use explicit timeouts, reject redirects and non-v1.0 Graph
links, request immutable Outlook IDs, retry bounded transient failures, and
remove incomplete MIME downloads. Folder and message delta requests ask Graph
for up to 1,000 changes per page and repeat that preference on continuation
requests; Graph may return fewer. Synchronization uses deterministic Maildir
keys, bounded four-at-a-time MIME transfers, serialized atomic Maildir delivery,
SQLite-backed delta checkpoints, and cloud-wins conflict handling that
preserves divergent local content. Local flag, move, trash, and delete
mutations use a durable SQLite journal and are cleared only after Graph accepts
them and their matching delta change is observed.

By default, SQLite, completed MIME files, and affected Maildir directories are
synchronized durably before progress is committed. `sync --no-fsync` explicitly
disables those guarantees for that invocation and should be treated as a
recoverable-import optimization, not the normal operating mode.

See [PLAN.md](PLAN.md) for the architecture, synchronization semantics,
acceptance tests, and deferred work.
