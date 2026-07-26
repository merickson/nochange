//! Hardened Microsoft Graph URL and transport policies.

use crate::auth::{AccessTokenProvider, AuthError};
use crate::model::{
    DeltaChange, DeltaPage, FollowUpState, MessageFlags, RemoteFolderMetadata, RemoteMessage,
};
use async_trait::async_trait;
use futures_util::StreamExt;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::Method;
use secrecy::ExposeSecret;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use url::Url;

const GRAPH_ROOT: &str = "https://graph.microsoft.com/v1.0";
const DEFAULT_MAX_ATTEMPTS: u32 = 8;
const DEFAULT_MAX_TOTAL_DELAY: Duration = Duration::from_secs(5 * 60);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const IMMUTABLE_ID_PREFERENCE: &str = "IdType=\"ImmutableId\"";
const DELTA_PREFERENCE: &str = "IdType=\"ImmutableId\", odata.maxpagesize=1000";

/// Validated URL for a Microsoft Graph v1.0 request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphUrl(String);

impl GraphUrl {
    /// Build a v1.0 URL from a relative endpoint or validate an opaque server link.
    pub fn build(endpoint: &str) -> Result<Self, GraphError> {
        let request_url = if endpoint.starts_with('/') && !endpoint.starts_with("//") {
            format!("{GRAPH_ROOT}{endpoint}")
        } else if endpoint.starts_with("https://") {
            endpoint.to_owned()
        } else {
            return Err(GraphError::UnexpectedUrl);
        };
        validate_graph_url(&request_url)?;
        Ok(Self(request_url))
    }

    /// Return the exact URL string retained for the HTTP request.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Bounded retry settings for transient Graph responses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Maximum retry number permitted after the initial attempt.
    pub max_attempts: u32,
    /// Maximum cumulative sleep across one request.
    pub max_total_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            max_total_delay: DEFAULT_MAX_TOTAL_DELAY,
        }
    }
}

impl RetryPolicy {
    /// Return the required delay, `None` for permanent responses, or exhaustion.
    pub fn get_retry_delay(
        &self,
        status: u16,
        retry_after: Option<&str>,
        retry_number: u32,
        total_delay: Duration,
        now: SystemTime,
    ) -> Result<Option<Duration>, GraphError> {
        if !is_retryable_status(status) {
            return Ok(None);
        }
        if retry_number >= self.max_attempts {
            return Err(GraphError::RetryExhausted);
        }
        let delay = retry_after
            .and_then(|value| parse_retry_after(value, now))
            .unwrap_or_else(|| get_exponential_delay(retry_number));
        let Some(next_total) = total_delay.checked_add(delay) else {
            return Err(GraphError::RetryExhausted);
        };
        if next_total > self.max_total_delay {
            return Err(GraphError::RetryExhausted);
        }
        Ok(Some(delay))
    }
}

/// Injectable delay boundary for deterministic retry tests.
#[async_trait]
pub trait Sleeper: Send + Sync {
    /// Wait for the requested retry delay.
    async fn sleep(&self, duration: Duration);
}

/// Tokio-backed production retry sleeper.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokioSleeper;

#[async_trait]
impl Sleeper for TokioSleeper {
    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

/// Authenticated, retrying transport for Graph v1.0 JSON requests.
pub struct GraphTransport<P, S> {
    token_provider: Arc<P>,
    sleeper: Arc<S>,
    retry_policy: RetryPolicy,
    http_client: reqwest::Client,
    fsync_enabled: bool,
}

impl<P, S> GraphTransport<P, S>
where
    P: AccessTokenProvider,
    S: Sleeper,
{
    /// Build a hardened Graph client with redirects disabled and explicit timeouts.
    pub fn build(token_provider: Arc<P>, sleeper: Arc<S>) -> Result<Self, GraphError> {
        Self::build_with_fsync(token_provider, sleeper, true)
    }

    /// Build a Graph client with MIME-file fsync explicitly enabled or disabled.
    pub fn build_with_fsync(
        token_provider: Arc<P>,
        sleeper: Arc<S>,
        fsync_enabled: bool,
    ) -> Result<Self, GraphError> {
        let http_client = build_http_client()?;
        Ok(Self {
            token_provider,
            sleeper,
            retry_policy: RetryPolicy::default(),
            http_client,
            fsync_enabled,
        })
    }

    #[cfg(test)]
    fn build_for_test(
        token_provider: Arc<P>,
        sleeper: Arc<S>,
        retry_policy: RetryPolicy,
    ) -> Result<Self, GraphError> {
        Self::build_for_test_with_timeout(
            token_provider,
            sleeper,
            retry_policy,
            DEFAULT_REQUEST_TIMEOUT,
        )
    }

    #[cfg(test)]
    fn build_for_test_with_timeout(
        token_provider: Arc<P>,
        sleeper: Arc<S>,
        retry_policy: RetryPolicy,
        request_timeout: Duration,
    ) -> Result<Self, GraphError> {
        Ok(Self {
            token_provider,
            sleeper,
            retry_policy,
            http_client: build_http_client_with_timeout(request_timeout)?,
            fsync_enabled: true,
        })
    }

    /// Execute a Graph `GET` and deserialize its successful JSON body.
    pub async fn get_json<T>(&self, url: &GraphUrl) -> Result<T, GraphError>
    where
        T: DeserializeOwned,
    {
        self.get_json_with_preference(url, IMMUTABLE_ID_PREFERENCE)
            .await
    }

    async fn get_json_with_preference<T>(
        &self,
        url: &GraphUrl,
        preference: &'static str,
    ) -> Result<T, GraphError>
    where
        T: DeserializeOwned,
    {
        let mut retry_number = 0;
        let mut total_delay = Duration::ZERO;
        loop {
            let response = self
                .get_success_response(url, "application/json", preference)
                .await?;
            match response.bytes().await {
                Ok(body) => {
                    return serde_json::from_slice(&body).map_err(|_| GraphError::MalformedJson);
                }
                Err(error) if is_retryable_request_error(&error) => {
                    self.sleep_before_transport_retry(&mut retry_number, &mut total_delay)
                        .await?;
                }
                Err(error) => return Err(classify_request_error(error)),
            }
        }
    }

    /// Fetch one validated page of mailbox-folder delta changes.
    pub async fn get_folder_delta_page(
        &self,
        checkpoint: Option<&str>,
    ) -> Result<DeltaPage<RemoteFolderMetadata>, GraphError> {
        let url = match checkpoint {
            Some(checkpoint) => GraphUrl::build(checkpoint)?,
            None => get_initial_folder_delta_url()?,
        };
        self.get_folder_delta_page_from(&url).await
    }

    /// Fetch one validated page of per-folder message delta changes.
    pub async fn get_message_delta_page(
        &self,
        folder_id: &str,
        checkpoint: Option<&str>,
    ) -> Result<DeltaPage<RemoteMessage>, GraphError> {
        let url = match checkpoint {
            Some(checkpoint) => GraphUrl::build(checkpoint)?,
            None => get_initial_message_delta_url(folder_id)?,
        };
        self.get_message_delta_page_from(folder_id, &url).await
    }

    /// Stream one message's MIME representation to a newly created file.
    pub async fn download_message(
        &self,
        message_id: &str,
        destination: &std::path::Path,
    ) -> Result<(), GraphError> {
        let encoded_id = utf8_percent_encode(message_id, NON_ALPHANUMERIC);
        let url = GraphUrl::build(&format!("/me/messages/{encoded_id}/$value"))?;
        self.download_to(&url, destination).await
    }

    /// Stream a prepared base64 MIME file to Graph's send-mail endpoint.
    ///
    /// Explicit rejection responses are retried according to the bounded
    /// transport policy. A transport failure after the POST begins is not
    /// replayed because Graph may already have accepted the message.
    pub async fn send_mime_file(&self, payload: &std::path::Path) -> Result<(), GraphError> {
        let url = GraphUrl::build("/me/sendMail")?;
        self.send_mime_file_to(&url, payload).await
    }

    async fn send_mime_file_to(
        &self,
        url: &GraphUrl,
        payload: &std::path::Path,
    ) -> Result<(), GraphError> {
        let mut force_refresh = false;
        let mut refreshed_after_unauthorized = false;
        let mut retry_number = 0;
        let mut total_delay = Duration::ZERO;
        loop {
            let access_token = match self.token_provider.get_access_token(force_refresh).await {
                Ok(access_token) => access_token,
                Err(AuthError::TokenRequest) => {
                    self.sleep_before_transport_retry(&mut retry_number, &mut total_delay)
                        .await?;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            force_refresh = false;
            let input = tokio::fs::File::open(payload)
                .await
                .map_err(|_| GraphError::InputFile)?;
            let response = self
                .http_client
                .post(url.as_str())
                .bearer_auth(access_token.expose_secret())
                .header(reqwest::header::CONTENT_TYPE, "text/plain")
                .body(reqwest::Body::from(input))
                .send()
                .await
                .map_err(|_| GraphError::SubmissionUnknown)?;
            let status = response.status().as_u16();
            if status == 401 && !refreshed_after_unauthorized {
                refreshed_after_unauthorized = true;
                force_refresh = true;
                continue;
            }
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok());
            if let Some(delay) = self.retry_policy.get_retry_delay(
                status,
                retry_after,
                retry_number,
                total_delay,
                SystemTime::now(),
            )? {
                self.sleeper.sleep(delay).await;
                total_delay += delay;
                retry_number += 1;
                continue;
            }
            if status == 202 {
                return Ok(());
            }
            if response.status().is_success() {
                return Err(GraphError::UnexpectedSendStatus(status));
            }
            let request_id = response
                .headers()
                .get("request-id")
                .and_then(|value| value.to_str().ok())
                .and_then(get_safe_diagnostic_value);
            let body = response.bytes().await.map_err(|_| GraphError::Request)?;
            return Err(build_response_error(status, request_id, &body));
        }
    }

    /// Update the supported read and follow-up flags for one immutable message.
    pub async fn update_message_flags(
        &self,
        message_id: &str,
        flags: MessageFlags,
    ) -> Result<(), GraphError> {
        validate_resource_id(message_id)?;
        let encoded_id = utf8_percent_encode(message_id, NON_ALPHANUMERIC);
        let url = GraphUrl::build(&format!("/me/messages/{encoded_id}"))?;
        self.update_message_flags_at(&url, flags).await
    }

    /// Resolve the immutable ID of the mailbox's well-known Deleted Items folder.
    pub async fn get_deleted_items_folder_id(&self) -> Result<String, GraphError> {
        let url = GraphUrl::build("/me/mailFolders/deleteditems?$select=id")?;
        self.get_deleted_items_folder_id_from(&url).await
    }

    async fn get_deleted_items_folder_id_from(&self, url: &GraphUrl) -> Result<String, GraphError> {
        let folder: FolderIdResource = self.get_json(url).await?;
        validate_resource_id(&folder.id)?;
        Ok(folder.id)
    }

    /// Move one immutable message to an immutable destination folder.
    pub async fn move_message(
        &self,
        message_id: &str,
        destination_folder_id: &str,
    ) -> Result<(), GraphError> {
        validate_resource_id(message_id)?;
        validate_resource_id(destination_folder_id)?;
        let encoded_id = utf8_percent_encode(message_id, NON_ALPHANUMERIC);
        let url = GraphUrl::build(&format!("/me/messages/{encoded_id}/move"))?;
        self.move_message_at(&url, destination_folder_id).await
    }

    async fn move_message_at(
        &self,
        url: &GraphUrl,
        destination_folder_id: &str,
    ) -> Result<(), GraphError> {
        validate_resource_id(destination_folder_id)?;
        let body = serde_json::json!({"destinationId": destination_folder_id});
        self.send_success_response(
            Method::POST,
            url,
            "application/json",
            IMMUTABLE_ID_PREFERENCE,
            Some(&body),
        )
        .await?;
        Ok(())
    }

    /// Permanently delete one immutable message, accepting an idempotent replay.
    pub async fn delete_message(&self, message_id: &str) -> Result<(), GraphError> {
        validate_resource_id(message_id)?;
        let encoded_id = utf8_percent_encode(message_id, NON_ALPHANUMERIC);
        let url = GraphUrl::build(&format!("/me/messages/{encoded_id}"))?;
        self.delete_message_at(&url).await
    }

    async fn delete_message_at(&self, url: &GraphUrl) -> Result<(), GraphError> {
        match self
            .send_success_response(
                Method::DELETE,
                url,
                "application/json",
                IMMUTABLE_ID_PREFERENCE,
                None,
            )
            .await
        {
            Ok(_) | Err(GraphError::Response { status: 404, .. }) => Ok(()),
            Err(error) => Err(error),
        }
    }

    async fn update_message_flags_at(
        &self,
        url: &GraphUrl,
        flags: MessageFlags,
    ) -> Result<(), GraphError> {
        let flag_status = match flags.follow_up {
            FollowUpState::NotFlagged => "notFlagged",
            FollowUpState::Flagged => "flagged",
        };
        let body = serde_json::json!({
            "isRead": flags.is_read,
            "flag": {"flagStatus": flag_status},
        });
        self.send_success_response(
            Method::PATCH,
            url,
            "application/json",
            IMMUTABLE_ID_PREFERENCE,
            Some(&body),
        )
        .await?;
        Ok(())
    }

    /// Stream a successful Graph response to a newly created destination file.
    pub async fn download_to(
        &self,
        url: &GraphUrl,
        destination: &std::path::Path,
    ) -> Result<(), GraphError> {
        let mut retry_number = 0;
        let mut total_delay = Duration::ZERO;
        loop {
            match self.download_once(url, destination).await {
                Ok(()) => return Ok(()),
                Err(GraphError::Request | GraphError::Timeout) => {
                    self.sleep_before_transport_retry(&mut retry_number, &mut total_delay)
                        .await?;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Attempt one MIME transfer, removing any partial destination on failure.
    async fn download_once(
        &self,
        url: &GraphUrl,
        destination: &std::path::Path,
    ) -> Result<(), GraphError> {
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut output = options
            .open(destination)
            .await
            .map_err(|_| GraphError::OutputFile)?;
        let response = match self
            .get_success_response(url, "message/rfc822", IMMUTABLE_ID_PREFERENCE)
            .await
        {
            Ok(response) => response,
            Err(error) => {
                drop(output);
                let _remove_result = tokio::fs::remove_file(destination).await;
                return Err(error);
            }
        };
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    drop(output);
                    let _remove_result = tokio::fs::remove_file(destination).await;
                    return Err(classify_request_error(error));
                }
            };
            if output.write_all(&chunk).await.is_err() {
                drop(output);
                let _remove_result = tokio::fs::remove_file(destination).await;
                return Err(GraphError::OutputFile);
            }
        }
        if self.fsync_enabled && output.sync_all().await.is_err() {
            drop(output);
            let _remove_result = tokio::fs::remove_file(destination).await;
            return Err(GraphError::OutputFile);
        }
        Ok(())
    }

    async fn get_folder_delta_page_from(
        &self,
        url: &GraphUrl,
    ) -> Result<DeltaPage<RemoteFolderMetadata>, GraphError> {
        let envelope: DeltaEnvelope<serde_json::Value> =
            self.get_json_with_preference(url, DELTA_PREFERENCE).await?;
        let links = build_delta_links(envelope.next_link, envelope.delta_link)?;
        let changes = envelope
            .value
            .into_iter()
            .map(|value| serde_json::from_value(value).map_err(|_| GraphError::MalformedFolder))
            .collect::<Result<Vec<FolderDeltaResource>, _>>()?
            .into_iter()
            .map(build_folder_change)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(DeltaPage {
            changes,
            next_link: links.0,
            delta_link: links.1,
        })
    }

    async fn get_message_delta_page_from(
        &self,
        folder_id: &str,
        url: &GraphUrl,
    ) -> Result<DeltaPage<RemoteMessage>, GraphError> {
        let envelope: DeltaEnvelope<serde_json::Value> =
            self.get_json_with_preference(url, DELTA_PREFERENCE).await?;
        let links = build_delta_links(envelope.next_link, envelope.delta_link)?;
        let mut changes = Vec::with_capacity(envelope.value.len());
        for value in envelope.value {
            let resource: MessageDeltaResource =
                serde_json::from_value(value).map_err(|_| GraphError::MalformedMessage)?;
            let message_id = resource.id.clone();
            match build_message_change(resource, folder_id) {
                Ok(change) => changes.push(change),
                Err(error) if is_incomplete_message_error(&error) => {
                    changes.push(
                        self.get_message_metadata_from(&message_id, folder_id, url)
                            .await?,
                    );
                }
                Err(error) => return Err(error),
            }
        }
        Ok(DeltaPage {
            changes,
            next_link: links.0,
            delta_link: links.1,
        })
    }

    async fn get_message_metadata_from(
        &self,
        message_id: &str,
        requested_folder_id: &str,
        delta_url: &GraphUrl,
    ) -> Result<DeltaChange<RemoteMessage>, GraphError> {
        validate_resource_id(message_id).map_err(|_| GraphError::MalformedMessage)?;
        let url = get_message_metadata_url(delta_url, message_id)?;
        let value: serde_json::Value = match self.get_json(&url).await {
            Ok(value) => value,
            Err(GraphError::Response { status: 404, .. }) => {
                return Ok(DeltaChange::Delete {
                    id: message_id.to_owned(),
                });
            }
            Err(error) => return Err(error),
        };
        let resource: MessageDeltaResource =
            serde_json::from_value(value).map_err(|_| GraphError::MalformedMessage)?;
        build_message_change(resource, requested_folder_id)
    }

    async fn get_success_response(
        &self,
        url: &GraphUrl,
        accept: &'static str,
        preference: &'static str,
    ) -> Result<reqwest::Response, GraphError> {
        self.send_success_response(Method::GET, url, accept, preference, None)
            .await
    }

    async fn send_success_response(
        &self,
        method: Method,
        url: &GraphUrl,
        accept: &'static str,
        preference: &'static str,
        body: Option<&serde_json::Value>,
    ) -> Result<reqwest::Response, GraphError> {
        let mut force_refresh = false;
        let mut refreshed_after_unauthorized = false;
        let mut retry_number = 0;
        let mut total_delay = Duration::ZERO;
        loop {
            let access_token = match self.token_provider.get_access_token(force_refresh).await {
                Ok(access_token) => access_token,
                Err(AuthError::TokenRequest) => {
                    self.sleep_before_transport_retry(&mut retry_number, &mut total_delay)
                        .await?;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            force_refresh = false;
            let mut request = self
                .http_client
                .request(method.clone(), url.as_str())
                .bearer_auth(access_token.expose_secret())
                .header("Prefer", preference)
                .header(reqwest::header::ACCEPT, accept);
            if let Some(body) = body {
                request = request.json(body);
            }
            let response = match request.send().await {
                Ok(response) => response,
                Err(error) if is_retryable_request_error(&error) => {
                    self.sleep_before_transport_retry(&mut retry_number, &mut total_delay)
                        .await?;
                    continue;
                }
                Err(error) => return Err(classify_request_error(error)),
            };
            let status = response.status().as_u16();
            if status == 401 && !refreshed_after_unauthorized {
                refreshed_after_unauthorized = true;
                force_refresh = true;
                continue;
            }
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok());
            if let Some(delay) = self.retry_policy.get_retry_delay(
                status,
                retry_after,
                retry_number,
                total_delay,
                SystemTime::now(),
            )? {
                self.sleeper.sleep(delay).await;
                total_delay += delay;
                retry_number += 1;
                continue;
            }
            let request_id = response
                .headers()
                .get("request-id")
                .and_then(|value| value.to_str().ok())
                .and_then(get_safe_diagnostic_value);
            let successful = response.status().is_success();
            if successful {
                return Ok(response);
            }
            match response.bytes().await {
                Ok(body) => return Err(build_response_error(status, request_id, &body)),
                Err(error) if is_retryable_request_error(&error) => {
                    self.sleep_before_transport_retry(&mut retry_number, &mut total_delay)
                        .await?;
                }
                Err(error) => return Err(classify_request_error(error)),
            }
        }
    }

    /// Apply the bounded exponential delay shared by transport and token retries.
    async fn sleep_before_transport_retry(
        &self,
        retry_number: &mut u32,
        total_delay: &mut Duration,
    ) -> Result<(), GraphError> {
        let delay = self
            .retry_policy
            .get_retry_delay(503, None, *retry_number, *total_delay, SystemTime::now())?
            .ok_or(GraphError::RetryExhausted)?;
        self.sleeper.sleep(delay).await;
        *total_delay += delay;
        *retry_number += 1;
        Ok(())
    }
}

/// Mockable Microsoft Graph operations required by cloud-to-local sync.
#[async_trait]
pub trait GraphApi: Send + Sync {
    /// Fetch one folder delta page, starting a round when no checkpoint is supplied.
    async fn get_folder_delta_page(
        &self,
        checkpoint: Option<&str>,
    ) -> Result<DeltaPage<RemoteFolderMetadata>, GraphError>;

    /// Fetch one message delta page for a folder.
    async fn get_message_delta_page(
        &self,
        folder_id: &str,
        checkpoint: Option<&str>,
    ) -> Result<DeltaPage<RemoteMessage>, GraphError>;

    /// Stream a message's MIME representation to a new destination file.
    async fn download_message(
        &self,
        message_id: &str,
        destination: &std::path::Path,
    ) -> Result<(), GraphError>;

    /// Update supported flags for one immutable message.
    async fn update_message_flags(
        &self,
        message_id: &str,
        flags: MessageFlags,
    ) -> Result<(), GraphError>;

    /// Resolve the immutable ID of the well-known Deleted Items folder.
    async fn get_deleted_items_folder_id(&self) -> Result<String, GraphError>;

    /// Move one immutable message to an immutable destination folder.
    async fn move_message(
        &self,
        message_id: &str,
        destination_folder_id: &str,
    ) -> Result<(), GraphError>;

    /// Permanently delete one immutable message.
    async fn delete_message(&self, message_id: &str) -> Result<(), GraphError>;
}

#[async_trait]
impl<P, S> GraphApi for GraphTransport<P, S>
where
    P: AccessTokenProvider,
    S: Sleeper,
{
    async fn get_folder_delta_page(
        &self,
        checkpoint: Option<&str>,
    ) -> Result<DeltaPage<RemoteFolderMetadata>, GraphError> {
        Self::get_folder_delta_page(self, checkpoint).await
    }

    async fn get_message_delta_page(
        &self,
        folder_id: &str,
        checkpoint: Option<&str>,
    ) -> Result<DeltaPage<RemoteMessage>, GraphError> {
        Self::get_message_delta_page(self, folder_id, checkpoint).await
    }

    async fn download_message(
        &self,
        message_id: &str,
        destination: &std::path::Path,
    ) -> Result<(), GraphError> {
        Self::download_message(self, message_id, destination).await
    }

    async fn update_message_flags(
        &self,
        message_id: &str,
        flags: MessageFlags,
    ) -> Result<(), GraphError> {
        Self::update_message_flags(self, message_id, flags).await
    }

    async fn get_deleted_items_folder_id(&self) -> Result<String, GraphError> {
        Self::get_deleted_items_folder_id(self).await
    }

    async fn move_message(
        &self,
        message_id: &str,
        destination_folder_id: &str,
    ) -> Result<(), GraphError> {
        Self::move_message(self, message_id, destination_folder_id).await
    }

    async fn delete_message(&self, message_id: &str) -> Result<(), GraphError> {
        Self::delete_message(self, message_id).await
    }
}

/// Safe Graph failures that exclude tokens and response message content.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum GraphError {
    /// A URL did not target the fixed Microsoft Graph v1.0 origin.
    #[error("Microsoft Graph returned an unexpected URL")]
    UnexpectedUrl,
    /// The bounded transient-failure retry budget was exhausted.
    #[error("Microsoft Graph retry budget was exhausted")]
    RetryExhausted,
    /// Access-token acquisition failed.
    #[error(transparent)]
    Authentication(#[from] AuthError),
    /// The hardened HTTP client could not be constructed.
    #[error("could not construct the Microsoft Graph HTTP client")]
    HttpClient,
    /// The request failed before a Graph response was received.
    #[error("Microsoft Graph request failed")]
    Request,
    /// A configured Graph request timeout elapsed.
    #[error("Microsoft Graph request timed out")]
    Timeout,
    /// Graph returned a response body that did not match the expected schema.
    #[error("Microsoft Graph returned malformed JSON")]
    MalformedJson,
    /// A delta response did not contain exactly one safe continuation link.
    #[error("Microsoft Graph returned invalid delta continuation links")]
    MalformedDeltaLinks,
    /// A mail-folder delta resource omitted required folder metadata.
    #[error("Microsoft Graph returned incomplete mail-folder metadata")]
    MalformedFolder,
    /// A message delta resource omitted required synchronization metadata.
    #[error("Microsoft Graph returned incomplete message metadata")]
    MalformedMessage,
    /// A message delta resource omitted its read/unread state.
    #[error("Microsoft Graph omitted a message's read state")]
    MissingMessageReadState,
    /// A message delta resource omitted its modification timestamp.
    #[error("Microsoft Graph omitted a message's modification timestamp")]
    MissingMessageModificationTime,
    /// A message delta resource omitted its follow-up flag state.
    #[error("Microsoft Graph omitted a message's follow-up flag state")]
    MissingMessageFlagState,
    /// A MIME destination could not be created, written, or synchronized.
    #[error("could not write the Microsoft Graph response to its destination file")]
    OutputFile,
    /// A prepared outbound MIME file could not be opened.
    #[error("could not read the prepared message payload")]
    InputFile,
    /// Graph returned a successful status other than the required acceptance.
    #[error("Microsoft Graph returned unexpected send status HTTP {0}")]
    UnexpectedSendStatus(u16),
    /// The connection failed after submission began, so acceptance is unknown.
    #[error("message submission result is unknown; retrying may send a duplicate")]
    SubmissionUnknown,
    /// Graph rejected a request permanently.
    #[error("Microsoft Graph request failed with HTTP {status}")]
    Response {
        /// HTTP response status.
        status: u16,
        /// Safe Graph error code, when present.
        code: Option<String>,
        /// Graph request identifier, when present.
        request_id: Option<String>,
    },
}

fn validate_graph_url(request_url: &str) -> Result<(), GraphError> {
    let parsed = Url::parse(request_url).map_err(|_| GraphError::UnexpectedUrl)?;
    let valid = parsed.scheme() == "https"
        && parsed.host_str() == Some("graph.microsoft.com")
        && parsed.port().is_none()
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.fragment().is_none()
        && (parsed.path() == "/v1.0" || parsed.path().starts_with("/v1.0/"));
    if valid {
        Ok(())
    } else {
        Err(GraphError::UnexpectedUrl)
    }
}

fn get_initial_folder_delta_url() -> Result<GraphUrl, GraphError> {
    GraphUrl::build(
        "/me/mailFolders/delta?$select=id,parentFolderId,displayName,isHidden,totalItemCount",
    )
}

fn get_initial_message_delta_url(folder_id: &str) -> Result<GraphUrl, GraphError> {
    if folder_id.is_empty() {
        return Err(GraphError::MalformedJson);
    }
    let encoded_id = utf8_percent_encode(folder_id, NON_ALPHANUMERIC);
    GraphUrl::build(&format!(
        "/me/mailFolders/{encoded_id}/messages/delta?$select=id,parentFolderId,internetMessageId,lastModifiedDateTime,isRead,flag"
    ))
}

fn get_message_metadata_url(
    delta_url: &GraphUrl,
    message_id: &str,
) -> Result<GraphUrl, GraphError> {
    let mut url = Url::parse(delta_url.as_str()).map_err(|_| GraphError::UnexpectedUrl)?;
    url.set_path("/v1.0/me/messages");
    url.path_segments_mut()
        .map_err(|_| GraphError::UnexpectedUrl)?
        .push(message_id);
    url.set_query(Some(
        "$select=id,parentFolderId,internetMessageId,lastModifiedDateTime,isRead,flag",
    ));
    Ok(GraphUrl(url.into()))
}

fn is_retryable_status(status: u16) -> bool {
    matches!(status, 408 | 429 | 500 | 502 | 503 | 504)
}

fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    httpdate::parse_http_date(value)
        .ok()
        .map(|retry_at| retry_at.duration_since(now).unwrap_or(Duration::ZERO))
}

fn get_exponential_delay(retry_number: u32) -> Duration {
    let seconds = 1_u64.checked_shl(retry_number.min(8)).unwrap_or(256);
    Duration::from_secs(seconds)
}

#[derive(Deserialize)]
struct GraphErrorEnvelope {
    error: GraphErrorBody,
}

#[derive(Deserialize)]
struct GraphErrorBody {
    code: String,
}

#[derive(Deserialize)]
struct DeltaEnvelope<T> {
    value: Vec<T>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
    #[serde(rename = "@odata.deltaLink")]
    delta_link: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FolderDeltaResource {
    id: String,
    parent_folder_id: Option<String>,
    display_name: Option<String>,
    #[serde(default)]
    is_hidden: bool,
    total_item_count: Option<i64>,
    #[serde(rename = "@removed")]
    removed: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MessageDeltaResource {
    id: String,
    parent_folder_id: Option<String>,
    internet_message_id: Option<String>,
    last_modified_date_time: Option<String>,
    is_read: Option<bool>,
    flag: Option<MessageFlagResource>,
    #[serde(rename = "@removed")]
    removed: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MessageFlagResource {
    flag_status: String,
}

#[derive(Deserialize)]
struct FolderIdResource {
    id: String,
}

fn build_delta_links(
    next_link: Option<String>,
    delta_link: Option<String>,
) -> Result<(Option<String>, Option<String>), GraphError> {
    let link = match (&next_link, &delta_link) {
        (Some(link), None) | (None, Some(link)) => link,
        _ => return Err(GraphError::MalformedDeltaLinks),
    };
    GraphUrl::build(link)?;
    Ok((next_link, delta_link))
}

fn build_folder_change(
    resource: FolderDeltaResource,
) -> Result<DeltaChange<RemoteFolderMetadata>, GraphError> {
    validate_resource_id(&resource.id).map_err(|_| GraphError::MalformedFolder)?;
    if resource.removed.is_some() {
        return Ok(DeltaChange::Delete { id: resource.id });
    }
    let display_name = resource
        .display_name
        .filter(|name| !name.is_empty())
        .ok_or(GraphError::MalformedFolder)?;
    let total_item_count = resource
        .total_item_count
        .and_then(|count| u32::try_from(count).ok())
        .ok_or(GraphError::MalformedFolder)?;
    Ok(DeltaChange::Upsert(RemoteFolderMetadata {
        id: resource.id,
        parent_id: resource.parent_folder_id,
        display_name,
        is_hidden: resource.is_hidden,
        total_item_count,
    }))
}

fn build_message_change(
    resource: MessageDeltaResource,
    requested_folder_id: &str,
) -> Result<DeltaChange<RemoteMessage>, GraphError> {
    validate_resource_id(&resource.id).map_err(|_| GraphError::MalformedMessage)?;
    if resource.removed.is_some() {
        return Ok(DeltaChange::Delete { id: resource.id });
    }
    let folder_id = resource
        .parent_folder_id
        .unwrap_or_else(|| requested_folder_id.to_owned());
    validate_resource_id(&folder_id).map_err(|_| GraphError::MalformedMessage)?;
    let is_read = resource
        .is_read
        .ok_or(GraphError::MissingMessageReadState)?;
    let remote_version = resource
        .last_modified_date_time
        .filter(|version| !version.is_empty())
        .ok_or(GraphError::MissingMessageModificationTime)?;
    let flag_status = resource
        .flag
        .ok_or(GraphError::MissingMessageFlagState)?
        .flag_status;
    let follow_up = match flag_status.as_str() {
        "notFlagged" => FollowUpState::NotFlagged,
        "flagged" | "complete" => FollowUpState::Flagged,
        _ => return Err(GraphError::MalformedMessage),
    };
    Ok(DeltaChange::Upsert(RemoteMessage {
        id: resource.id,
        folder_id,
        internet_message_id: resource.internet_message_id,
        remote_version,
        flags: MessageFlags { is_read, follow_up },
    }))
}

fn is_incomplete_message_error(error: &GraphError) -> bool {
    matches!(
        error,
        GraphError::MissingMessageReadState
            | GraphError::MissingMessageModificationTime
            | GraphError::MissingMessageFlagState
    )
}

fn validate_resource_id(id: &str) -> Result<(), GraphError> {
    if id.is_empty() || id.len() > 4096 {
        Err(GraphError::MalformedJson)
    } else {
        Ok(())
    }
}

fn build_http_client() -> Result<reqwest::Client, GraphError> {
    build_http_client_with_timeout(DEFAULT_REQUEST_TIMEOUT)
}

fn build_http_client_with_timeout(
    request_timeout: Duration,
) -> Result<reqwest::Client, GraphError> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(request_timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| GraphError::HttpClient)
}

fn is_retryable_request_error(error: &reqwest::Error) -> bool {
    !error.is_builder()
}

fn classify_request_error(error: reqwest::Error) -> GraphError {
    if error.is_timeout() {
        GraphError::Timeout
    } else {
        GraphError::Request
    }
}

fn build_response_error(status: u16, request_id: Option<String>, body: &[u8]) -> GraphError {
    let code = serde_json::from_slice::<GraphErrorEnvelope>(body)
        .ok()
        .and_then(|envelope| get_safe_diagnostic_value(&envelope.error.code));
    GraphError::Response {
        status,
        code,
        request_id,
    }
}

fn get_safe_diagnostic_value(value: &str) -> Option<String> {
    let valid = !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '-'));
    valid.then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{
        GraphError, GraphTransport, GraphUrl, RetryPolicy, Sleeper, TokioSleeper,
        get_initial_folder_delta_url, get_initial_message_delta_url,
    };
    use crate::auth::{AccessTokenProvider, AuthError};
    use crate::model::{
        DeltaChange, DeltaPage, FollowUpState, MessageFlags, RemoteFolderMetadata, RemoteMessage,
    };
    use async_trait::async_trait;
    use secrecy::SecretString;
    use serde::Deserialize;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::task::JoinHandle;
    use wiremock::matchers::{body_json, body_string, header, header_regex, method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    #[derive(Default)]
    struct FakeTokenProvider {
        forced_refreshes: Mutex<Vec<bool>>,
    }

    #[async_trait]
    impl AccessTokenProvider for FakeTokenProvider {
        async fn get_access_token(&self, force_refresh: bool) -> Result<SecretString, AuthError> {
            self.forced_refreshes
                .lock()
                .map_err(|_| AuthError::TokenExchange)?
                .push(force_refresh);
            let token = if force_refresh {
                "refreshed-access-token"
            } else {
                "initial-access-token"
            };
            Ok(token.into())
        }
    }

    #[derive(Default)]
    struct RecordingSleeper {
        delays: Mutex<Vec<Duration>>,
    }

    #[async_trait]
    impl Sleeper for RecordingSleeper {
        async fn sleep(&self, duration: Duration) {
            if let Ok(mut delays) = self.delays.lock() {
                delays.push(duration);
            }
        }
    }

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    struct TestProfile {
        id: String,
    }

    struct FirstResponseThenSuccess {
        calls: AtomicUsize,
        first_status: u16,
        retry_after: Option<&'static str>,
    }

    struct FirstDelayedResponseThenSuccess {
        calls: AtomicUsize,
        delay: Duration,
    }

    struct FirstResponseThenAccepted {
        calls: AtomicUsize,
        first_status: u16,
        retry_after: Option<&'static str>,
    }

    impl Respond for FirstDelayedResponseThenSuccess {
        fn respond(&self, _request: &Request) -> ResponseTemplate {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(200)
                    .set_delay(self.delay)
                    .set_body_json(serde_json::json!({"id": "late-user-id"}))
            } else {
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "retried-user-id"}))
            }
        }
    }

    impl Respond for FirstResponseThenAccepted {
        fn respond(&self, _request: &Request) -> ResponseTemplate {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                let response = ResponseTemplate::new(self.first_status);
                if let Some(retry_after) = self.retry_after {
                    response.insert_header("Retry-After", retry_after)
                } else {
                    response
                }
            } else {
                ResponseTemplate::new(202)
            }
        }
    }

    struct FlakyTokenProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl AccessTokenProvider for FlakyTokenProvider {
        async fn get_access_token(&self, _force_refresh: bool) -> Result<SecretString, AuthError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(AuthError::TokenRequest)
            } else {
                Ok("retried-access-token".into())
            }
        }
    }

    impl Respond for FirstResponseThenSuccess {
        fn respond(&self, _request: &Request) -> ResponseTemplate {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                let response = ResponseTemplate::new(self.first_status);
                if let Some(retry_after) = self.retry_after {
                    response.insert_header("Retry-After", retry_after)
                } else {
                    response
                }
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "user-id"}))
            }
        }
    }

    fn build_test_transport(
        tokens: Arc<FakeTokenProvider>,
        sleeper: Arc<RecordingSleeper>,
        policy: RetryPolicy,
    ) -> GraphTransport<FakeTokenProvider, RecordingSleeper> {
        GraphTransport::build_for_test(tokens, sleeper, policy)
            .expect("test transport should be created")
    }

    fn build_test_url(server: &MockServer, path: &str) -> GraphUrl {
        GraphUrl(format!("{}{}", server.uri(), path))
    }

    async fn start_truncated_then_complete_server(
        content_type: &'static str,
        complete_body: &'static [u8],
    ) -> (GraphUrl, JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener should bind");
        let address = listener
            .local_addr()
            .expect("test listener address should load");
        let task = tokio::spawn(async move {
            for truncated in [true, false] {
                let (mut stream, _) = listener.accept().await.expect("request should connect");
                let mut request = [0_u8; 4096];
                let _read = stream
                    .read(&mut request)
                    .await
                    .expect("request should be readable");
                let body = if truncated {
                    &complete_body[..complete_body.len().min(2)]
                } else {
                    complete_body
                };
                let declared_length = if truncated {
                    complete_body.len() + 100
                } else {
                    complete_body.len()
                };
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {declared_length}\r\nConnection: close\r\n\r\n"
                );
                stream
                    .write_all(headers.as_bytes())
                    .await
                    .expect("response headers should write");
                stream
                    .write_all(body)
                    .await
                    .expect("response body should write");
                stream.shutdown().await.expect("response should close");
            }
        });
        (
            GraphUrl(format!("http://{address}/v1.0/flaky-response")),
            task,
        )
    }

    #[test]
    fn configures_mime_fsync_for_one_transport() {
        let durable = GraphTransport::build(
            Arc::new(FakeTokenProvider::default()),
            Arc::new(RecordingSleeper::default()),
        )
        .expect("durable transport should build");
        let buffered = GraphTransport::build_with_fsync(
            Arc::new(FakeTokenProvider::default()),
            Arc::new(RecordingSleeper::default()),
            false,
        )
        .expect("buffered transport should build");

        assert!(durable.fsync_enabled);
        assert!(!buffered.fsync_enabled);
    }

    #[tokio::test]
    async fn adds_bearer_and_immutable_id_headers() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me"))
            .and(header("Authorization", "Bearer initial-access-token"))
            .and(header("Prefer", "IdType=\"ImmutableId\""))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "user-id"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let tokens = Arc::new(FakeTokenProvider::default());
        let sleeper = Arc::new(RecordingSleeper::default());
        let transport = GraphTransport::build(Arc::clone(&tokens), sleeper)
            .expect("production transport should be created");

        let profile: TestProfile = transport
            .get_json(&build_test_url(&server, "/v1.0/me"))
            .await
            .expect("Graph response should decode");

        assert_eq!(
            profile,
            TestProfile {
                id: "user-id".into()
            }
        );
        assert_eq!(
            *tokens
                .forced_refreshes
                .lock()
                .expect("token calls should be readable"),
            [false]
        );
    }

    #[tokio::test]
    async fn patches_supported_message_flags_with_replayable_json() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/v1.0/me/messages/message-id"))
            .and(header("Authorization", "Bearer initial-access-token"))
            .and(header("Prefer", "IdType=\"ImmutableId\""))
            .and(body_json(serde_json::json!({
                "isRead": true,
                "flag": {"flagStatus": "flagged"}
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let transport = build_test_transport(
            Arc::new(FakeTokenProvider::default()),
            Arc::new(RecordingSleeper::default()),
            RetryPolicy::default(),
        );

        transport
            .update_message_flags_at(
                &build_test_url(&server, "/v1.0/me/messages/message-id"),
                MessageFlags {
                    is_read: true,
                    follow_up: FollowUpState::Flagged,
                },
            )
            .await
            .expect("supported flags should patch");
    }

    #[tokio::test]
    async fn moves_and_deletes_messages_with_immutable_ids() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1.0/me/messages/message-id/move"))
            .and(header("Prefer", "IdType=\"ImmutableId\""))
            .and(body_json(
                serde_json::json!({"destinationId": "archive-id"}),
            ))
            .respond_with(ResponseTemplate::new(201))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/v1.0/me/messages/deleted-id"))
            .and(header("Prefer", "IdType=\"ImmutableId\""))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/v1.0/me/messages/already-deleted-id"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
        let transport = build_test_transport(
            Arc::new(FakeTokenProvider::default()),
            Arc::new(RecordingSleeper::default()),
            RetryPolicy::default(),
        );

        transport
            .move_message_at(
                &build_test_url(&server, "/v1.0/me/messages/message-id/move"),
                "archive-id",
            )
            .await
            .expect("message should move");
        transport
            .delete_message_at(&build_test_url(&server, "/v1.0/me/messages/deleted-id"))
            .await
            .expect("message should delete");
        transport
            .delete_message_at(&build_test_url(
                &server,
                "/v1.0/me/messages/already-deleted-id",
            ))
            .await
            .expect("a replayed delete should be idempotent");
    }

    #[tokio::test]
    async fn resolves_the_well_known_deleted_items_folder_id() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/mailFolders/deleteditems"))
            .and(header("Prefer", "IdType=\"ImmutableId\""))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "deleted-items-id"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let transport = build_test_transport(
            Arc::new(FakeTokenProvider::default()),
            Arc::new(RecordingSleeper::default()),
            RetryPolicy::default(),
        );

        assert_eq!(
            transport
                .get_deleted_items_folder_id_from(&build_test_url(
                    &server,
                    "/v1.0/me/mailFolders/deleteditems?$select=id",
                ))
                .await
                .expect("well-known folder should resolve"),
            "deleted-items-id"
        );
    }

    #[tokio::test]
    async fn refreshes_once_and_replays_after_unauthorized() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me"))
            .respond_with(FirstResponseThenSuccess {
                calls: AtomicUsize::new(0),
                first_status: 401,
                retry_after: None,
            })
            .expect(2)
            .mount(&server)
            .await;
        let tokens = Arc::new(FakeTokenProvider::default());
        let transport = build_test_transport(
            Arc::clone(&tokens),
            Arc::new(RecordingSleeper::default()),
            RetryPolicy::default(),
        );

        let profile: TestProfile = transport
            .get_json(&build_test_url(&server, "/v1.0/me"))
            .await
            .expect("401 should be replayed after one refresh");

        assert_eq!(profile.id, "user-id");
        assert_eq!(
            *tokens
                .forced_refreshes
                .lock()
                .expect("token calls should be readable"),
            [false, true]
        );
    }

    #[tokio::test]
    async fn honors_retry_after_before_replaying_transient_failures() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me"))
            .respond_with(FirstResponseThenSuccess {
                calls: AtomicUsize::new(0),
                first_status: 429,
                retry_after: Some("2"),
            })
            .expect(2)
            .mount(&server)
            .await;
        let sleeper = Arc::new(RecordingSleeper::default());
        let transport = build_test_transport(
            Arc::new(FakeTokenProvider::default()),
            Arc::clone(&sleeper),
            RetryPolicy::default(),
        );

        let _: TestProfile = transport
            .get_json(&build_test_url(&server, "/v1.0/me"))
            .await
            .expect("429 should be retried");

        assert_eq!(
            *sleeper.delays.lock().expect("delays should be readable"),
            [Duration::from_secs(2)]
        );
    }

    #[tokio::test]
    async fn streams_base64_mime_to_sendmail_and_requires_accepted() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1.0/me/sendMail"))
            .and(header("Content-Type", "text/plain"))
            .and(body_string(
                "RnJvbTogc2VuZGVyQGV4YW1wbGUuY29tDQoNCkJvZHkNCg==",
            ))
            .respond_with(ResponseTemplate::new(202))
            .expect(1)
            .mount(&server)
            .await;
        let transport = build_test_transport(
            Arc::new(FakeTokenProvider::default()),
            Arc::new(RecordingSleeper::default()),
            RetryPolicy::default(),
        );
        let mut payload =
            tempfile::NamedTempFile::new().expect("temporary encoded message should be created");
        std::io::Write::write_all(
            &mut payload,
            b"RnJvbTogc2VuZGVyQGV4YW1wbGUuY29tDQoNCkJvZHkNCg==",
        )
        .expect("encoded message should be written");

        transport
            .send_mime_file_to(
                &build_test_url(&server, "/v1.0/me/sendMail"),
                payload.path(),
            )
            .await
            .expect("202 should accept a streamed MIME message");

        Mock::given(method("POST"))
            .and(path("/v1.0/me/unexpected"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            transport
                .send_mime_file_to(
                    &build_test_url(&server, "/v1.0/me/unexpected"),
                    payload.path(),
                )
                .await,
            Err(GraphError::UnexpectedSendStatus(200))
        );
    }

    #[tokio::test]
    async fn refreshes_sendmail_authentication_and_returns_safe_rejections() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1.0/me/sendMail"))
            .respond_with(FirstResponseThenAccepted {
                calls: AtomicUsize::new(0),
                first_status: 401,
                retry_after: None,
            })
            .expect(2)
            .mount(&server)
            .await;
        let tokens = Arc::new(FakeTokenProvider::default());
        let transport = build_test_transport(
            Arc::clone(&tokens),
            Arc::new(RecordingSleeper::default()),
            RetryPolicy::default(),
        );
        let mut payload =
            tempfile::NamedTempFile::new().expect("temporary encoded message should be created");
        std::io::Write::write_all(&mut payload, b"cGF5bG9hZA==")
            .expect("encoded message should be written");

        transport
            .send_mime_file_to(
                &build_test_url(&server, "/v1.0/me/sendMail"),
                payload.path(),
            )
            .await
            .expect("401 should refresh once before accepted submission");
        assert_eq!(
            *tokens
                .forced_refreshes
                .lock()
                .expect("token calls should be readable"),
            [false, true]
        );

        Mock::given(method("POST"))
            .and(path("/v1.0/me/forbidden"))
            .respond_with(
                ResponseTemplate::new(403)
                    .insert_header("request-id", "send-request-123")
                    .set_body_json(serde_json::json!({
                        "error": {
                            "code": "ErrorAccessDenied",
                            "message": "message-content-must-not-leak"
                        }
                    })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let error = transport
            .send_mime_file_to(
                &build_test_url(&server, "/v1.0/me/forbidden"),
                payload.path(),
            )
            .await
            .expect_err("403 should be returned without a replay");

        assert_eq!(
            error,
            GraphError::Response {
                status: 403,
                code: Some("ErrorAccessDenied".into()),
                request_id: Some("send-request-123".into()),
            }
        );
        assert!(!format!("{error:?}").contains("message-content-must-not-leak"));
    }

    #[tokio::test]
    async fn retries_rejected_sendmail_responses_but_not_ambiguous_timeouts() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1.0/me/sendMail"))
            .respond_with(FirstResponseThenAccepted {
                calls: AtomicUsize::new(0),
                first_status: 429,
                retry_after: Some("2"),
            })
            .expect(2)
            .mount(&server)
            .await;
        let sleeper = Arc::new(RecordingSleeper::default());
        let transport = build_test_transport(
            Arc::new(FakeTokenProvider::default()),
            Arc::clone(&sleeper),
            RetryPolicy::default(),
        );
        let mut payload =
            tempfile::NamedTempFile::new().expect("temporary encoded message should be created");
        std::io::Write::write_all(&mut payload, b"cGF5bG9hZA==")
            .expect("encoded message should be written");

        transport
            .send_mime_file_to(
                &build_test_url(&server, "/v1.0/me/sendMail"),
                payload.path(),
            )
            .await
            .expect("an explicit 429 rejection should be retried");
        assert_eq!(
            *sleeper.delays.lock().expect("delays should be readable"),
            [Duration::from_secs(2)]
        );

        Mock::given(method("POST"))
            .and(path("/v1.0/me/token-retry"))
            .respond_with(ResponseTemplate::new(202))
            .expect(1)
            .mount(&server)
            .await;
        let token_provider = Arc::new(FlakyTokenProvider {
            calls: AtomicUsize::new(0),
        });
        let token_sleeper = Arc::new(RecordingSleeper::default());
        let token_transport = GraphTransport::build_for_test(
            Arc::clone(&token_provider),
            Arc::clone(&token_sleeper),
            RetryPolicy::default(),
        )
        .expect("token retry transport should build");
        token_transport
            .send_mime_file_to(
                &build_test_url(&server, "/v1.0/me/token-retry"),
                payload.path(),
            )
            .await
            .expect("a transient token request failure should retry before submission");
        assert_eq!(token_provider.calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            *token_sleeper
                .delays
                .lock()
                .expect("token delays should be readable"),
            [Duration::from_secs(1)]
        );

        let timeout_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1.0/me/sendMail"))
            .respond_with(ResponseTemplate::new(202).set_delay(Duration::from_millis(50)))
            .expect(1)
            .mount(&timeout_server)
            .await;
        let timeout_transport = GraphTransport::build_for_test_with_timeout(
            Arc::new(FakeTokenProvider::default()),
            Arc::new(RecordingSleeper::default()),
            RetryPolicy::default(),
            Duration::from_millis(10),
        )
        .expect("timeout transport should build");

        assert_eq!(
            timeout_transport
                .send_mime_file_to(
                    &build_test_url(&timeout_server, "/v1.0/me/sendMail"),
                    payload.path(),
                )
                .await,
            Err(GraphError::SubmissionUnknown)
        );
    }

    #[tokio::test]
    async fn retries_request_timeouts_and_transient_token_refresh_failures() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/timeout"))
            .respond_with(FirstDelayedResponseThenSuccess {
                calls: AtomicUsize::new(0),
                delay: Duration::from_millis(50),
            })
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1.0/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "token-user-id"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let timeout_sleeper = Arc::new(RecordingSleeper::default());
        let timeout_transport = GraphTransport::build_for_test_with_timeout(
            Arc::new(FakeTokenProvider::default()),
            Arc::clone(&timeout_sleeper),
            RetryPolicy::default(),
            Duration::from_millis(10),
        )
        .expect("timeout transport should build");
        let token_sleeper = Arc::new(RecordingSleeper::default());
        let token_provider = Arc::new(FlakyTokenProvider {
            calls: AtomicUsize::new(0),
        });
        let token_transport = GraphTransport::build_for_test(
            Arc::clone(&token_provider),
            Arc::clone(&token_sleeper),
            RetryPolicy::default(),
        )
        .expect("token transport should build");

        let timeout_profile: TestProfile = timeout_transport
            .get_json(&build_test_url(&server, "/v1.0/timeout"))
            .await
            .expect("timed-out request should retry");
        let token_profile: TestProfile = token_transport
            .get_json(&build_test_url(&server, "/v1.0/token"))
            .await
            .expect("failed token refresh should retry");

        assert_eq!(timeout_profile.id, "retried-user-id");
        assert_eq!(token_profile.id, "token-user-id");
        assert_eq!(token_provider.calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            *timeout_sleeper
                .delays
                .lock()
                .expect("timeout delays should load"),
            [Duration::from_secs(1)]
        );
        assert_eq!(
            *token_sleeper
                .delays
                .lock()
                .expect("token delays should load"),
            [Duration::from_secs(1)]
        );
    }

    #[tokio::test]
    async fn retries_truncated_json_and_mime_response_bodies() {
        let (json_url, json_server) =
            start_truncated_then_complete_server("application/json", br#"{"id":"retried-user"}"#)
                .await;
        let json_sleeper = Arc::new(RecordingSleeper::default());
        let json_transport = build_test_transport(
            Arc::new(FakeTokenProvider::default()),
            Arc::clone(&json_sleeper),
            RetryPolicy::default(),
        );

        let profile: TestProfile = json_transport
            .get_json(&json_url)
            .await
            .expect("truncated JSON body should retry");
        json_server.await.expect("JSON server should complete");

        let mime = b"Subject: Retried\r\n\r\nComplete body\r\n";
        let (mime_url, mime_server) =
            start_truncated_then_complete_server("message/rfc822", mime).await;
        let mime_sleeper = Arc::new(RecordingSleeper::default());
        let mime_transport = build_test_transport(
            Arc::new(FakeTokenProvider::default()),
            Arc::clone(&mime_sleeper),
            RetryPolicy::default(),
        );
        let temp = tempfile::TempDir::new().expect("temporary directory should be created");
        let destination = temp.path().join("message.eml");

        mime_transport
            .download_to(&mime_url, &destination)
            .await
            .expect("truncated MIME body should retry");
        mime_server.await.expect("MIME server should complete");

        assert_eq!(profile.id, "retried-user");
        assert_eq!(
            std::fs::read(destination).expect("MIME should be readable"),
            mime
        );
        assert_eq!(
            *json_sleeper.delays.lock().expect("JSON delays should load"),
            [Duration::from_secs(1)]
        );
        assert_eq!(
            *mime_sleeper.delays.lock().expect("MIME delays should load"),
            [Duration::from_secs(1)]
        );
    }

    #[tokio::test]
    async fn returns_safe_graph_codes_and_request_ids() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me"))
            .respond_with(
                ResponseTemplate::new(403)
                    .insert_header("request-id", "request-123")
                    .set_body_json(serde_json::json!({
                        "error": {
                            "code": "ErrorAccessDenied",
                            "message": "response-body-secret"
                        }
                    })),
            )
            .mount(&server)
            .await;
        let transport = build_test_transport(
            Arc::new(FakeTokenProvider::default()),
            Arc::new(RecordingSleeper::default()),
            RetryPolicy::default(),
        );

        let error = transport
            .get_json::<TestProfile>(&build_test_url(&server, "/v1.0/me"))
            .await
            .expect_err("403 should be permanent");

        assert_eq!(
            error,
            GraphError::Response {
                status: 403,
                code: Some("ErrorAccessDenied".into()),
                request_id: Some("request-123".into()),
            }
        );
        assert!(!format!("{error:?}").contains("response-body-secret"));
    }

    #[tokio::test]
    async fn classifies_malformed_success_json() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not-json"))
            .mount(&server)
            .await;
        let transport = build_test_transport(
            Arc::new(FakeTokenProvider::default()),
            Arc::new(RecordingSleeper::default()),
            RetryPolicy::default(),
        );

        assert_eq!(
            transport
                .get_json::<TestProfile>(&build_test_url(&server, "/v1.0/me"))
                .await,
            Err(GraphError::MalformedJson)
        );
    }

    #[tokio::test]
    async fn stops_replaying_when_attempts_are_exhausted() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me"))
            .respond_with(ResponseTemplate::new(503))
            .expect(2)
            .mount(&server)
            .await;
        let policy = RetryPolicy {
            max_attempts: 1,
            max_total_delay: Duration::from_secs(300),
        };
        let transport = build_test_transport(
            Arc::new(FakeTokenProvider::default()),
            Arc::new(RecordingSleeper::default()),
            policy,
        );

        assert_eq!(
            transport
                .get_json::<TestProfile>(&build_test_url(&server, "/v1.0/me"))
                .await,
            Err(GraphError::RetryExhausted)
        );
    }

    #[tokio::test]
    async fn streams_mime_responses_to_a_new_file_without_overwriting() {
        let server = MockServer::start().await;
        let mime = b"From: sender@example.com\r\nSubject: Test\r\n\r\nBody\r\n";
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages/id/$value"))
            .and(header("Accept", "message/rfc822"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(mime.as_slice()))
            .expect(1)
            .mount(&server)
            .await;
        let transport = build_test_transport(
            Arc::new(FakeTokenProvider::default()),
            Arc::new(RecordingSleeper::default()),
            RetryPolicy::default(),
        );
        let temp = tempfile::TempDir::new().expect("temporary directory should be created");
        let destination = temp.path().join("message.eml");

        transport
            .download_to(
                &build_test_url(&server, "/v1.0/me/messages/id/$value"),
                &destination,
            )
            .await
            .expect("MIME response should stream to disk");

        assert_eq!(
            std::fs::read(&destination).expect("download should be readable"),
            mime
        );
        assert!(matches!(
            transport
                .download_to(
                    &build_test_url(&server, "/v1.0/me/messages/id/$value"),
                    &destination,
                )
                .await,
            Err(GraphError::OutputFile)
        ));
    }

    #[tokio::test]
    async fn removes_a_partial_destination_when_graph_rejects_a_mime_request() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages/id/$value"))
            .respond_with(ResponseTemplate::new(403))
            .expect(1)
            .mount(&server)
            .await;
        let transport = build_test_transport(
            Arc::new(FakeTokenProvider::default()),
            Arc::new(RecordingSleeper::default()),
            RetryPolicy::default(),
        );
        let temp = tempfile::TempDir::new().expect("temporary directory should be created");
        let destination = temp.path().join("message.eml");

        let result = transport
            .download_to(
                &build_test_url(&server, "/v1.0/me/messages/id/$value"),
                &destination,
            )
            .await;

        assert!(matches!(
            result,
            Err(GraphError::Response { status: 403, .. })
        ));
        assert!(!destination.exists());
    }

    #[tokio::test]
    async fn classifies_connection_failures_without_exposing_client_details() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("ephemeral port should be reservable");
        let address = listener
            .local_addr()
            .expect("listener address should be available");
        drop(listener);
        let transport = GraphTransport::build(
            Arc::new(FakeTokenProvider::default()),
            Arc::new(RecordingSleeper::default()),
        )
        .expect("transport should be created");
        let url = GraphUrl(format!("http://{address}/v1.0/me"));

        assert_eq!(
            transport.get_json::<TestProfile>(&url).await,
            Err(GraphError::RetryExhausted)
        );
    }

    #[tokio::test]
    async fn production_sleeper_accepts_zero_delay() {
        Sleeper::sleep(&TokioSleeper, Duration::ZERO).await;
    }

    #[tokio::test]
    async fn decodes_folder_delta_upserts_deletions_and_final_checkpoint() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/mailFolders/delta"))
            .and(header_regex(
                "Prefer",
                "IdType=\"ImmutableId\".*odata\\.maxpagesize=1000",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "value": [
                    {
                        "id": "inbox-id",
                        "parentFolderId": "root-id",
                        "displayName": "Inbox",
                        "isHidden": false,
                        "totalItemCount": 1234
                    },
                    {
                        "id": "deleted-id",
                        "@removed": {"reason": "deleted"}
                    }
                ],
                "@odata.deltaLink": "https://graph.microsoft.com/v1.0/me/mailFolders/delta?$deltatoken=opaque%2Bvalue"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let transport = build_test_transport(
            Arc::new(FakeTokenProvider::default()),
            Arc::new(RecordingSleeper::default()),
            RetryPolicy::default(),
        );

        let page = transport
            .get_folder_delta_page_from(&build_test_url(&server, "/v1.0/me/mailFolders/delta"))
            .await
            .expect("folder delta should decode");

        assert_eq!(
            page.changes,
            [
                DeltaChange::Upsert(RemoteFolderMetadata {
                    id: "inbox-id".into(),
                    parent_id: Some("root-id".into()),
                    display_name: "Inbox".into(),
                    is_hidden: false,
                    total_item_count: 1234,
                }),
                DeltaChange::Delete {
                    id: "deleted-id".into()
                }
            ]
        );
        assert_eq!(page.next_link, None);
        assert_eq!(
            page.delta_link.as_deref(),
            Some(
                "https://graph.microsoft.com/v1.0/me/mailFolders/delta?$deltatoken=opaque%2Bvalue"
            )
        );
    }

    #[tokio::test]
    async fn decodes_message_delta_flags_and_deletions() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/messages/delta"))
            .and(header_regex(
                "Prefer",
                "IdType=\"ImmutableId\".*odata\\.maxpagesize=1000",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "value": [
                    {
                        "id": "message-id",
                        "parentFolderId": "inbox-id",
                        "internetMessageId": "<message@example.com>",
                        "lastModifiedDateTime": "2026-07-25T12:00:00Z",
                        "isRead": true,
                        "flag": {"flagStatus": "complete"}
                    },
                    {
                        "id": "deleted-message-id",
                        "@removed": {"reason": "deleted"}
                    }
                ],
                "@odata.nextLink": "https://graph.microsoft.com/v1.0/me/mailFolders/inbox/messages/delta?$skiptoken=opaque"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let transport = build_test_transport(
            Arc::new(FakeTokenProvider::default()),
            Arc::new(RecordingSleeper::default()),
            RetryPolicy::default(),
        );

        let page = transport
            .get_message_delta_page_from(
                "inbox-id",
                &build_test_url(&server, "/v1.0/messages/delta"),
            )
            .await
            .expect("message delta should decode");

        assert_eq!(
            page.changes,
            [
                DeltaChange::Upsert(RemoteMessage {
                    id: "message-id".into(),
                    folder_id: "inbox-id".into(),
                    internet_message_id: Some("<message@example.com>".into()),
                    remote_version: "2026-07-25T12:00:00Z".into(),
                    flags: MessageFlags {
                        is_read: true,
                        follow_up: FollowUpState::Flagged,
                    },
                }),
                DeltaChange::Delete {
                    id: "deleted-message-id".into()
                }
            ]
        );
        assert!(page.delta_link.is_none());
        assert!(page.next_link.is_some());
    }

    #[tokio::test]
    async fn rejects_delta_pages_without_exactly_one_continuation_link() {
        for body in [
            serde_json::json!({"value": []}),
            serde_json::json!({
                "value": [],
                "@odata.nextLink": "https://graph.microsoft.com/v1.0/me/mailFolders/delta?$skiptoken=x",
                "@odata.deltaLink": "https://graph.microsoft.com/v1.0/me/mailFolders/delta?$deltatoken=y"
            }),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/v1.0/delta"))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .expect(1)
                .mount(&server)
                .await;
            let transport = build_test_transport(
                Arc::new(FakeTokenProvider::default()),
                Arc::new(RecordingSleeper::default()),
                RetryPolicy::default(),
            );

            assert_eq!(
                transport
                    .get_folder_delta_page_from(&build_test_url(&server, "/v1.0/delta"))
                    .await,
                Err(GraphError::MalformedDeltaLinks)
            );
        }
    }

    #[tokio::test]
    async fn distinguishes_untrusted_delta_links_from_incomplete_folder_resources() {
        let cases = [
            (
                serde_json::json!({
                    "value": [],
                    "@odata.deltaLink": "https://evil.example/v1.0/me/mailFolders/delta?$deltatoken=y"
                }),
                GraphError::UnexpectedUrl,
            ),
            (
                serde_json::json!({
                    "value": [{"id": "folder-without-a-name"}],
                    "@odata.deltaLink": "https://graph.microsoft.com/v1.0/me/mailFolders/delta?$deltatoken=y"
                }),
                GraphError::MalformedFolder,
            ),
        ];
        for (body, expected) in cases {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/v1.0/delta"))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .expect(1)
                .mount(&server)
                .await;
            let transport = build_test_transport(
                Arc::new(FakeTokenProvider::default()),
                Arc::new(RecordingSleeper::default()),
                RetryPolicy::default(),
            );

            assert_eq!(
                transport
                    .get_folder_delta_page_from(&build_test_url(&server, "/v1.0/delta"))
                    .await,
                Err(expected)
            );
        }
    }

    #[tokio::test]
    async fn refetches_complete_metadata_for_partial_message_delta_entries() {
        let complete = serde_json::json!({
            "id": "message-id",
            "parentFolderId": "inbox-id",
            "lastModifiedDateTime": "2026-07-25T12:00:00Z",
            "isRead": false,
            "flag": {"flagStatus": "notFlagged"}
        });
        for missing in ["isRead", "lastModifiedDateTime", "flag"] {
            let mut message = complete.clone();
            message
                .as_object_mut()
                .expect("message fixture should be an object")
                .remove(missing);
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/v1.0/messages/delta"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "value": [message],
                    "@odata.deltaLink": "https://graph.microsoft.com/v1.0/me/mailFolders/inbox/messages/delta?$deltatoken=y"
                })))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/v1.0/me/messages/message-id"))
                .respond_with(ResponseTemplate::new(200).set_body_json(complete.clone()))
                .expect(1)
                .mount(&server)
                .await;
            let transport = build_test_transport(
                Arc::new(FakeTokenProvider::default()),
                Arc::new(RecordingSleeper::default()),
                RetryPolicy::default(),
            );

            assert_eq!(
                transport
                    .get_message_delta_page_from(
                        "inbox-id",
                        &build_test_url(&server, "/v1.0/messages/delta"),
                    )
                    .await,
                Ok(DeltaPage {
                    changes: vec![DeltaChange::Upsert(RemoteMessage {
                        id: "message-id".into(),
                        folder_id: "inbox-id".into(),
                        internet_message_id: None,
                        remote_version: "2026-07-25T12:00:00Z".into(),
                        flags: MessageFlags::default(),
                    })],
                    next_link: None,
                    delta_link: Some(
                        "https://graph.microsoft.com/v1.0/me/mailFolders/inbox/messages/delta?$deltatoken=y"
                            .into(),
                    ),
                })
            );
        }
    }

    #[tokio::test]
    async fn treats_an_unresolvable_partial_delta_entry_as_deleted() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/messages/delta"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "value": [{
                    "id": "missing-message-id",
                    "parentFolderId": "inbox-id",
                    "lastModifiedDateTime": "2026-07-25T12:00:00Z",
                    "flag": {"flagStatus": "notFlagged"}
                }],
                "@odata.deltaLink": "https://graph.microsoft.com/v1.0/me/mailFolders/inbox/messages/delta?$deltatoken=y"
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages/missing-message-id"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": {"code": "ErrorItemNotFound"}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let transport = build_test_transport(
            Arc::new(FakeTokenProvider::default()),
            Arc::new(RecordingSleeper::default()),
            RetryPolicy::default(),
        );

        let page = transport
            .get_message_delta_page_from(
                "inbox-id",
                &build_test_url(&server, "/v1.0/messages/delta"),
            )
            .await
            .expect("confirmed missing partial entry should decode");

        assert_eq!(
            page.changes,
            [DeltaChange::Delete {
                id: "missing-message-id".into()
            }]
        );
    }

    #[test]
    fn percent_encodes_folder_ids_in_message_delta_urls() {
        assert_eq!(
            get_initial_folder_delta_url()
                .expect("folder delta URL should build")
                .as_str(),
            "https://graph.microsoft.com/v1.0/me/mailFolders/delta?$select=id,parentFolderId,displayName,isHidden,totalItemCount"
        );
        assert_eq!(
            get_initial_message_delta_url("folder/id?secret")
                .expect("folder ID should encode")
                .as_str(),
            "https://graph.microsoft.com/v1.0/me/mailFolders/folder%2Fid%3Fsecret/messages/delta?$select=id,parentFolderId,internetMessageId,lastModifiedDateTime,isRead,flag"
        );
    }
}
