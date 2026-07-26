use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use nochange::send::{SendError, SendOptions, prepare_message};
use std::fs;
use std::io::{Cursor, Read};

fn get_decoded_message(message: &nochange::send::PreparedMessage) -> Vec<u8> {
    let encoded =
        fs::read(message.get_encoded_path()).expect("encoded message should remain readable");
    STANDARD
        .decode(encoded)
        .expect("prepared message should contain valid base64")
}

#[test]
fn preserves_the_original_mime_when_all_recipients_are_in_headers() {
    let original = b"From: Sender <sender@example.com>\r\n\
To: Person <person@example.com>\r\n\
Subject: encoded =?UTF-8?Q?subject?=\r\n\
X-Unrelated: \xff\r\n\
\r\n\
Body bytes: \x00\xff\r\n";
    let recipients = vec!["person@example.com".to_owned()];
    let options = SendOptions {
        configured_sender: "sender@example.com",
        envelope_sender: None,
        read_recipients_from_headers: false,
        envelope_recipients: &recipients,
    };

    let prepared =
        prepare_message(Cursor::new(original), &options).expect("valid message should be prepared");

    assert_eq!(get_decoded_message(&prepared), original);
}

#[test]
fn adds_missing_envelope_recipients_as_bcc_without_reserializing_the_message() {
    let original =
        b"From: sender@example.com\nTo: visible@example.com\nSubject: Test\n\nExact body\n";
    let recipients = vec![
        "visible@example.com".to_owned(),
        "hidden@example.com".to_owned(),
    ];
    let options = SendOptions {
        configured_sender: "SENDER@example.com",
        envelope_sender: Some("Sender@Example.Com"),
        read_recipients_from_headers: false,
        envelope_recipients: &recipients,
    };

    let prepared = prepare_message(Cursor::new(original), &options)
        .expect("missing envelope recipient should be injected");

    assert_eq!(
        get_decoded_message(&prepared),
        b"From: sender@example.com\nTo: visible@example.com\nSubject: Test\n\
Bcc: hidden@example.com\n\nExact body\n"
    );
}

#[test]
fn header_recipient_mode_unions_header_and_command_line_recipients() {
    let original = b"From: sender@example.com\r\n\
To: one@example.com\r\n\
Cc: Friends: two@example.com, three@example.com;\r\n\
Bcc: hidden@example.com\r\n\
\r\n\
Body\r\n";
    let recipients = vec!["THREE@example.com".to_owned(), "cli@example.com".to_owned()];
    let options = SendOptions {
        configured_sender: "sender@example.com",
        envelope_sender: None,
        read_recipients_from_headers: true,
        envelope_recipients: &recipients,
    };

    let prepared = prepare_message(Cursor::new(original), &options)
        .expect("header and command-line recipients should be combined");

    assert_eq!(
        get_decoded_message(&prepared),
        b"From: sender@example.com\r\n\
To: one@example.com\r\n\
Cc: Friends: two@example.com, three@example.com;\r\n\
Bcc: hidden@example.com\r\n\
Bcc: cli@example.com\r\n\
\r\n\
Body\r\n"
    );
}

#[test]
fn rejects_header_recipients_missing_from_the_envelope_without_t() {
    let original = b"From: sender@example.com\r\nTo: other@example.com\r\n\r\nBody\r\n";
    let recipients = vec!["cli@example.com".to_owned()];
    let options = SendOptions {
        configured_sender: "sender@example.com",
        envelope_sender: None,
        read_recipients_from_headers: false,
        envelope_recipients: &recipients,
    };

    assert!(matches!(
        prepare_message(Cursor::new(original), &options),
        Err(SendError::HeaderRecipientOutsideEnvelope)
    ));
}

#[test]
fn rejects_missing_or_conflicting_senders() {
    let recipients = vec!["person@example.com".to_owned()];
    let cases: &[(&[u8], Option<&str>)] = &[
        (
            b"To: person@example.com\r\n\r\nBody\r\n",
            None,
        ),
        (
            b"From: other@example.com\r\nTo: person@example.com\r\n\r\nBody\r\n",
            None,
        ),
        (
            b"From: sender@example.com\r\nSender: other@example.com\r\nTo: person@example.com\r\n\r\nBody\r\n",
            None,
        ),
        (
            b"From: sender@example.com\r\nTo: person@example.com\r\n\r\nBody\r\n",
            Some("other@example.com"),
        ),
        (
            b"From: one@example.com, two@example.com\r\nTo: person@example.com\r\n\r\nBody\r\n",
            None,
        ),
    ];

    for (message, envelope_sender) in cases {
        let options = SendOptions {
            configured_sender: "sender@example.com",
            envelope_sender: *envelope_sender,
            read_recipients_from_headers: false,
            envelope_recipients: &recipients,
        };
        assert!(
            matches!(
                prepare_message(Cursor::new(*message), &options),
                Err(SendError::InvalidSender)
            ),
            "sender case should be rejected"
        );
    }
}

#[test]
fn rejects_malformed_message_and_recipient_input() {
    let valid = b"From: sender@example.com\r\nTo: person@example.com\r\n\r\nBody\r\n";
    let cases: &[(&[u8], Vec<String>, bool)] = &[
        (
            b"From: sender@example.com\r\nTo: person@example.com\r\nBody",
            vec!["person@example.com".to_owned()],
            false,
        ),
        (
            b"From sender@example.com\r\n\r\nBody\r\n",
            vec!["person@example.com".to_owned()],
            false,
        ),
        (valid, Vec::new(), false),
        (
            valid,
            vec!["bad\r\nBcc: injected@example.com".to_owned()],
            false,
        ),
        (
            b"From: sender@example.com\r\nTo: malformed\r\n\r\nBody\r\n",
            Vec::new(),
            true,
        ),
    ];

    for (message, recipients, read_headers) in cases {
        let options = SendOptions {
            configured_sender: "sender@example.com",
            envelope_sender: None,
            read_recipients_from_headers: *read_headers,
            envelope_recipients: recipients,
        };
        assert!(
            prepare_message(Cursor::new(*message), &options).is_err(),
            "malformed input should be rejected"
        );
    }
}

#[test]
fn rejects_unbounded_headers_and_reports_input_read_failures_safely() {
    let mut oversized = b"From: sender@example.com\nX-Large: ".to_vec();
    oversized.extend(std::iter::repeat_n(b'a', 1024 * 1024));
    oversized.extend_from_slice(b"\n\nBody\n");
    let recipients = vec!["person@example.com".to_owned()];
    let options = SendOptions {
        configured_sender: "sender@example.com",
        envelope_sender: None,
        read_recipients_from_headers: false,
        envelope_recipients: &recipients,
    };

    assert!(matches!(
        prepare_message(Cursor::new(oversized), &options),
        Err(SendError::InvalidMessage)
    ));
    assert!(matches!(
        prepare_message(FailingReader, &options),
        Err(SendError::Spool)
    ));
}

struct FailingReader;

impl Read for FailingReader {
    fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("intentional test failure"))
    }
}
