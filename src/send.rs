//! RFC message validation and disk-backed MIME preparation.

use crate::auth::AuthError;
use crate::error::ExitCode;
use crate::graph::GraphError;
use base64::engine::general_purpose::STANDARD;
use mailparse::{MailAddr, MailHeader, MailHeaderMap, SingleInfo, addrparse, addrparse_header};
use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use tempfile::NamedTempFile;
use thiserror::Error;

const MAX_HEADER_BYTES: usize = 1024 * 1024;

/// Inputs that control sendmail-compatible sender and recipient validation.
#[derive(Clone, Copy, Debug)]
pub struct SendOptions<'a> {
    /// Microsoft 365 mailbox allowed to send the message.
    pub configured_sender: &'a str,
    /// Whether recipients in `To`, `Cc`, and `Bcc` authorize delivery.
    pub read_recipients_from_headers: bool,
    /// Recipient arguments supplied after the send command's options.
    pub envelope_recipients: &'a [String],
}

/// One complete input message retained on disk before account selection.
pub struct SpooledMessage {
    original: NamedTempFile,
    header_block: Vec<u8>,
    separator: Vec<u8>,
    sender: String,
}

impl SpooledMessage {
    /// Return the unique address established by `From` and optional `Sender`.
    pub fn get_sender_address(&self) -> &str {
        &self.sender
    }

    /// Validate recipients for the selected account and encode the Graph payload.
    pub fn prepare(mut self, options: &SendOptions<'_>) -> Result<PreparedMessage, SendError> {
        let parseable_headers = get_parseable_headers(&self.header_block, &self.separator);
        let (headers, _) =
            mailparse::parse_headers(&parseable_headers).map_err(|_| SendError::InvalidMessage)?;
        validate_sender(&headers, options)?;
        let missing_recipients = get_missing_header_recipients(&headers, options)?;

        self.original
            .as_file_mut()
            .seek(SeekFrom::Start(
                (self.header_block.len() + self.separator.len()) as u64,
            ))
            .map_err(|_| SendError::Spool)?;
        let mut encoded = NamedTempFile::new().map_err(|_| SendError::Spool)?;
        {
            let mut encoder = base64::write::EncoderWriter::new(encoded.as_file_mut(), &STANDARD);
            encoder
                .write_all(&self.header_block)
                .map_err(|_| SendError::Spool)?;
            write_bcc_headers(&mut encoder, &missing_recipients, &self.separator)?;
            encoder
                .write_all(&self.separator)
                .map_err(|_| SendError::Spool)?;
            std::io::copy(self.original.as_file_mut(), &mut encoder)
                .map_err(|_| SendError::Spool)?;
            encoder.finish().map_err(|_| SendError::Spool)?;
        }
        encoded.flush().map_err(|_| SendError::Spool)?;
        Ok(PreparedMessage { encoded })
    }
}

/// A validated message encoded for Graph and retained in a secure temporary file.
pub struct PreparedMessage {
    encoded: NamedTempFile,
}

impl PreparedMessage {
    /// Return the base64 payload path while this prepared message remains alive.
    pub fn get_encoded_path(&self) -> &Path {
        self.encoded.path()
    }
}

/// Safe sending failures that never include message content or access tokens.
#[derive(Debug, Error)]
pub enum SendError {
    /// The input was not a complete, parseable RFC message.
    #[error("message input is not a valid RFC message")]
    InvalidMessage,
    /// The message or envelope sender did not uniquely match the configured mailbox.
    #[error("message sender must uniquely match the configured account")]
    InvalidSender,
    /// No configured account owns the message sender address.
    #[error("message sender does not match a configured account")]
    UnconfiguredSender,
    /// A recipient field or argument did not contain a valid mailbox.
    #[error("message contains an invalid recipient address")]
    InvalidRecipient,
    /// The selected recipient mode produced no recipients.
    #[error("message has no recipients")]
    NoRecipients,
    /// Without `-t`, a header recipient was not explicitly provided as an argument.
    #[error(
        "message header contains a recipient absent from the command line; use -t to allow header recipients"
    )]
    HeaderRecipientOutsideEnvelope,
    /// A secure temporary message file could not be created, read, or written.
    #[error("could not spool the message for sending")]
    Spool,
    /// The authenticated Microsoft identity does not own the selected mailbox.
    #[error("signed-in Microsoft identity does not match the selected account")]
    IdentityMismatch,
    /// Microsoft Graph or authentication failed.
    #[error(transparent)]
    Graph(#[from] GraphError),
}

impl SendError {
    /// Return the sendmail-compatible process classification for this failure.
    pub fn get_exit_code(&self) -> ExitCode {
        match self {
            Self::InvalidMessage
            | Self::InvalidSender
            | Self::UnconfiguredSender
            | Self::InvalidRecipient
            | Self::NoRecipients
            | Self::HeaderRecipientOutsideEnvelope => ExitCode::DataError,
            Self::IdentityMismatch => ExitCode::Unavailable,
            Self::Graph(error) => get_graph_exit_code(error),
            Self::Spool => ExitCode::Software,
        }
    }
}

fn get_graph_exit_code(error: &GraphError) -> ExitCode {
    match error {
        GraphError::RetryExhausted
        | GraphError::Request
        | GraphError::Timeout
        | GraphError::SubmissionUnknown
        | GraphError::Authentication(AuthError::TokenRequest) => ExitCode::TemporaryFailure,
        GraphError::Authentication(_)
        | GraphError::Response {
            status: 401 | 403, ..
        } => ExitCode::Unavailable,
        GraphError::Response {
            status: 408 | 429 | 500 | 502 | 503 | 504,
            ..
        } => ExitCode::TemporaryFailure,
        GraphError::Response { .. } => ExitCode::Unavailable,
        GraphError::UnexpectedUrl
        | GraphError::HttpClient
        | GraphError::MalformedJson
        | GraphError::MalformedDeltaLinks
        | GraphError::MalformedFolder
        | GraphError::MalformedMessage
        | GraphError::MissingMessageReadState
        | GraphError::MissingMessageModificationTime
        | GraphError::MissingMessageFlagState
        | GraphError::OutputFile
        | GraphError::InputFile
        | GraphError::UnexpectedSendStatus(_) => ExitCode::Software,
    }
}

/// Validate an RFC message and stream its Graph base64 payload to disk.
///
/// Original header and body bytes are preserved. When a command-line recipient
/// is not already represented in the headers, one `Bcc` header is inserted
/// immediately before the original header/body separator.
pub fn prepare_message<R: Read>(
    input: R,
    options: &SendOptions<'_>,
) -> Result<PreparedMessage, SendError> {
    spool_message(input)?.prepare(options)
}

/// Stream one complete message to disk and extract its consistent sender.
///
/// This phase permits account selection without retaining an unbounded message
/// in memory or performing any authentication or network operations.
pub fn spool_message<R: Read>(mut input: R) -> Result<SpooledMessage, SendError> {
    let mut original = NamedTempFile::new().map_err(|_| SendError::Spool)?;
    let (header_block, separator) = spool_through_headers(&mut input, &mut original)?;
    std::io::copy(&mut input, &mut original).map_err(|_| SendError::Spool)?;
    original.flush().map_err(|_| SendError::Spool)?;

    let parseable_headers = get_parseable_headers(&header_block, &separator);
    let (headers, _) =
        mailparse::parse_headers(&parseable_headers).map_err(|_| SendError::InvalidMessage)?;
    let sender = get_consistent_sender(&headers)?;
    Ok(SpooledMessage {
        original,
        header_block,
        separator,
        sender,
    })
}

fn get_parseable_headers(header_block: &[u8], separator: &[u8]) -> Vec<u8> {
    let mut parseable_headers = Vec::with_capacity(header_block.len() + separator.len());
    parseable_headers.extend_from_slice(header_block);
    parseable_headers.extend_from_slice(separator);
    parseable_headers
}

fn spool_through_headers<R: Read, W: Write>(
    input: &mut R,
    output: &mut W,
) -> Result<(Vec<u8>, Vec<u8>), SendError> {
    let mut headers = Vec::new();
    let mut line = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        match input.read(&mut byte) {
            Ok(0) => return Err(SendError::InvalidMessage),
            Ok(_) => {
                output.write_all(&byte).map_err(|_| SendError::Spool)?;
                line.push(byte[0]);
                if byte[0] != b'\n' {
                    if headers.len() + line.len() > MAX_HEADER_BYTES {
                        return Err(SendError::InvalidMessage);
                    }
                    continue;
                }
                if line == b"\n" || line == b"\r\n" {
                    return Ok((headers, line));
                }
                if headers.len() + line.len() > MAX_HEADER_BYTES {
                    return Err(SendError::InvalidMessage);
                }
                headers.extend_from_slice(&line);
                line.clear();
            }
            Err(_) => return Err(SendError::Spool),
        }
    }
}

fn validate_sender(headers: &[MailHeader<'_>], options: &SendOptions<'_>) -> Result<(), SendError> {
    let sender = get_consistent_sender(headers)?;
    if !sender.eq_ignore_ascii_case(options.configured_sender) {
        return Err(SendError::InvalidSender);
    }
    Ok(())
}

fn get_consistent_sender(headers: &[MailHeader<'_>]) -> Result<String, SendError> {
    let from = get_unique_header_mailbox(headers, "From")?.ok_or(SendError::InvalidSender)?;
    if let Some(sender) = get_unique_header_mailbox(headers, "Sender")?
        && !sender.eq_ignore_ascii_case(&from)
    {
        return Err(SendError::InvalidSender);
    }
    Ok(from)
}

fn get_unique_header_mailbox(
    headers: &[MailHeader<'_>],
    name: &str,
) -> Result<Option<String>, SendError> {
    let matching = headers.get_all_headers(name);
    if matching.is_empty() {
        return Ok(None);
    }
    if matching.len() != 1 {
        return Err(SendError::InvalidSender);
    }
    let addresses = addrparse_header(matching[0]).map_err(|_| SendError::InvalidSender)?;
    let mailbox = addresses
        .extract_single_info()
        .ok_or(SendError::InvalidSender)?;
    validate_mailbox(&mailbox.addr).map_err(|_| SendError::InvalidSender)?;
    Ok(Some(mailbox.addr))
}

fn get_missing_header_recipients(
    headers: &[MailHeader<'_>],
    options: &SendOptions<'_>,
) -> Result<Vec<String>, SendError> {
    let header_recipients = get_header_recipients(headers)?;
    let envelope_recipients = options
        .envelope_recipients
        .iter()
        .map(|recipient| get_single_mailbox(recipient))
        .collect::<Result<Vec<_>, _>>()?;
    let header_keys: HashSet<String> = header_recipients
        .iter()
        .map(|address| address.to_lowercase())
        .collect();
    let envelope_recipients = get_unique_addresses(envelope_recipients);
    let envelope_keys: HashSet<String> = envelope_recipients
        .iter()
        .map(|address| address.to_lowercase())
        .collect();

    if !options.read_recipients_from_headers {
        if envelope_recipients.is_empty() {
            return Err(SendError::NoRecipients);
        }
        if header_keys
            .iter()
            .any(|address| !envelope_keys.contains(address))
        {
            return Err(SendError::HeaderRecipientOutsideEnvelope);
        }
    } else if header_keys.is_empty() && envelope_recipients.is_empty() {
        return Err(SendError::NoRecipients);
    }

    Ok(envelope_recipients
        .into_iter()
        .filter(|address| !header_keys.contains(&address.to_lowercase()))
        .collect())
}

fn get_header_recipients(headers: &[MailHeader<'_>]) -> Result<Vec<String>, SendError> {
    let mut recipients = Vec::new();
    for name in ["To", "Cc", "Bcc"] {
        for header in headers.get_all_headers(name) {
            let addresses = addrparse_header(header).map_err(|_| SendError::InvalidRecipient)?;
            for address in addresses.iter() {
                match address {
                    MailAddr::Single(single) => recipients.push(get_valid_address(single)?),
                    MailAddr::Group(group) => {
                        for single in &group.addrs {
                            recipients.push(get_valid_address(single)?);
                        }
                    }
                }
            }
        }
    }
    Ok(recipients)
}

fn get_valid_address(single: &SingleInfo) -> Result<String, SendError> {
    validate_mailbox(&single.addr)?;
    Ok(single.addr.clone())
}

fn get_single_mailbox(value: &str) -> Result<String, SendError> {
    if value.chars().any(char::is_control) {
        return Err(SendError::InvalidRecipient);
    }
    let mailbox = addrparse(value)
        .map_err(|_| SendError::InvalidRecipient)?
        .extract_single_info()
        .ok_or(SendError::InvalidRecipient)?;
    get_valid_address(&mailbox)
}

fn validate_mailbox(address: &str) -> Result<(), SendError> {
    let (local, domain) = address
        .rsplit_once('@')
        .ok_or(SendError::InvalidRecipient)?;
    if local.is_empty()
        || domain.is_empty()
        || address.chars().any(|character| {
            character.is_control() || character.is_whitespace() || matches!(character, '<' | '>')
        })
    {
        return Err(SendError::InvalidRecipient);
    }
    Ok(())
}

fn get_unique_addresses(addresses: Vec<String>) -> Vec<String> {
    let mut keys = HashSet::new();
    let mut unique = Vec::new();
    for address in addresses {
        if keys.insert(address.to_lowercase()) {
            unique.push(address);
        }
    }
    unique
}

fn write_bcc_headers<W: Write>(
    output: &mut W,
    recipients: &[String],
    separator: &[u8],
) -> Result<(), SendError> {
    if recipients.is_empty() {
        return Ok(());
    }
    let newline = if separator == b"\r\n" {
        b"\r\n".as_slice()
    } else {
        b"\n".as_slice()
    };
    output.write_all(b"Bcc: ").map_err(|_| SendError::Spool)?;
    output
        .write_all(recipients.join(", ").as_bytes())
        .map_err(|_| SendError::Spool)?;
    output.write_all(newline).map_err(|_| SendError::Spool)
}
