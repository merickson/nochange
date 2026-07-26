use async_trait::async_trait;
use nochange::auth::{
    AuthError, AuthorizationSession, BrowserLauncher, Clock, CredentialStore, EntraEndpoints,
    EntraTokenExchange, LocalCallbackListener, OAuthTokenExchange, TokenGrant, TokenManager,
    get_required_scopes,
};
use secrecy::{ExposeSecret, SecretString};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[test]
fn builds_commercial_entra_endpoints() {
    let endpoints = EntraEndpoints::build("organizations")
        .expect("the default commercial tenant should be accepted");

    assert_eq!(
        endpoints.authorization.as_str(),
        "https://login.microsoftonline.com/organizations/oauth2/v2.0/authorize"
    );
    assert_eq!(
        endpoints.token.as_str(),
        "https://login.microsoftonline.com/organizations/oauth2/v2.0/token"
    );
    assert_eq!(
        endpoints.device_authorization.as_str(),
        "https://login.microsoftonline.com/organizations/oauth2/v2.0/devicecode"
    );
}

#[test]
fn rejects_tenants_that_could_change_the_authority_url() {
    for tenant in [
        "",
        ".",
        "..",
        "organizations/other",
        "organizations?x=y",
        "organizations#fragment",
        "organization%73",
        "two words",
    ] {
        assert!(
            EntraEndpoints::build(tenant).is_err(),
            "tenant should be rejected: {tenant:?}"
        );
    }
}

#[test]
fn requests_only_required_identity_and_mail_scopes() {
    assert_eq!(
        get_required_scopes(),
        [
            "offline_access",
            "https://graph.microsoft.com/User.Read",
            "https://graph.microsoft.com/Mail.ReadWrite",
            "https://graph.microsoft.com/Mail.Send",
        ]
    );
}

#[test]
fn builds_pkce_authorization_requests_and_verifies_csrf_state() {
    let endpoints = EntraEndpoints::build("organizations").expect("tenant should be valid");
    let redirect =
        url::Url::parse("http://localhost:43123/callback").expect("test redirect should be valid");
    let session = AuthorizationSession::build("client-id", &endpoints, redirect)
        .expect("authorization session should be created");
    let query: std::collections::HashMap<_, _> = session
        .get_authorization_url()
        .query_pairs()
        .into_owned()
        .collect();
    let state = query.get("state").expect("state should be present");

    assert_eq!(
        query.get("client_id").map(String::as_str),
        Some("client-id")
    );
    assert_eq!(query.get("response_type").map(String::as_str), Some("code"));
    assert_eq!(
        query.get("redirect_uri").map(String::as_str),
        Some("http://localhost:43123/callback")
    );
    assert_eq!(
        query.get("code_challenge_method").map(String::as_str),
        Some("S256")
    );
    assert!(query.contains_key("code_challenge"));
    assert_eq!(
        query.get("scope").map(String::as_str),
        Some(get_required_scopes().join(" ").as_str())
    );
    assert!(session.is_valid_state(state));
    assert!(!session.is_valid_state("wrong-state"));

    let debug = format!("{session:?}");
    assert!(!debug.contains(state));
    assert!(
        !debug.contains(
            query
                .get("code_challenge")
                .expect("challenge should be present")
        )
    );
}

#[derive(Default)]
struct RecordingBrowser {
    opened_urls: Mutex<Vec<String>>,
}

impl BrowserLauncher for RecordingBrowser {
    fn open_url(&self, url: &url::Url) -> Result<(), AuthError> {
        self.opened_urls
            .lock()
            .map_err(|_| AuthError::BrowserLauncher)?
            .push(url.to_string());
        Ok(())
    }
}

#[test]
fn opens_the_generated_authorization_url_through_the_browser_boundary() {
    let endpoints = EntraEndpoints::build("organizations").expect("tenant should be valid");
    let redirect = url::Url::parse("http://localhost:49152/").expect("redirect should be valid");
    let session = AuthorizationSession::build("client-id", &endpoints, redirect)
        .expect("authorization session should be built");
    let browser = RecordingBrowser::default();

    session
        .open_with(&browser)
        .expect("browser boundary should accept the URL");

    assert_eq!(
        *browser
            .opened_urls
            .lock()
            .expect("recorded URLs should be readable"),
        [session.get_authorization_url().as_str()]
    );
}

#[test]
fn rejects_empty_public_client_identifiers() {
    let endpoints = EntraEndpoints::build("organizations").expect("tenant should be valid");
    let redirect = url::Url::parse("http://localhost:49152/").expect("redirect should be valid");

    assert!(matches!(
        AuthorizationSession::build(" ", &endpoints, redirect),
        Err(AuthError::InvalidClientId)
    ));
    assert!(matches!(
        EntraTokenExchange::build("", &endpoints),
        Err(AuthError::InvalidClientId)
    ));
}

#[derive(Default)]
struct FakeCredentialStore {
    token: Mutex<Option<SecretString>>,
    replacements: Mutex<Vec<String>>,
}

impl FakeCredentialStore {
    fn with_token(token: &str) -> Self {
        Self {
            token: Mutex::new(Some(token.into())),
            replacements: Mutex::default(),
        }
    }
}

impl CredentialStore for FakeCredentialStore {
    fn get_refresh_token(&self, _account: &str) -> Result<Option<SecretString>, AuthError> {
        Ok(self
            .token
            .lock()
            .map_err(|_| AuthError::CredentialStore)?
            .clone())
    }

    fn replace_refresh_token(&self, _account: &str, token: &SecretString) -> Result<(), AuthError> {
        let value = token.expose_secret().to_owned();
        *self.token.lock().map_err(|_| AuthError::CredentialStore)? = Some(value.clone().into());
        self.replacements
            .lock()
            .map_err(|_| AuthError::CredentialStore)?
            .push(value);
        Ok(())
    }

    fn delete_refresh_token(&self, _account: &str) -> Result<(), AuthError> {
        *self.token.lock().map_err(|_| AuthError::CredentialStore)? = None;
        Ok(())
    }
}

struct FakeTokenExchange {
    grants: Mutex<VecDeque<TokenGrant>>,
    received_refresh_tokens: Mutex<Vec<String>>,
}

impl FakeTokenExchange {
    fn with_grants(grants: impl IntoIterator<Item = TokenGrant>) -> Self {
        Self {
            grants: Mutex::new(grants.into_iter().collect()),
            received_refresh_tokens: Mutex::default(),
        }
    }
}

#[async_trait]
impl OAuthTokenExchange for FakeTokenExchange {
    async fn exchange_refresh_token(
        &self,
        refresh_token: &SecretString,
    ) -> Result<TokenGrant, AuthError> {
        self.received_refresh_tokens
            .lock()
            .map_err(|_| AuthError::TokenExchange)?
            .push(refresh_token.expose_secret().to_owned());
        self.grants
            .lock()
            .map_err(|_| AuthError::TokenExchange)?
            .pop_front()
            .ok_or(AuthError::TokenExchange)
    }
}

fn build_grant(access_token: &str, refresh_token: Option<&str>) -> TokenGrant {
    TokenGrant {
        access_token: access_token.into(),
        refresh_token: refresh_token.map(SecretString::from),
        expires_in: Duration::from_secs(3_600),
    }
}

struct FakeClock {
    seconds: AtomicU64,
}

impl FakeClock {
    fn new(seconds: u64) -> Self {
        Self {
            seconds: AtomicU64::new(seconds),
        }
    }

    fn set(&self, seconds: u64) {
        self.seconds.store(seconds, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn get_now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(self.seconds.load(Ordering::SeqCst))
    }
}

#[tokio::test]
async fn caches_access_tokens_and_rotates_refresh_tokens() {
    let credentials = Arc::new(FakeCredentialStore::with_token("refresh-1"));
    let exchange = Arc::new(FakeTokenExchange::with_grants([build_grant(
        "access-1",
        Some("refresh-2"),
    )]));
    let manager = TokenManager::new("work", Arc::clone(&credentials), Arc::clone(&exchange));

    let first = manager
        .get_access_token(false)
        .await
        .expect("the initial refresh should succeed");
    let second = nochange::auth::AccessTokenProvider::get_access_token(&manager, false)
        .await
        .expect("the cached token should be returned");

    assert_eq!(first.expose_secret(), "access-1");
    assert_eq!(second.expose_secret(), "access-1");
    assert_eq!(
        *exchange
            .received_refresh_tokens
            .lock()
            .expect("captured calls should be readable"),
        ["refresh-1"]
    );
    assert_eq!(
        *credentials
            .replacements
            .lock()
            .expect("captured rotations should be readable"),
        ["refresh-2"]
    );
}

#[tokio::test]
async fn force_refresh_uses_the_rotated_refresh_token() {
    let credentials = Arc::new(FakeCredentialStore::with_token("refresh-1"));
    let exchange = Arc::new(FakeTokenExchange::with_grants([
        build_grant("access-1", Some("refresh-2")),
        build_grant("access-2", None),
    ]));
    let manager = TokenManager::new("work", Arc::clone(&credentials), Arc::clone(&exchange));

    manager
        .get_access_token(false)
        .await
        .expect("the initial refresh should succeed");
    let refreshed = manager
        .get_access_token(true)
        .await
        .expect("a forced refresh should succeed");

    assert_eq!(refreshed.expose_secret(), "access-2");
    assert_eq!(
        *exchange
            .received_refresh_tokens
            .lock()
            .expect("captured calls should be readable"),
        ["refresh-1", "refresh-2"]
    );
    assert_eq!(
        *credentials
            .replacements
            .lock()
            .expect("captured rotations should be readable"),
        ["refresh-2"]
    );
}

#[tokio::test]
async fn refreshes_access_tokens_after_their_expiry() {
    let credentials = Arc::new(FakeCredentialStore::with_token("refresh-1"));
    let exchange = Arc::new(FakeTokenExchange::with_grants([
        build_grant("access-1", None),
        build_grant("access-2", None),
    ]));
    let clock = Arc::new(FakeClock::new(1_000));
    let manager = TokenManager::new_with_clock(
        "work",
        Arc::clone(&credentials),
        Arc::clone(&exchange),
        Arc::clone(&clock),
    );

    manager
        .get_access_token(false)
        .await
        .expect("initial refresh should succeed");
    clock.set(4_601);
    let refreshed = manager
        .get_access_token(false)
        .await
        .expect("expired access token should refresh");

    assert_eq!(refreshed.expose_secret(), "access-2");
    assert_eq!(
        exchange
            .received_refresh_tokens
            .lock()
            .expect("captured calls should be readable")
            .len(),
        2
    );
}

#[tokio::test]
async fn reports_missing_credentials_without_contacting_oauth() {
    let credentials = Arc::new(FakeCredentialStore::default());
    let exchange = Arc::new(FakeTokenExchange::with_grants([]));
    let manager = TokenManager::new("work", Arc::clone(&credentials), Arc::clone(&exchange));

    assert!(matches!(
        manager.get_access_token(false).await,
        Err(AuthError::MissingCredentials { .. })
    ));
    assert!(
        exchange
            .received_refresh_tokens
            .lock()
            .expect("captured calls should be readable")
            .is_empty()
    );
}

#[test]
fn token_debug_output_is_redacted() {
    let grant = build_grant("highly-secret-access", Some("highly-secret-refresh"));
    let debug = format!("{grant:?}");

    assert!(!debug.contains("highly-secret-access"));
    assert!(!debug.contains("highly-secret-refresh"));
}

fn build_test_endpoints(server: &MockServer) -> EntraEndpoints {
    EntraEndpoints {
        authorization: url::Url::parse(&format!("{}/authorize", server.uri()))
            .expect("authorization URL should be valid"),
        token: url::Url::parse(&format!("{}/token", server.uri()))
            .expect("token URL should be valid"),
        device_authorization: url::Url::parse(&format!("{}/devicecode", server.uri()))
            .expect("device URL should be valid"),
    }
}

#[tokio::test]
async fn exchanges_refresh_tokens_as_a_public_client() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("client_id=client-id"))
        .and(body_string_contains("refresh_token=refresh-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "access-secret",
            "token_type": "Bearer",
            "expires_in": 3600,
            "refresh_token": "rotated-secret"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let exchange = EntraTokenExchange::build("client-id", &build_test_endpoints(&server))
        .expect("token exchange should be configured");

    let grant = exchange
        .exchange_refresh_token(&SecretString::from("refresh-secret"))
        .await
        .expect("refresh exchange should succeed");

    assert_eq!(grant.access_token.expose_secret(), "access-secret");
    assert_eq!(
        grant
            .refresh_token
            .expect("rotated token should be present")
            .expose_secret(),
        "rotated-secret"
    );
}

#[tokio::test]
async fn distinguishes_transient_token_transport_failures_from_server_rejections() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": "invalid_grant",
            "error_description": "rejected"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let rejected = EntraTokenExchange::build("client-id", &build_test_endpoints(&server))
        .expect("rejected exchange should build");

    assert!(matches!(
        rejected
            .exchange_refresh_token(&SecretString::from("rejected-refresh"))
            .await,
        Err(AuthError::TokenExchange)
    ));

    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("ephemeral port should be reservable");
    let address = listener.local_addr().expect("listener address should load");
    drop(listener);
    let unavailable_url =
        url::Url::parse(&format!("http://{address}/token")).expect("token URL should be valid");
    let unavailable = EntraTokenExchange::build(
        "client-id",
        &EntraEndpoints {
            authorization: unavailable_url.clone(),
            token: unavailable_url.clone(),
            device_authorization: unavailable_url,
        },
    )
    .expect("unavailable exchange should build");

    assert!(matches!(
        unavailable
            .exchange_refresh_token(&SecretString::from("stored-refresh"))
            .await,
        Err(AuthError::TokenRequest)
    ));
}

#[tokio::test]
async fn exchanges_authorization_codes_with_the_original_pkce_verifier() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .and(body_string_contains("client_id=client-id"))
        .and(body_string_contains("code=authorization-secret"))
        .and(body_string_contains("code_verifier="))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "access-secret",
            "token_type": "Bearer",
            "expires_in": 3600,
            "refresh_token": "refresh-secret"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let endpoints = build_test_endpoints(&server);
    let redirect =
        url::Url::parse("http://localhost:43123/callback").expect("redirect URL should be valid");
    let session = AuthorizationSession::build("client-id", &endpoints, redirect)
        .expect("authorization session should be created");
    let exchange = EntraTokenExchange::build("client-id", &endpoints)
        .expect("token exchange should be configured");

    let grant = exchange
        .exchange_authorization_code(&SecretString::from("authorization-secret"), session)
        .await
        .expect("authorization-code exchange should succeed");

    assert_eq!(grant.access_token.expose_secret(), "access-secret");
    assert_eq!(
        grant
            .refresh_token
            .expect("refresh token should be present")
            .expose_secret(),
        "refresh-secret"
    );
}

#[tokio::test]
async fn completes_device_authorization_without_exposing_codes_in_debug_output() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/devicecode"))
        .and(body_string_contains("client_id=client-id"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "device_code": "device-secret",
            "user_code": "ABCD-EFGH",
            "verification_uri": "https://microsoft.com/devicelogin",
            "expires_in": 900,
            "interval": 1,
            "message": "sign in"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains(
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code",
        ))
        .and(body_string_contains("device_code=device-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "access-secret",
            "token_type": "Bearer",
            "expires_in": 3600,
            "refresh_token": "refresh-secret"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let exchange = EntraTokenExchange::build("client-id", &build_test_endpoints(&server))
        .expect("token exchange should be configured");

    let session = exchange
        .start_device_authorization()
        .await
        .expect("device authorization should start");
    assert_eq!(
        session.get_verification_uri().as_str(),
        "https://microsoft.com/devicelogin"
    );
    assert_eq!(session.get_user_code().expose_secret(), "ABCD-EFGH");
    let debug = format!("{session:?}");
    assert!(!debug.contains("device-secret"));
    assert!(!debug.contains("ABCD-EFGH"));

    let grant = exchange
        .poll_device_authorization(session)
        .await
        .expect("device authorization should complete");
    assert_eq!(grant.access_token.expose_secret(), "access-secret");
    assert_eq!(
        grant
            .refresh_token
            .expect("refresh token should be returned")
            .expose_secret(),
        "refresh-secret"
    );
}

#[tokio::test]
async fn accepts_a_local_callback_only_with_the_original_csrf_state() {
    let endpoints = EntraEndpoints::build("organizations").expect("tenant should be valid");
    let listener = LocalCallbackListener::bind().expect("local callback should bind");
    let session =
        AuthorizationSession::build("client-id", &endpoints, listener.get_redirect_url().clone())
            .expect("authorization session should be created");
    let state = session
        .get_authorization_url()
        .query_pairs()
        .find_map(|(key, value)| (key == "state").then(|| value.into_owned()))
        .expect("authorization URL should contain state");
    let callback_url = format!(
        "{}?code=authorization-secret&state={state}",
        listener.get_redirect_url()
    );
    let callback_request = tokio::spawn(async move {
        reqwest::get(callback_url)
            .await
            .expect("callback request should succeed")
    });

    let code = listener
        .wait_for_code(&session, std::time::Duration::from_secs(2))
        .await
        .expect("valid callback should return its code");
    let response = callback_request.await.expect("callback task should finish");

    assert_eq!(code.expose_secret(), "authorization-secret");
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn rejects_callback_state_mismatches() {
    let endpoints = EntraEndpoints::build("organizations").expect("tenant should be valid");
    let listener = LocalCallbackListener::bind().expect("local callback should bind");
    let session =
        AuthorizationSession::build("client-id", &endpoints, listener.get_redirect_url().clone())
            .expect("authorization session should be created");
    let callback_url = format!(
        "{}?code=authorization-secret&state=wrong-state",
        listener.get_redirect_url()
    );
    let callback_request = tokio::spawn(async move {
        reqwest::get(callback_url)
            .await
            .expect("callback request should succeed")
    });

    assert!(matches!(
        listener
            .wait_for_code(&session, std::time::Duration::from_secs(2))
            .await,
        Err(AuthError::InvalidCallback)
    ));
    assert_eq!(
        callback_request
            .await
            .expect("callback task should finish")
            .status(),
        400
    );
}
