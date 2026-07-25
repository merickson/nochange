//! Account-login orchestration for the `nochange init` command.

use crate::auth::{
    AccessTokenProvider, AuthError, AuthorizationSession, BrowserLauncher, CredentialStore,
    EntraEndpoints, EntraTokenExchange, LocalCallbackListener, OAuthTokenExchange, TokenGrant,
};
use crate::config::AccountConfig;
use crate::graph::{GraphError, GraphTransport, GraphUrl, TokioSleeper};
use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use url::Url;

const CALLBACK_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Interactive authentication flow selected for an account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginMethod {
    /// Open a PKCE authorization request and receive its localhost callback.
    Browser,
    /// Display a Microsoft device-login URL and short user code.
    DeviceCode,
}

/// Validated identity returned by Microsoft Graph `/me`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailboxIdentity {
    /// Stable Microsoft Graph user identifier.
    pub id: String,
    /// Primary sign-in identity used to validate account configuration.
    pub user_principal_name: String,
}

/// Structured user-facing output required during interactive authentication.
pub trait LoginPrompter: Send + Sync {
    /// Show the authorization URL before attempting to open a browser.
    fn show_browser_login(&self, account: &str, url: &Url);

    /// Show the device authorization URL and its intentionally user-visible code.
    fn show_device_login(&self, account: &str, url: &Url, code: &SecretString);

    /// Report a successfully verified Microsoft 365 identity.
    fn show_authenticated(&self, account: &str, user: &str);
}

/// Refresh and interactive authorization operations used by initialization.
#[async_trait]
pub trait AccountAuthenticator: Send + Sync {
    /// Exchange an existing account refresh token.
    async fn refresh(
        &self,
        account: &AccountConfig,
        refresh_token: &SecretString,
    ) -> Result<TokenGrant, AuthError>;

    /// Run the selected interactive authorization flow.
    async fn authenticate(
        &self,
        account: &AccountConfig,
        method: LoginMethod,
        prompter: &dyn LoginPrompter,
    ) -> Result<TokenGrant, AuthError>;
}

/// Microsoft Graph identity lookup boundary.
#[async_trait]
pub trait ProfileVerifier: Send + Sync {
    /// Retrieve `/me` using a newly issued access token.
    async fn get_profile(&self, access_token: &SecretString)
    -> Result<MailboxIdentity, GraphError>;
}

/// Microsoft Entra implementation of refresh, PKCE, and device authorization.
pub struct EntraAccountAuthenticator<B> {
    browser: B,
    callback_timeout: Duration,
    endpoint_override: Option<EntraEndpoints>,
}

impl<B> EntraAccountAuthenticator<B> {
    /// Build a production authenticator with a ten-minute browser callback timeout.
    pub const fn new(browser: B) -> Self {
        Self {
            browser,
            callback_timeout: CALLBACK_TIMEOUT,
            endpoint_override: None,
        }
    }

    fn get_endpoints(&self, account: &AccountConfig) -> Result<EntraEndpoints, AuthError> {
        match self.endpoint_override.as_ref() {
            Some(endpoints) => Ok(endpoints.clone()),
            None => EntraEndpoints::build(&account.tenant),
        }
    }

    #[cfg(test)]
    const fn new_for_test(
        browser: B,
        callback_timeout: Duration,
        endpoints: EntraEndpoints,
    ) -> Self {
        Self {
            browser,
            callback_timeout,
            endpoint_override: Some(endpoints),
        }
    }
}

#[async_trait]
impl<B> AccountAuthenticator for EntraAccountAuthenticator<B>
where
    B: BrowserLauncher,
{
    async fn refresh(
        &self,
        account: &AccountConfig,
        refresh_token: &SecretString,
    ) -> Result<TokenGrant, AuthError> {
        let endpoints = self.get_endpoints(account)?;
        let exchange = EntraTokenExchange::build(&account.client_id, &endpoints)?;
        exchange.exchange_refresh_token(refresh_token).await
    }

    async fn authenticate(
        &self,
        account: &AccountConfig,
        method: LoginMethod,
        prompter: &dyn LoginPrompter,
    ) -> Result<TokenGrant, AuthError> {
        let endpoints = self.get_endpoints(account)?;
        let exchange = EntraTokenExchange::build(&account.client_id, &endpoints)?;
        match method {
            LoginMethod::Browser => {
                let listener = LocalCallbackListener::bind()?;
                let session = AuthorizationSession::build(
                    &account.client_id,
                    &endpoints,
                    listener.get_redirect_url().clone(),
                )?;
                prompter.show_browser_login(&account.name, session.get_authorization_url());
                session.open_with(&self.browser)?;
                let authorization_code = listener
                    .wait_for_code(&session, self.callback_timeout)
                    .await?;
                exchange
                    .exchange_authorization_code(&authorization_code, session)
                    .await
            }
            LoginMethod::DeviceCode => {
                let session = exchange.start_device_authorization().await?;
                prompter.show_device_login(
                    &account.name,
                    session.get_verification_uri(),
                    session.get_user_code(),
                );
                exchange.poll_device_authorization(session).await
            }
        }
    }
}

struct IssuedAccessToken {
    token: SecretString,
}

#[async_trait]
impl AccessTokenProvider for IssuedAccessToken {
    async fn get_access_token(&self, _force_refresh: bool) -> Result<SecretString, AuthError> {
        Ok(self.token.clone())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphProfile {
    id: String,
    user_principal_name: String,
}

/// Production `/me` verifier backed by the hardened Microsoft Graph transport.
#[derive(Clone, Copy, Debug, Default)]
pub struct GraphProfileVerifier;

#[async_trait]
impl ProfileVerifier for GraphProfileVerifier {
    async fn get_profile(
        &self,
        access_token: &SecretString,
    ) -> Result<MailboxIdentity, GraphError> {
        let provider = Arc::new(IssuedAccessToken {
            token: access_token.clone(),
        });
        let transport = GraphTransport::build(provider, Arc::new(TokioSleeper))?;
        let url = GraphUrl::build("/me?$select=id,userPrincipalName")?;
        let profile: GraphProfile = transport.get_json(&url).await?;
        build_mailbox_identity(profile)
    }
}

fn build_mailbox_identity(profile: GraphProfile) -> Result<MailboxIdentity, GraphError> {
    if profile.id.trim().is_empty() || profile.user_principal_name.trim().is_empty() {
        return Err(GraphError::MalformedJson);
    }
    Ok(MailboxIdentity {
        id: profile.id,
        user_principal_name: profile.user_principal_name,
    })
}

/// Terminal output for browser, device-code, and success instructions.
#[derive(Clone, Copy, Debug, Default)]
pub struct ConsoleLoginPrompter;

impl LoginPrompter for ConsoleLoginPrompter {
    fn show_browser_login(&self, account: &str, url: &Url) {
        println!("Authenticating account '{account}'.");
        println!("If the browser does not open, visit:\n{url}");
    }

    fn show_device_login(&self, account: &str, url: &Url, code: &SecretString) {
        println!("Authenticating account '{account}' with a device code.");
        println!("Visit {url} and enter code: {}", code.expose_secret());
    }

    fn show_authenticated(&self, account: &str, user: &str) {
        println!("Account '{account}' authenticated as {user}.");
    }
}

/// Coordinates credential refresh, interactive login, identity checks, and persistence.
pub struct InitRunner<C, A, V, P> {
    credentials: Arc<C>,
    authenticator: Arc<A>,
    profiles: Arc<V>,
    prompter: Arc<P>,
}

impl<C, A, V, P> InitRunner<C, A, V, P> {
    /// Build an account initializer from independently testable subsystem boundaries.
    pub const fn new(
        credentials: Arc<C>,
        authenticator: Arc<A>,
        profiles: Arc<V>,
        prompter: Arc<P>,
    ) -> Self {
        Self {
            credentials,
            authenticator,
            profiles,
            prompter,
        }
    }
}

impl<C, A, V, P> InitRunner<C, A, V, P>
where
    C: CredentialStore,
    A: AccountAuthenticator,
    V: ProfileVerifier,
    P: LoginPrompter,
{
    /// Authenticate one account and persist credentials only after `/me` matches.
    pub async fn initialize_account(
        &self,
        account: &AccountConfig,
        method: LoginMethod,
    ) -> Result<MailboxIdentity, InitError> {
        let stored_refresh_token = self.credentials.get_refresh_token(&account.name)?;
        let (grant, interactive) = match stored_refresh_token.as_ref() {
            Some(refresh_token) => match self.authenticator.refresh(account, refresh_token).await {
                Ok(grant) => (grant, false),
                Err(AuthError::TokenExchange) => (
                    self.authenticator
                        .authenticate(account, method, self.prompter.as_ref())
                        .await?,
                    true,
                ),
                Err(error) => return Err(error.into()),
            },
            None => (
                self.authenticator
                    .authenticate(account, method, self.prompter.as_ref())
                    .await?,
                true,
            ),
        };
        if interactive && grant.refresh_token.is_none() {
            return Err(InitError::MissingRefreshToken);
        }

        let identity = self.profiles.get_profile(&grant.access_token).await?;
        if !identity
            .user_principal_name
            .eq_ignore_ascii_case(&account.user)
        {
            return Err(InitError::IdentityMismatch {
                account: account.name.clone(),
                expected: account.user.clone(),
                actual: identity.user_principal_name,
            });
        }
        if let Some(refresh_token) = grant.refresh_token.as_ref() {
            self.credentials
                .replace_refresh_token(&account.name, refresh_token)?;
        }
        self.prompter
            .show_authenticated(&account.name, &identity.user_principal_name);
        Ok(identity)
    }
}

/// Safe initialization failures containing no token or authorization-code values.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum InitError {
    /// Entra authentication or credential persistence failed.
    #[error(transparent)]
    Authentication(#[from] AuthError),
    /// Microsoft Graph could not verify the signed-in user.
    #[error(transparent)]
    Graph(#[from] GraphError),
    /// The signed-in Microsoft identity did not match the selected configuration.
    #[error(
        "account '{account}' expected Microsoft 365 user '{expected}', but signed in as '{actual}'"
    )]
    IdentityMismatch {
        /// Selected local account.
        account: String,
        /// Configured Microsoft 365 identity.
        expected: String,
        /// Identity returned by Graph.
        actual: String,
    },
    /// Entra omitted the refresh token needed for future unattended operation.
    #[error("Microsoft Entra did not issue a refresh token; verify public-client permissions")]
    MissingRefreshToken,
}

#[cfg(test)]
mod tests {
    use super::{
        AccessTokenProvider, AccountAuthenticator, ConsoleLoginPrompter, EntraAccountAuthenticator,
        EntraEndpoints, GraphProfile, IssuedAccessToken, LoginMethod, LoginPrompter,
        MailboxIdentity, ProfileVerifier, build_mailbox_identity,
    };
    use crate::auth::{AuthError, BrowserLauncher};
    use crate::config::{AccountConfig, FolderFilter};
    use crate::graph::GraphError;
    use secrecy::{ExposeSecret, SecretString};
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::Duration;
    use url::Url;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn build_account() -> AccountConfig {
        AccountConfig {
            name: "work".into(),
            maildir: PathBuf::from("/mail/work"),
            user: "me@example.com".into(),
            client_id: "client-id".into(),
            tenant: "organizations".into(),
            folder_separator: ".".into(),
            folder_filter: FolderFilter::All,
        }
    }

    fn build_endpoints(server: &MockServer) -> EntraEndpoints {
        EntraEndpoints {
            authorization: Url::parse(&format!("{}/authorize", server.uri()))
                .expect("authorization endpoint should be valid"),
            token: Url::parse(&format!("{}/token", server.uri()))
                .expect("token endpoint should be valid"),
            device_authorization: Url::parse(&format!("{}/devicecode", server.uri()))
                .expect("device endpoint should be valid"),
        }
    }

    #[derive(Default)]
    struct RecordingPrompter {
        browser_accounts: Mutex<Vec<String>>,
        device_codes: Mutex<Vec<String>>,
    }

    impl LoginPrompter for RecordingPrompter {
        fn show_browser_login(&self, account: &str, _url: &Url) {
            if let Ok(mut accounts) = self.browser_accounts.lock() {
                accounts.push(account.to_owned());
            }
        }

        fn show_device_login(&self, _account: &str, _url: &Url, code: &SecretString) {
            if let Ok(mut codes) = self.device_codes.lock() {
                codes.push(code.expose_secret().to_owned());
            }
        }

        fn show_authenticated(&self, _account: &str, _user: &str) {}
    }

    #[derive(Clone, Copy)]
    struct CallbackBrowser;

    impl BrowserLauncher for CallbackBrowser {
        fn open_url(&self, authorization_url: &Url) -> Result<(), AuthError> {
            let query: HashMap<_, _> = authorization_url.query_pairs().into_owned().collect();
            let state = query.get("state").ok_or(AuthError::InvalidCallback)?;
            let redirect = query
                .get("redirect_uri")
                .ok_or(AuthError::InvalidCallback)
                .and_then(|value| Url::parse(value).map_err(|_| AuthError::InvalidCallback))?;
            let port = redirect.port().ok_or(AuthError::InvalidCallback)?;
            let target = format!("/?code=mock-code&state={state}");
            std::thread::spawn(move || {
                if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
                    let request = format!(
                        "GET {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
                    );
                    let _write_result = stream.write_all(request.as_bytes());
                    let mut response = Vec::new();
                    let _read_result = stream.read_to_end(&mut response);
                }
            });
            Ok(())
        }
    }

    #[test]
    fn converts_complete_graph_profiles_to_domain_identities() {
        assert_eq!(
            build_mailbox_identity(GraphProfile {
                id: "user-id".into(),
                user_principal_name: "me@example.com".into(),
            }),
            Ok(MailboxIdentity {
                id: "user-id".into(),
                user_principal_name: "me@example.com".into(),
            })
        );
    }

    #[test]
    fn rejects_graph_profiles_without_required_identity_values() {
        for (id, user_principal_name) in [
            ("", "me@example.com"),
            ("user-id", ""),
            (" ", "user@example.com"),
        ] {
            assert_eq!(
                build_mailbox_identity(GraphProfile {
                    id: id.into(),
                    user_principal_name: user_principal_name.into(),
                }),
                Err(GraphError::MalformedJson)
            );
        }
    }

    #[tokio::test]
    async fn concrete_authenticator_refreshes_against_the_configured_token_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "refreshed-access",
                "token_type": "Bearer",
                "expires_in": 3600,
                "refresh_token": "rotated-refresh"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let authenticator = EntraAccountAuthenticator::new_for_test(
            CallbackBrowser,
            Duration::from_secs(2),
            build_endpoints(&server),
        );

        let grant = authenticator
            .refresh(&build_account(), &SecretString::from("stored-refresh"))
            .await
            .expect("refresh should succeed");

        assert_eq!(grant.access_token.expose_secret(), "refreshed-access");
    }

    #[tokio::test]
    async fn concrete_authenticator_completes_a_local_pkce_callback() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=authorization_code"))
            .and(body_string_contains("code=mock-code"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "browser-access",
                "token_type": "Bearer",
                "expires_in": 3600,
                "refresh_token": "browser-refresh"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let authenticator = EntraAccountAuthenticator::new_for_test(
            CallbackBrowser,
            Duration::from_secs(2),
            build_endpoints(&server),
        );
        let prompter = RecordingPrompter::default();

        let grant = authenticator
            .authenticate(&build_account(), LoginMethod::Browser, &prompter)
            .await
            .expect("browser callback should complete");

        assert_eq!(grant.access_token.expose_secret(), "browser-access");
        assert_eq!(
            *prompter
                .browser_accounts
                .lock()
                .expect("browser prompts should be readable"),
            ["work"]
        );
    }

    #[tokio::test]
    async fn concrete_authenticator_completes_device_authorization() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/devicecode"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "device_code": "device-secret",
                "user_code": "ABCD-EFGH",
                "verification_uri": "https://microsoft.com/devicelogin",
                "expires_in": 900,
                "interval": 1
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains(
                "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "device-access",
                "token_type": "Bearer",
                "expires_in": 3600,
                "refresh_token": "device-refresh"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let authenticator = EntraAccountAuthenticator::new_for_test(
            CallbackBrowser,
            Duration::from_secs(2),
            build_endpoints(&server),
        );
        let prompter = RecordingPrompter::default();

        let grant = authenticator
            .authenticate(&build_account(), LoginMethod::DeviceCode, &prompter)
            .await
            .expect("device authorization should complete");

        assert_eq!(grant.access_token.expose_secret(), "device-access");
        assert_eq!(
            *prompter
                .device_codes
                .lock()
                .expect("device prompts should be readable"),
            ["ABCD-EFGH"]
        );
    }

    #[tokio::test]
    async fn issued_access_token_replays_the_same_in_memory_token() {
        let provider = IssuedAccessToken {
            token: SecretString::from("issued-access"),
        };

        let token = provider
            .get_access_token(true)
            .await
            .expect("issued token should be available");

        assert_eq!(token.expose_secret(), "issued-access");
    }

    #[test]
    fn constructs_production_login_adapters() {
        let _authenticator = EntraAccountAuthenticator::new(CallbackBrowser);
        let _profiles: &dyn ProfileVerifier = &super::GraphProfileVerifier;
        let prompt = ConsoleLoginPrompter;
        let url = Url::parse("https://microsoft.com/devicelogin")
            .expect("device login URL should be valid");

        prompt.show_browser_login("work", &url);
        prompt.show_device_login("work", &url, &SecretString::from("ABCD-EFGH"));
        prompt.show_authenticated("work", "me@example.com");
    }
}
