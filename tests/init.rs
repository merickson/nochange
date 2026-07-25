use async_trait::async_trait;
use nochange::auth::{AuthError, CredentialStore, TokenGrant};
use nochange::config::AccountConfig;
use nochange::config::FolderFilter;
use nochange::graph::GraphError;
use nochange::init::{
    AccountAuthenticator, InitError, InitRunner, LoginMethod, LoginPrompter, MailboxIdentity,
    ProfileVerifier,
};
use secrecy::{ExposeSecret, SecretString};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use url::Url;

fn build_account() -> AccountConfig {
    AccountConfig {
        name: "work".into(),
        maildir: PathBuf::from("/mail/work"),
        user: "Me@Example.com".into(),
        client_id: "client-id".into(),
        tenant: "organizations".into(),
        folder_separator: ".".into(),
        folder_filter: FolderFilter::All,
    }
}

fn build_grant(access: &str, refresh: Option<&str>) -> TokenGrant {
    TokenGrant {
        access_token: SecretString::from(access),
        refresh_token: refresh.map(SecretString::from),
        expires_in: Duration::from_secs(3_600),
    }
}

#[derive(Default)]
struct FakeCredentialStore {
    current: Mutex<Option<SecretString>>,
    replacements: Mutex<Vec<String>>,
}

struct RejectingCredentialStore;

impl CredentialStore for RejectingCredentialStore {
    fn get_refresh_token(&self, _account: &str) -> Result<Option<SecretString>, AuthError> {
        Ok(None)
    }

    fn replace_refresh_token(
        &self,
        _account: &str,
        _token: &SecretString,
    ) -> Result<(), AuthError> {
        Err(AuthError::CredentialStore)
    }

    fn delete_refresh_token(&self, _account: &str) -> Result<(), AuthError> {
        Err(AuthError::CredentialStore)
    }
}

impl FakeCredentialStore {
    fn with_token(token: &str) -> Self {
        Self {
            current: Mutex::new(Some(token.into())),
            replacements: Mutex::default(),
        }
    }
}

impl CredentialStore for FakeCredentialStore {
    fn get_refresh_token(&self, _account: &str) -> Result<Option<SecretString>, AuthError> {
        self.current
            .lock()
            .map_err(|_| AuthError::CredentialStore)
            .map(|token| token.clone())
    }

    fn replace_refresh_token(&self, _account: &str, token: &SecretString) -> Result<(), AuthError> {
        let token = token.expose_secret().to_owned();
        *self
            .current
            .lock()
            .map_err(|_| AuthError::CredentialStore)? = Some(token.clone().into());
        self.replacements
            .lock()
            .map_err(|_| AuthError::CredentialStore)?
            .push(token);
        Ok(())
    }

    fn delete_refresh_token(&self, _account: &str) -> Result<(), AuthError> {
        *self
            .current
            .lock()
            .map_err(|_| AuthError::CredentialStore)? = None;
        Ok(())
    }
}

struct FakeAuthenticator {
    refresh_result: Mutex<Option<Result<TokenGrant, AuthError>>>,
    interactive_grant: Mutex<Option<TokenGrant>>,
    refresh_tokens: Mutex<Vec<String>>,
    login_methods: Mutex<Vec<LoginMethod>>,
}

impl FakeAuthenticator {
    fn interactive(grant: TokenGrant) -> Self {
        Self {
            refresh_result: Mutex::default(),
            interactive_grant: Mutex::new(Some(grant)),
            refresh_tokens: Mutex::default(),
            login_methods: Mutex::default(),
        }
    }

    fn refreshing(result: Result<TokenGrant, AuthError>, fallback: Option<TokenGrant>) -> Self {
        Self {
            refresh_result: Mutex::new(Some(result)),
            interactive_grant: Mutex::new(fallback),
            refresh_tokens: Mutex::default(),
            login_methods: Mutex::default(),
        }
    }
}

#[async_trait]
impl AccountAuthenticator for FakeAuthenticator {
    async fn refresh(
        &self,
        _account: &AccountConfig,
        refresh_token: &SecretString,
    ) -> Result<TokenGrant, AuthError> {
        self.refresh_tokens
            .lock()
            .map_err(|_| AuthError::TokenExchange)?
            .push(refresh_token.expose_secret().to_owned());
        self.refresh_result
            .lock()
            .map_err(|_| AuthError::TokenExchange)?
            .take()
            .ok_or(AuthError::TokenExchange)?
    }

    async fn authenticate(
        &self,
        _account: &AccountConfig,
        method: LoginMethod,
        _prompter: &dyn LoginPrompter,
    ) -> Result<TokenGrant, AuthError> {
        self.login_methods
            .lock()
            .map_err(|_| AuthError::TokenExchange)?
            .push(method);
        self.interactive_grant
            .lock()
            .map_err(|_| AuthError::TokenExchange)?
            .take()
            .ok_or(AuthError::TokenExchange)
    }
}

struct FakeProfileVerifier {
    result: Result<MailboxIdentity, GraphError>,
    access_tokens: Mutex<Vec<String>>,
}

impl FakeProfileVerifier {
    fn matching() -> Self {
        Self {
            result: Ok(MailboxIdentity {
                id: "user-id".into(),
                user_principal_name: "me@example.com".into(),
            }),
            access_tokens: Mutex::default(),
        }
    }
}

#[async_trait]
impl ProfileVerifier for FakeProfileVerifier {
    async fn get_profile(
        &self,
        access_token: &SecretString,
    ) -> Result<MailboxIdentity, GraphError> {
        self.access_tokens
            .lock()
            .map_err(|_| GraphError::Request)?
            .push(access_token.expose_secret().to_owned());
        self.result.clone()
    }
}

#[derive(Default)]
struct RecordingPrompter {
    authenticated: Mutex<Vec<(String, String)>>,
}

impl LoginPrompter for RecordingPrompter {
    fn show_browser_login(&self, _account: &str, _url: &Url) {}

    fn show_device_login(&self, _account: &str, _url: &Url, _code: &SecretString) {}

    fn show_authenticated(&self, account: &str, user: &str) {
        if let Ok(mut authenticated) = self.authenticated.lock() {
            authenticated.push((account.to_owned(), user.to_owned()));
        }
    }
}

#[tokio::test]
async fn authenticates_interactively_verifies_identity_then_stores_credentials() {
    let credentials = Arc::new(FakeCredentialStore::default());
    let authenticator = Arc::new(FakeAuthenticator::interactive(build_grant(
        "access-secret",
        Some("refresh-secret"),
    )));
    let profiles = Arc::new(FakeProfileVerifier::matching());
    let prompts = Arc::new(RecordingPrompter::default());
    let runner = InitRunner::new(
        Arc::clone(&credentials),
        Arc::clone(&authenticator),
        Arc::clone(&profiles),
        Arc::clone(&prompts),
    );

    let identity = runner
        .initialize_account(&build_account(), LoginMethod::Browser)
        .await
        .expect("interactive login should complete");

    assert_eq!(identity.user_principal_name, "me@example.com");
    assert_eq!(
        *authenticator
            .login_methods
            .lock()
            .expect("login calls should be readable"),
        [LoginMethod::Browser]
    );
    assert_eq!(
        *profiles
            .access_tokens
            .lock()
            .expect("profile calls should be readable"),
        ["access-secret"]
    );
    assert_eq!(
        *credentials
            .replacements
            .lock()
            .expect("credential writes should be readable"),
        ["refresh-secret"]
    );
    assert_eq!(
        *prompts
            .authenticated
            .lock()
            .expect("prompts should be readable"),
        [("work".into(), "me@example.com".into())]
    );
}

#[tokio::test]
async fn refreshes_existing_credentials_without_an_interactive_prompt() {
    let credentials = Arc::new(FakeCredentialStore::with_token("stored-refresh"));
    let authenticator = Arc::new(FakeAuthenticator::refreshing(
        Ok(build_grant("fresh-access", Some("rotated-refresh"))),
        None,
    ));
    let runner = InitRunner::new(
        Arc::clone(&credentials),
        Arc::clone(&authenticator),
        Arc::new(FakeProfileVerifier::matching()),
        Arc::new(RecordingPrompter::default()),
    );

    runner
        .initialize_account(&build_account(), LoginMethod::DeviceCode)
        .await
        .expect("stored credentials should be refreshed");

    assert_eq!(
        *authenticator
            .refresh_tokens
            .lock()
            .expect("refresh calls should be readable"),
        ["stored-refresh"]
    );
    assert!(
        authenticator
            .login_methods
            .lock()
            .expect("login calls should be readable")
            .is_empty()
    );
    assert_eq!(
        *credentials
            .replacements
            .lock()
            .expect("credential writes should be readable"),
        ["rotated-refresh"]
    );
}

#[tokio::test]
async fn falls_back_to_the_selected_interactive_method_for_an_invalid_refresh_token() {
    let credentials = Arc::new(FakeCredentialStore::with_token("invalid-refresh"));
    let authenticator = Arc::new(FakeAuthenticator::refreshing(
        Err(AuthError::TokenExchange),
        Some(build_grant("interactive-access", Some("new-refresh"))),
    ));
    let runner = InitRunner::new(
        Arc::clone(&credentials),
        Arc::clone(&authenticator),
        Arc::new(FakeProfileVerifier::matching()),
        Arc::new(RecordingPrompter::default()),
    );

    runner
        .initialize_account(&build_account(), LoginMethod::DeviceCode)
        .await
        .expect("invalid refresh token should trigger login");

    assert_eq!(
        *authenticator
            .login_methods
            .lock()
            .expect("login calls should be readable"),
        [LoginMethod::DeviceCode]
    );
    assert_eq!(
        *credentials
            .replacements
            .lock()
            .expect("credential writes should be readable"),
        ["new-refresh"]
    );
}

#[tokio::test]
async fn does_not_store_credentials_for_the_wrong_microsoft_account() {
    let credentials = Arc::new(FakeCredentialStore::default());
    let profiles = FakeProfileVerifier {
        result: Ok(MailboxIdentity {
            id: "other-id".into(),
            user_principal_name: "other@example.com".into(),
        }),
        access_tokens: Mutex::default(),
    };
    let runner = InitRunner::new(
        Arc::clone(&credentials),
        Arc::new(FakeAuthenticator::interactive(build_grant(
            "access-secret",
            Some("refresh-secret"),
        ))),
        Arc::new(profiles),
        Arc::new(RecordingPrompter::default()),
    );

    let result = runner
        .initialize_account(&build_account(), LoginMethod::Browser)
        .await;

    assert!(matches!(result, Err(InitError::IdentityMismatch { .. })));
    assert!(
        credentials
            .replacements
            .lock()
            .expect("credential writes should be readable")
            .is_empty()
    );
}

#[tokio::test]
async fn rejects_interactive_grants_without_a_refresh_token() {
    let credentials = Arc::new(FakeCredentialStore::default());
    let runner = InitRunner::new(
        Arc::clone(&credentials),
        Arc::new(FakeAuthenticator::interactive(build_grant(
            "access-secret",
            None,
        ))),
        Arc::new(FakeProfileVerifier::matching()),
        Arc::new(RecordingPrompter::default()),
    );

    let result = runner
        .initialize_account(&build_account(), LoginMethod::Browser)
        .await;

    assert_eq!(result, Err(InitError::MissingRefreshToken));
    assert!(
        credentials
            .replacements
            .lock()
            .expect("credential writes should be readable")
            .is_empty()
    );
}

#[tokio::test]
async fn does_not_store_credentials_when_graph_cannot_verify_the_identity() {
    let credentials = Arc::new(FakeCredentialStore::default());
    let profiles = FakeProfileVerifier {
        result: Err(GraphError::Request),
        access_tokens: Mutex::default(),
    };
    let runner = InitRunner::new(
        Arc::clone(&credentials),
        Arc::new(FakeAuthenticator::interactive(build_grant(
            "access-secret",
            Some("refresh-secret"),
        ))),
        Arc::new(profiles),
        Arc::new(RecordingPrompter::default()),
    );

    let result = runner
        .initialize_account(&build_account(), LoginMethod::Browser)
        .await;

    assert_eq!(result, Err(InitError::Graph(GraphError::Request)));
    assert!(
        credentials
            .replacements
            .lock()
            .expect("credential writes should be readable")
            .is_empty()
    );
}

#[tokio::test]
async fn does_not_start_interactive_login_for_non_oauth_refresh_failures() {
    let credentials = Arc::new(FakeCredentialStore::with_token("stored-refresh"));
    let authenticator = Arc::new(FakeAuthenticator::refreshing(
        Err(AuthError::CredentialStore),
        Some(build_grant("interactive-access", Some("new-refresh"))),
    ));
    let runner = InitRunner::new(
        credentials,
        Arc::clone(&authenticator),
        Arc::new(FakeProfileVerifier::matching()),
        Arc::new(RecordingPrompter::default()),
    );

    let result = runner
        .initialize_account(&build_account(), LoginMethod::Browser)
        .await;

    assert_eq!(
        result,
        Err(InitError::Authentication(AuthError::CredentialStore))
    );
    assert!(
        authenticator
            .login_methods
            .lock()
            .expect("login calls should be readable")
            .is_empty()
    );
}

#[tokio::test]
async fn reports_credential_store_failures_after_identity_verification() {
    let runner = InitRunner::new(
        Arc::new(RejectingCredentialStore),
        Arc::new(FakeAuthenticator::interactive(build_grant(
            "access-secret",
            Some("refresh-secret"),
        ))),
        Arc::new(FakeProfileVerifier::matching()),
        Arc::new(RecordingPrompter::default()),
    );

    let result = runner
        .initialize_account(&build_account(), LoginMethod::Browser)
        .await;

    assert_eq!(
        result,
        Err(InitError::Authentication(AuthError::CredentialStore))
    );
}
