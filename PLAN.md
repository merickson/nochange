# Nochange Rust Architecture and Implementation Plan

## Summary

Re-create Nochange as a Rust CLI that synchronizes Microsoft 365 mailboxes to local Maildirs and sends mail through a sendmail-compatible interface.

Use Microsoft Graph rather than EWS. Exchange Online starts disabling EWS in October 2026 and completes its shutdown in April 2027. Rust is not among Microsoft's supported Graph SDK languages, so call the stable Graph v1.0 REST API directly instead of depending on an unofficial Graph SDK.

The finished application must:

- Authenticate interactively with a user-provided Microsoft Entra public-client application.
- Persist credentials securely and refresh access tokens without repeated logins.
- Download complete MIME messages into an OfflineIMAP-style Maildir hierarchy.
- Incrementally synchronize folders and messages using Graph delta links.
- Propagate supported local flags, trash operations, and moves back to Microsoft 365.
- Send RFC messages from stdin with a documented sendmail-compatible CLI subset.
- Recover safely from interruption without duplicate messages or advanced checkpoints.
- Preserve local divergent content whenever cloud state wins a conflict.

## Technology and Crates

Create a Cargo binary package with Rust's current stable edition and commit `Cargo.lock` for reproducible CLI builds.

Use these crate families, selecting current compatible releases when implementation begins:

- `clap` with derive support for command-line parsing.
- `tokio` for the async runtime, filesystem operations, timers, and synchronization.
- `reqwest` with `json`, `stream`, and `rustls-tls` for Microsoft Graph REST calls and streaming MIME transfers.
- `oauth2` for authorization-code PKCE, refresh-token, and device-authorization flows.
- `open` to launch the system browser and `tiny_http` for the temporary localhost OAuth callback.
- `keyring` for platform credential storage; store refresh tokens only, never access tokens.
- `rusqlite` with bundled SQLite for schema-versioned synchronization state.
- `serde` and `serde_json` for Graph payloads, journal records, and typed serialization.
- `configparser` for the existing INI-style `nochange.conf` contract.
- `directories` for XDG/platform configuration and state paths.
- `fs4` for per-account interprocess file locks.
- `mail-parser` for RFC message and recipient parsing without implementing MIME parsing locally.
- `percent-encoding`, `sha2`, and `hex` for minimally escaped reversible folder
  paths and deterministic Maildir keys.
- `thiserror` for dedicated error types and `tracing` plus `tracing-subscriber` for diagnostics.
- `backon` or an equivalently focused retry crate for bounded retries that honor Graph backoff headers.
- `secrecy` and `zeroize` for values containing tokens while they are in memory.

Do not use an unofficial Graph SDK. Keep Graph wire types inside the Graph adapter so API payload changes do not leak into synchronization domain types.

## Architecture

Organize the crate into modules with one-way dependencies toward the domain layer:

- `cli`: Clap definitions, account selection, exit-code mapping, and human-readable output.
- `config`: INI loading, validation, path expansion, folder filters, and XDG path resolution.
- `auth`: Entra endpoints, PKCE/device flows, keyring access, refresh-token rotation, and access-token acquisition.
- `graph`: Authenticated HTTP transport, Graph DTOs, delta pagination, MIME streaming, mutation methods, retries, and Graph error translation.
- `state`: SQLite schema, migrations, transactions, staged changes, pending operations, and typed repositories.
- `maildir`: Folder mapping, atomic delivery, flag parsing and renames, scans, move detection, and conflict preservation.
- `sync`: Local/remote change collection, reconciliation, conflict rules, action ordering, and checkpoint commits.
- `send`: RFC message validation, safe recipient calculation, MIME preparation, and Graph `sendMail` calls.
- `model`: Domain records and enums that contain no HTTP, SQLite, or filesystem implementation details.
- `error`: Application errors and stable process exit classifications.

Define mockable traits at subsystem boundaries:

- `GraphApi`: profile verification, folder/message delta traversal, MIME download, flag update, message move, delete, and MIME send.
- `StateStore`: account/folder/message records, staging, journaling, migrations, and transactions.
- `MailStore`: managed-folder scans, atomic writes, renames, removals, hashes, and conflict copies.
- `CredentialStore`: load, replace, and delete refresh tokens.
- `Clock` and `Sleeper`: injectable time and retry delays for deterministic tests.

Use concrete adapters in production and test doubles in unit tests. Do not expose `reqwest`, `rusqlite`, keyring, or MIME-parser types through domain interfaces.

## Public CLI and Configuration

Provide a `nochange` binary with these interfaces:

```text
nochange [--config PATH] [--verbose] <COMMAND>

nochange init [--account NAME] [--device-code]
nochange sync [--account NAME] [--dry-run]
nochange send [-a ACCOUNT] [-f ADDRESS] [-t] [-i|-oi] [--] [RECIPIENT...]
```

Behavior:

- `init` validates configuration, obtains consent, verifies `/me`, initializes state, and creates selected Maildirs. It does not upload or delete messages.
- `sync` processes all configured accounts serially unless one account is selected. Continue to later accounts after an account failure and return nonzero if any account failed.
- `sync --dry-run` performs discovery and reconciliation planning but makes no Graph, Maildir, checkpoint, or journal mutations.
- `send` reads one RFC message from stdin. `-i` and `-oi` are accepted no-ops for common sendmail callers.
- Infer the send account only when exactly one account is configured; otherwise require `-a`.
- Map failures to sendmail-style `EX_USAGE`, `EX_DATAERR`, `EX_UNAVAILABLE`, `EX_SOFTWARE`, `EX_TEMPFAIL`, and `EX_CONFIG` exit codes.

Use `$XDG_CONFIG_HOME/nochange/nochange.conf`, normally `~/.config/nochange/nochange.conf`:

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

Configuration rules:

- Require `accounts`, `maildir`, `user`, and `clientid`.
- Default `tenant` to `organizations` and `folderseparator` to `.`.
- Reject `clientsecret`; installed public clients cannot keep secrets securely.
- Make `folderinclude` and `folderexclude` mutually exclusive.
- Interpret folder filters as case-folded, `/`-delimited full remote paths and apply matches to complete subtrees.
- Expand `~` and environment-independent platform directories, then store canonical absolute paths.
- Reject duplicate accounts, duplicate Maildir roots, unsafe separators, unknown keys, and path collisions.

Document creation of a user-owned Entra public-client registration with delegated `Mail.ReadWrite` and `Mail.Send`, `http://localhost` as a desktop redirect, and public-client/device-code flows enabled. Request `offline_access`, the two Graph scopes, and only identity scopes required to validate the signed-in account.

## Authentication and Graph Transport

Build Entra v2 endpoints from the configured tenant:

```text
https://login.microsoftonline.com/{tenant}/oauth2/v2.0/authorize
https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token
https://login.microsoftonline.com/{tenant}/oauth2/v2.0/devicecode
```

Authentication flow:

1. Load the account's refresh token from the OS credential store.
2. Exchange it for an access token before making Graph calls.
3. Atomically replace the stored refresh token whenever Entra rotates it.
4. If no usable refresh token exists during `init`, use authorization-code PKCE with a random localhost port and verified CSRF state.
5. Use device authorization when `--device-code` is supplied.
6. On a Graph `401`, refresh once and replay only requests whose bodies can be reproduced safely.
7. Never log authorization codes, device codes, tokens, raw OAuth responses, or `Authorization` headers.

If a supported native credential store is unavailable, fail with remediation instructions by default. Permit a plaintext token file only through an explicit insecure configuration opt-in; create it atomically with mode `0600` and warn on every use.

The Graph transport must:

- Target only `https://graph.microsoft.com/v1.0` and reject unexpected hosts in server-provided pagination links.
- Add `Prefer: IdType="ImmutableId"` to every supported Outlook request.
- Retain and request opaque `@odata.nextLink` and `@odata.deltaLink` URLs exactly as returned.
- Stream message `$value` responses directly to temporary files instead of buffering whole mailboxes in memory.
- Decode Graph JSON into private DTOs and convert them into domain records after validation.
- Retry `429`, `502`, `503`, and `504`, honoring `Retry-After` and limiting total delay to five minutes.
- Treat other `4xx` responses as permanent, except one refresh attempt for `401`.
- Use explicit connect, request, and MIME-transfer timeouts and include Graph request IDs in safe diagnostics.

## State and Maildir Design

Store synchronization state at `$XDG_STATE_HOME/nochange/state.sqlite3`. Enable foreign keys and WAL mode, create the database with mode `0600`, and use explicit transactions.

Schema version 1 contains:

- `metadata`: schema version and application migration metadata.
- `accounts`: account name, user identity, immutable configuration fingerprint, folder delta link, and initialization status.
- `folders`: account, remote folder ID, parent ID, remote path, encoded local path, selection/deletion state, and message delta link.
- `messages`: account, immutable Graph message ID, current folder ID, deterministic Maildir key, relative path, SHA-256 MIME hash, Internet Message-ID, read/flag state, and remote modification metadata.
- `staged_changes`: sync-run ID, folder, message ID, change kind, validated payload, and candidate checkpoint URL.
- `pending_operations`: operation ID, account, immutable message ID, operation kind, target/desired state, and lifecycle status.

Reject databases with a newer unknown schema. Apply every future migration transactionally and test both upgrade and rollback paths.

Maildir layout:

- Create one Maildir per selected remote folder under the account root.
- Flatten remote hierarchy with the configured separator.
- Percent-encode `%`, `/`, the configured separator, control characters, and platform-invalid path characters per folder component.
- Derive a stable Maildir basename from the account identity and immutable Graph message ID using SHA-256; never expose the raw remote ID in a filename.
- Write message bytes to `tmp` with mode `0600`, call `sync_all`, and atomically rename to `new` or `cur`.
- Deliver plain unread messages to `new`; place read or otherwise flagged messages in `cur` with sorted `:2,` flags.
- Preserve unsupported `D`, `P`, and `R` Maildir flags whenever Nochange renames a file.
- Never overwrite an existing message or conflict file.

## Synchronization Algorithm

For each account:

1. Acquire an exclusive account sync lock.
2. Validate the account identity and immutable configuration fingerprint.
3. Scan every managed Maildir and classify tracked files, missing files, flags, moves, content edits, untracked files, duplicates, and ambiguities.
4. Traverse folder delta from its saved link or initial endpoint and stage the full round.
5. Traverse message delta for every selected folder and stage the full round without advancing durable checkpoints.
6. Collapse repeated remote events to the latest validated state per immutable message ID.
7. Correlate cross-folder delete/create pairs as remote moves.
8. Compare staged remote changes and local changes against the last synchronized baseline.
9. Produce an ordered `SyncAction` plan and print it without mutation for `--dry-run`.
10. Persist pending remote operations before issuing Graph mutations.
11. Apply local-origin remote moves/trash operations first, then flag updates against the message's resulting location.
12. Apply cloud-origin Maildir writes, moves, flag renames, removals, and conflict copies atomically.
13. In one SQLite transaction, update message/folder baselines, commit completed delta links, complete journals, and clear staged changes.
14. Release the lock and print per-account created, updated, moved, trashed, conflicted, ignored, and failed counts.

Synchronization semantics:

- The first sync downloads complete selected-folder history and never interprets pre-existing local files as remote deletions.
- Deterministic keys and staged changes make interrupted initial and incremental runs idempotent.
- Map Maildir `S` bidirectionally to Graph `isRead`.
- Map Maildir `F` bidirectionally to Graph follow-up flagged/not-flagged; represent Graph completed flags locally as `F` and clear completion when the user removes `F`.
- Interpret Maildir `T` or disappearance of a tracked file as a move to Deleted Items.
- If a locally removed message is already in Deleted Items, call Graph delete, leaving final retention behavior to Microsoft 365.
- Preserve but do not push `D`, `P`, and `R`.
- Propagate local moves only between already-managed folders.
- Correlate a rewritten local move by deterministic key first, then a unique Internet Message-ID, then a unique MIME hash. Never delete remotely when correlation is ambiguous.
- Leave untracked local messages untouched and report them; do not upload them.
- Do not create, rename, or delete remote folders from local filesystem changes.
- Treat local content edits as unsupported: copy the edited version to a conflict Maildir and restore the cloud MIME.
- Cloud state wins incompatible conflicts. Preserve every divergent local content version before applying cloud state.
- A remote deletion removes a clean local copy; a locally divergent copy is preserved as a conflict.
- A cloud folder rename or move updates the local encoded folder path, but a destination collision stops that account before overwriting anything.
- A newly excluded folder becomes unmanaged without deleting its existing local files.
- On an expired or invalid delta token, discard only that folder's checkpoint and perform a safe full reconciliation. Do not delete local files until the replacement baseline completes.
- Skip MIME re-download only when a remote metadata update exactly matches a journaled local flag operation; otherwise refresh MIME for correctness.

## Sendmail-Compatible Sending

Use Graph's MIME `POST /me/sendMail` endpoint and require `202 Accepted` for success.

Sending rules:

- Require syntactically valid RFC message input; update README examples to include at least a valid header/body separator.
- Parse headers without normalizing or reserializing unrelated MIME content.
- Require the MIME `From`/`Sender` and `-f`, when supplied, to match the configured account case-insensitively. Reject aliases and delegated sending in the first release.
- With `-t`, send to the union of valid `To`, `Cc`, `Bcc`, and command-line recipients.
- Without `-t`, require command-line recipients and reject any header recipient absent from that command-line set.
- Add command-line recipients missing from visible headers as transient `Bcc` fields so Graph receives the intended envelope set.
- Reject messages with no recipients, malformed address fields, conflicting sender fields, or unsafe header injection.
- Base64-encode the final MIME payload as required by Graph while using a spool file or streaming encoder to avoid unbounded memory use.
- Treat `202` as accepted, not proof of final delivery, and document that limitation.

## Implementation Sequence

Every phase follows red-green-refactor: write behavior tests first, confirm they fail for the intended reason, implement the minimum behavior, and reach 100% coverage for changed behavior before continuing.

1. **Project foundation**
   - Create Cargo metadata, module skeleton, error types, domain records, Clap interfaces, configuration parser, and XDG path resolution.
   - Replace the Python-specific README description with the Rust/Graph contract.

2. **Authentication and Graph transport**
   - Implement PKCE callback, device flow, keyring storage, token rotation, authenticated requests, safe pagination, immutable-ID headers, retries, timeouts, and Graph error mapping.

3. **SQLite and Maildir adapters**
   - Add schema/migrations, repositories, operation staging, locks, reversible folder names, deterministic keys, atomic delivery, scans, flag manipulation, hashes, and conflict storage.

4. **Cloud-to-local synchronization**
   - Implement folder selection, initial delta rounds, incremental message changes, MIME streaming, remote moves/deletes, checkpoint commits, safe token reset, and dry-run planning.

5. **Local-to-cloud synchronization**
   - Seen/Flagged propagation, deterministic-key managed-folder moves, trash
     and permanent-delete semantics, pending-operation replay, and own-change
     suppression are implemented.
   - Add rewritten-key correlation by Internet Message-ID/MIME hash and
     remaining cloud-wins conflict handling.

6. **Sending and release hardening**
   - Implement RFC parsing, recipient safety, MIME Graph sending, sendmail exit codes, multi-account behavior, documentation, sample configuration, and recovery guidance.

## Test and Acceptance Plan

Use Rust unit tests beside modules and black-box integration tests under `tests/`. Add `wiremock`, `assert_cmd`, `predicates`, and `tempfile` as development dependencies. Use injectable traits rather than network, clock, sleep, keyring, or global-environment access in unit tests.

Required coverage includes:

- Valid and invalid configuration, path expansion, filters, separators, collisions, account selection, and CLI exit codes.
- PKCE state/verifier handling, device flow, refresh rotation, keyring errors, insecure fallback gating, expired credentials, and redacted diagnostics.
- Graph request construction, immutable headers, pagination-host validation, delta-link preservation, streaming, throttling, retry exhaustion, timeout, malformed JSON, and Graph error classification.
- Schema creation, unknown versions, migrations, rollbacks, staged-run recovery, pending-operation replay, locking, and transaction failures.
- Atomic Maildir delivery, placement in `new`/`cur`, sorted and preserved flags, rewritten filenames, folder encoding, duplicate keys, interrupted writes, and conflict uniqueness.
- Idempotent initial/repeated sync; every remote create/update/delete/move/folder rename; every supported local flag/trash/move; untracked and edited files; ambiguous correlations; invalid delta recovery; and all conflict combinations.
- RFC send validation, `-t`, CLI recipients, Bcc injection, sender validation, ignored `-i/-oi`, payload encoding, Graph acceptance/rejection, and multiple accounts.

Validation commands:

```text
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo llvm-cov --workspace --all-features --branch --fail-under-lines 100
```

Do not use `assert!`, `unwrap`, or `expect` to handle production failures. Return typed errors with actionable context. Document all non-trivial public types and methods, and keep logs free of tokens and message content.

Finish with an opt-in live smoke test against a disposable commercial Microsoft 365 mailbox covering initialization, initial sync, incremental read/flag/move/trash changes, token refresh, and one sent message. Never run live tests in the normal automated suite.

## Initial Scope and Deferred Work

Initial scope:

- Global commercial Microsoft 365.
- Delegated access to the signed-in user's primary mailbox.
- Complete-history synchronization.
- Serial account processing.
- Existing remote mail folders and local Maildirs.
- Seen, Flagged, trash, and managed-folder moves.
- Core sendmail compatibility.

Deferred:

- Shared or delegated mailboxes and send-as aliases.
- Archive mailboxes and sovereign Microsoft clouds.
- Uploading local messages or drafts.
- Creating, renaming, or deleting remote folders locally.
- Date-limited bootstrap and retention policies.
- Graph change notifications, daemon mode, and real-time synchronization.
- Parallel account/folder downloads and Graph batching.
- Importing state from an earlier Python implementation.

## Reference Sources

- [Exchange Online EWS retirement](https://techcommunity.microsoft.com/blog/exchange/exchange-online-ews-your-time-is-almost-up/4492361)
- [Microsoft Graph SDK supported languages](https://learn.microsoft.com/en-us/graph/sdks/sdks-overview)
- [Microsoft Graph folder delta](https://learn.microsoft.com/en-us/graph/api/mailfolder-delta?view=graph-rest-1.0)
- [Microsoft Graph message delta](https://learn.microsoft.com/en-us/graph/api/message-delta?view=graph-rest-1.0)
- [Microsoft Graph immutable Outlook IDs](https://learn.microsoft.com/en-us/graph/outlook-immutable-id)
- [Microsoft Graph MIME sending](https://learn.microsoft.com/en-us/graph/api/user-sendmail?view=graph-rest-1.0)
- [Rust `oauth2` crate](https://docs.rs/oauth2/latest/oauth2/)
- [Rust `reqwest` crate](https://docs.rs/reqwest/latest/reqwest/)
- [Rust `keyring` crate](https://docs.rs/keyring/latest/keyring/)
