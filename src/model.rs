//! Domain records shared by adapters and synchronization logic.

/// Follow-up state synchronized with the Maildir `F` flag.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FollowUpState {
    /// The message has no follow-up flag.
    #[default]
    NotFlagged,
    /// The message is flagged or has a completed Graph follow-up flag.
    Flagged,
}

/// Synchronizable message flags independent of Graph and Maildir types.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MessageFlags {
    /// Whether the message has been read.
    pub is_read: bool,
    /// Whether the message has a follow-up flag.
    pub follow_up: FollowUpState,
}

/// A validated remote mail-folder identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteFolder {
    /// Immutable Graph folder identifier.
    pub id: String,
    /// Immutable identifier of the parent folder, when present.
    pub parent_id: Option<String>,
    /// Complete `/`-delimited remote folder path.
    pub path: String,
}

/// Message metadata used by synchronization without exposing Graph DTOs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteMessage {
    /// Immutable Graph message identifier.
    pub id: String,
    /// Immutable identifier of the containing folder.
    pub folder_id: String,
    /// RFC Internet Message-ID when supplied by the message.
    pub internet_message_id: Option<String>,
    /// Opaque Graph modification value used to recognize replayed changes.
    pub remote_version: String,
    /// Flags that can be represented locally.
    pub flags: MessageFlags,
}

/// Raw folder metadata returned by a Graph folder delta before path resolution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteFolderMetadata {
    /// Immutable Graph folder identifier.
    pub id: String,
    /// Immutable parent folder identifier, when present.
    pub parent_id: Option<String>,
    /// Current user-visible folder name.
    pub display_name: String,
    /// Whether Graph marks this as a hidden folder.
    pub is_hidden: bool,
    /// Current Graph estimate of all Outlook items directly in this folder.
    pub total_item_count: u32,
}

/// One upsert or deletion emitted by a Graph delta round.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeltaChange<T> {
    /// Create or replace the supplied resource metadata.
    Upsert(T),
    /// Delete the resource with this immutable identifier.
    Delete {
        /// Immutable identifier of the removed resource.
        id: String,
    },
}

/// One validated page from a Graph delta round.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeltaPage<T> {
    /// Resource changes in server-provided order.
    pub changes: Vec<DeltaChange<T>>,
    /// Opaque URL for the next page in the same round.
    pub next_link: Option<String>,
    /// Opaque checkpoint URL completing this round.
    pub delta_link: Option<String>,
}
