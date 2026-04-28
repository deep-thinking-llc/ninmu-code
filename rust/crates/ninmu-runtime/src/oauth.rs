use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::config::OAuthConfig;

/// Persisted OAuth access token bundle used by the CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthTokenSet {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<u64>,
    pub scopes: Vec<String>,
}

/// PKCE verifier/challenge pair generated for an OAuth authorization flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkceCodePair {
    pub verifier: String,
    pub challenge: String,
    pub challenge_method: PkceChallengeMethod,
}

/// Challenge algorithms supported by the local PKCE helpers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PkceChallengeMethod {
    S256,
}

impl PkceChallengeMethod {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::S256 => "S256",
        }
    }
}

/// Parameters needed to build an authorization URL for browser-based login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthAuthorizationRequest {
    pub authorize_url: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub scopes: Vec<String>,
    pub state: String,
    pub code_challenge: String,
    pub code_challenge_method: PkceChallengeMethod,
    pub extra_params: BTreeMap<String, String>,
}

/// Request body for exchanging an OAuth authorization code for tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthTokenExchangeRequest {
    pub grant_type: &'static str,
    pub code: String,
    pub redirect_uri: String,
    pub client_id: String,
    pub code_verifier: String,
    pub state: String,
}

/// Request body for refreshing an existing OAuth token set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthRefreshRequest {
    pub grant_type: &'static str,
    pub refresh_token: String,
    pub client_id: String,
    pub scopes: Vec<String>,
}

/// Parsed query parameters returned to the local OAuth callback endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthCallbackParams {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

/// Stored context for an in-flight OAuth authorization flow.
#[derive(Debug, Clone)]
pub struct PendingOAuthFlow {
    pub state: String,
    pub code_verifier: String,
    pub redirect_uri: String,
    pub created_at: Instant,
}

/// In-memory store for validating OAuth callbacks and preventing CSRF replay.
#[derive(Debug)]
pub struct OAuthFlowStore {
    pub flows: Arc<Mutex<HashMap<String, PendingOAuthFlow>>>,
    pub max_age: Duration,
}

impl OAuthFlowStore {
    #[must_use]
    pub fn new(max_age: Duration) -> Self {
        Self {
            flows: Arc::new(Mutex::new(HashMap::new())),
            max_age,
        }
    }

    pub fn register(&self, flow: PendingOAuthFlow) {
        let mut flows = self.flows.lock().expect("flows lock");
        flows.insert(flow.state.clone(), flow);
    }

    pub fn validate_and_remove(&self, state: &str) -> Result<PendingOAuthFlow, String> {
        let mut flows = self.flows.lock().expect("flows lock");
        let flow = flows
            .remove(state)
            .ok_or_else(|| "state not found or already consumed".to_string())?;
        if flow.created_at.elapsed() > self.max_age {
            return Err("state expired".to_string());
        }
        Ok(flow)
    }

    pub fn sweep_expired(&self) {
        let mut flows = self.flows.lock().expect("flows lock");
        flows.retain(|_, flow| flow.created_at.elapsed() <= self.max_age);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredOAuthCredentials {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_at: Option<u64>,
    #[serde(default)]
    scopes: Vec<String>,
}

impl From<OAuthTokenSet> for StoredOAuthCredentials {
    fn from(value: OAuthTokenSet) -> Self {
        Self {
            access_token: value.access_token,
            refresh_token: value.refresh_token,
            expires_at: value.expires_at,
            scopes: value.scopes,
        }
    }
}

impl From<StoredOAuthCredentials> for OAuthTokenSet {
    fn from(value: StoredOAuthCredentials) -> Self {
        Self {
            access_token: value.access_token,
            refresh_token: value.refresh_token,
            expires_at: value.expires_at,
            scopes: value.scopes,
        }
    }
}

impl OAuthAuthorizationRequest {
    #[must_use]
    pub fn from_config(
        config: &OAuthConfig,
        redirect_uri: impl Into<String>,
        state: impl Into<String>,
        pkce: &PkceCodePair,
    ) -> Self {
        Self {
            authorize_url: config.authorize_url.clone(),
            client_id: config.client_id.clone(),
            redirect_uri: redirect_uri.into(),
            scopes: config.scopes.clone(),
            state: state.into(),
            code_challenge: pkce.challenge.clone(),
            code_challenge_method: pkce.challenge_method,
            extra_params: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn with_extra_param(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_params.insert(key.into(), value.into());
        self
    }

    #[must_use]
    pub fn build_url(&self) -> String {
        let mut params = vec![
            ("response_type", "code".to_string()),
            ("client_id", self.client_id.clone()),
            ("redirect_uri", self.redirect_uri.clone()),
            ("scope", self.scopes.join(" ")),
            ("state", self.state.clone()),
            ("code_challenge", self.code_challenge.clone()),
            (
                "code_challenge_method",
                self.code_challenge_method.as_str().to_string(),
            ),
        ];
        params.extend(
            self.extra_params
                .iter()
                .map(|(key, value)| (key.as_str(), value.clone())),
        );
        let query = params
            .into_iter()
            .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(&value)))
            .collect::<Vec<_>>()
            .join("&");
        format!(
            "{}{}{}",
            self.authorize_url,
            if self.authorize_url.contains('?') {
                '&'
            } else {
                '?'
            },
            query
        )
    }
}

impl OAuthTokenExchangeRequest {
    #[must_use]
    pub fn from_config(
        config: &OAuthConfig,
        code: impl Into<String>,
        state: impl Into<String>,
        verifier: impl Into<String>,
        redirect_uri: impl Into<String>,
    ) -> Self {
        Self {
            grant_type: "authorization_code",
            code: code.into(),
            redirect_uri: redirect_uri.into(),
            client_id: config.client_id.clone(),
            code_verifier: verifier.into(),
            state: state.into(),
        }
    }

    #[must_use]
    pub fn form_params(&self) -> BTreeMap<&str, String> {
        BTreeMap::from([
            ("grant_type", self.grant_type.to_string()),
            ("code", self.code.clone()),
            ("redirect_uri", self.redirect_uri.clone()),
            ("client_id", self.client_id.clone()),
            ("code_verifier", self.code_verifier.clone()),
            ("state", self.state.clone()),
        ])
    }
}

impl OAuthRefreshRequest {
    #[must_use]
    pub fn from_config(
        config: &OAuthConfig,
        refresh_token: impl Into<String>,
        scopes: Option<Vec<String>>,
    ) -> Self {
        Self {
            grant_type: "refresh_token",
            refresh_token: refresh_token.into(),
            client_id: config.client_id.clone(),
            scopes: scopes.unwrap_or_else(|| config.scopes.clone()),
        }
    }

    #[must_use]
    pub fn form_params(&self) -> BTreeMap<&str, String> {
        BTreeMap::from([
            ("grant_type", self.grant_type.to_string()),
            ("refresh_token", self.refresh_token.clone()),
            ("client_id", self.client_id.clone()),
            ("scope", self.scopes.join(" ")),
        ])
    }
}

pub fn generate_pkce_pair() -> io::Result<PkceCodePair> {
    let verifier = generate_random_token(32)?;
    Ok(PkceCodePair {
        challenge: code_challenge_s256(&verifier),
        verifier,
        challenge_method: PkceChallengeMethod::S256,
    })
}

pub fn generate_state() -> io::Result<String> {
    generate_random_token(32)
}

#[must_use]
pub fn code_challenge_s256(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64url_encode(&digest)
}

#[must_use]
pub fn loopback_redirect_uri(port: u16) -> String {
    format!("http://localhost:{port}/callback")
}

pub fn credentials_path() -> io::Result<PathBuf> {
    Ok(credentials_home_dir()?.join("credentials.json"))
}

pub fn load_oauth_credentials() -> io::Result<Option<OAuthTokenSet>> {
    let path = credentials_path()?;
    let root = read_credentials_root(&path)?;
    let Some(oauth) = root.get("oauth") else {
        return Ok(None);
    };
    if oauth.is_null() {
        return Ok(None);
    }
    let stored = serde_json::from_value::<StoredOAuthCredentials>(oauth.clone())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(Some(stored.into()))
}

pub fn save_oauth_credentials(token_set: &OAuthTokenSet) -> io::Result<()> {
    let path = credentials_path()?;
    let mut root = read_credentials_root(&path)?;
    root.insert(
        "oauth".to_string(),
        serde_json::to_value(StoredOAuthCredentials::from(token_set.clone()))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
    );
    write_credentials_root(&path, &root)
}

pub fn clear_oauth_credentials() -> io::Result<()> {
    let path = credentials_path()?;
    let mut root = read_credentials_root(&path)?;
    root.remove("oauth");
    write_credentials_root(&path, &root)
}

pub fn parse_oauth_callback_request_target(
    target: &str,
    store: &OAuthFlowStore,
) -> Result<OAuthCallbackParams, String> {
    let (path, query) = target
        .split_once('?')
        .map_or((target, ""), |(path, query)| (path, query));
    if path != "/callback" {
        return Err(format!("unexpected callback path: {path}"));
    }
    let params = parse_oauth_callback_query(query)?;
    let state = params.state.as_ref().ok_or("missing state parameter")?;
    let _flow = store.validate_and_remove(state)?;
    Ok(params)
}

/// Parse callback target without state validation (for legacy/testing use only).
pub fn parse_oauth_callback_request_target_unvalidated(
    target: &str,
) -> Result<OAuthCallbackParams, String> {
    let (path, query) = target
        .split_once('?')
        .map_or((target, ""), |(path, query)| (path, query));
    if path != "/callback" {
        return Err(format!("unexpected callback path: {path}"));
    }
    parse_oauth_callback_query(query)
}

pub fn parse_oauth_callback_query(query: &str) -> Result<OAuthCallbackParams, String> {
    let mut params = BTreeMap::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair
            .split_once('=')
            .map_or((pair, ""), |(key, value)| (key, value));
        params.insert(percent_decode(key)?, percent_decode(value)?);
    }
    Ok(OAuthCallbackParams {
        code: params.get("code").cloned(),
        state: params.get("state").cloned(),
        error: params.get("error").cloned(),
        error_description: params.get("error_description").cloned(),
    })
}

fn generate_random_token(bytes: usize) -> io::Result<String> {
    let mut buffer = vec![0_u8; bytes];
    getrandom::getrandom(&mut buffer).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(base64url_encode(&buffer))
}

fn credentials_home_dir() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os("CLAW_CONFIG_HOME") {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "HOME is not set (on Windows, set USERPROFILE or HOME, \
                 or use CLAW_CONFIG_HOME to point directly at the config directory)",
            )
        })?;
    Ok(PathBuf::from(home).join(".claw"))
}

fn read_credentials_root(path: &PathBuf) -> io::Result<Map<String, Value>> {
    match fs::read_to_string(path) {
        Ok(contents) => {
            if contents.trim().is_empty() {
                return Ok(Map::new());
            }
            serde_json::from_str::<Value>(&contents)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
                .as_object()
                .cloned()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "credentials file must contain a JSON object",
                    )
                })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Map::new()),
        Err(error) => Err(error),
    }
}

fn write_credentials_root(path: &PathBuf, root: &Map<String, Value>) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
    }
    let rendered = serde_json::to_string_pretty(&Value::Object(root.clone()))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let temp_path = path.with_extension("json.tmp");
    fs::write(&temp_path, format!("{rendered}\n"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp_path, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(temp_path, path)
}

fn base64url_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut output = String::new();
    let mut index = 0;
    while index + 3 <= bytes.len() {
        let block = (u32::from(bytes[index]) << 16)
            | (u32::from(bytes[index + 1]) << 8)
            | u32::from(bytes[index + 2]);
        output.push(TABLE[((block >> 18) & 0x3F) as usize] as char);
        output.push(TABLE[((block >> 12) & 0x3F) as usize] as char);
        output.push(TABLE[((block >> 6) & 0x3F) as usize] as char);
        output.push(TABLE[(block & 0x3F) as usize] as char);
        index += 3;
    }
    match bytes.len().saturating_sub(index) {
        1 => {
            let block = u32::from(bytes[index]) << 16;
            output.push(TABLE[((block >> 18) & 0x3F) as usize] as char);
            output.push(TABLE[((block >> 12) & 0x3F) as usize] as char);
        }
        2 => {
            let block = (u32::from(bytes[index]) << 16) | (u32::from(bytes[index + 1]) << 8);
            output.push(TABLE[((block >> 18) & 0x3F) as usize] as char);
            output.push(TABLE[((block >> 12) & 0x3F) as usize] as char);
            output.push(TABLE[((block >> 6) & 0x3F) as usize] as char);
        }
        _ => {}
    }
    output
}

fn percent_encode(value: &str) -> String {
    use percent_encoding::AsciiSet;
    const QUERY: &AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~');
    percent_encoding::percent_encode(value.as_bytes(), QUERY).to_string()
}

fn percent_decode(value: &str) -> Result<String, String> {
    let normalized = value.replace('+', " ");
    percent_encoding::percent_decode_str(&normalized)
        .decode_utf8()
        .map_err(|e| format!("invalid percent encoding: {e}"))
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use super::{
        clear_oauth_credentials, code_challenge_s256, credentials_path, generate_pkce_pair,
        generate_random_token, generate_state, load_oauth_credentials, loopback_redirect_uri,
        parse_oauth_callback_query, parse_oauth_callback_request_target,
        parse_oauth_callback_request_target_unvalidated, save_oauth_credentials,
        OAuthAuthorizationRequest, OAuthConfig, OAuthFlowStore, OAuthRefreshRequest,
        OAuthTokenExchangeRequest, OAuthTokenSet, PendingOAuthFlow,
    };

    fn sample_config() -> OAuthConfig {
        OAuthConfig {
            client_id: "runtime-client".to_string(),
            authorize_url: "https://console.test/oauth/authorize".to_string(),
            token_url: "https://console.test/oauth/token".to_string(),
            callback_port: Some(4545),
            manual_redirect_url: Some("https://console.test/oauth/callback".to_string()),
            scopes: vec!["org:read".to_string(), "user:write".to_string()],
        }
    }

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_env_lock()
    }

    fn temp_config_home() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "runtime-oauth-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ))
    }

    #[test]
    fn s256_challenge_matches_expected_vector() {
        assert_eq!(
            code_challenge_s256("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn generates_pkce_pair_and_state() {
        let pair = generate_pkce_pair().expect("pkce pair");
        let state = generate_state().expect("state");
        assert!(!pair.verifier.is_empty());
        assert!(!pair.challenge.is_empty());
        assert!(!state.is_empty());
    }

    #[test]
    fn builds_authorize_url_and_form_requests() {
        let config = sample_config();
        let pair = generate_pkce_pair().expect("pkce");
        let url = OAuthAuthorizationRequest::from_config(
            &config,
            loopback_redirect_uri(4545),
            "state-123",
            &pair,
        )
        .with_extra_param("login_hint", "user@example.com")
        .build_url();
        assert!(url.starts_with("https://console.test/oauth/authorize?"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=runtime-client"));
        assert!(url.contains("scope=org%3Aread%20user%3Awrite"));
        assert!(url.contains("login_hint=user%40example.com"));

        let exchange = OAuthTokenExchangeRequest::from_config(
            &config,
            "auth-code",
            "state-123",
            pair.verifier,
            loopback_redirect_uri(4545),
        );
        assert_eq!(
            exchange.form_params().get("grant_type").map(String::as_str),
            Some("authorization_code")
        );

        let refresh = OAuthRefreshRequest::from_config(&config, "refresh-token", None);
        assert_eq!(
            refresh.form_params().get("scope").map(String::as_str),
            Some("org:read user:write")
        );
    }

    #[test]
    fn oauth_credentials_round_trip_and_clear_preserves_other_fields() {
        let _guard = env_lock();
        let config_home = temp_config_home();
        std::env::set_var("CLAW_CONFIG_HOME", &config_home);
        let path = credentials_path().expect("credentials path");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        std::fs::write(&path, "{\"other\":\"value\"}\n").expect("seed credentials");

        let token_set = OAuthTokenSet {
            access_token: "access-token".to_string(),
            refresh_token: Some("refresh-token".to_string()),
            expires_at: Some(123),
            scopes: vec!["scope:a".to_string()],
        };
        save_oauth_credentials(&token_set).expect("save credentials");
        assert_eq!(
            load_oauth_credentials().expect("load credentials"),
            Some(token_set)
        );
        let saved = std::fs::read_to_string(&path).expect("read saved file");
        assert!(saved.contains("\"other\": \"value\""));
        assert!(saved.contains("\"oauth\""));

        clear_oauth_credentials().expect("clear credentials");
        assert_eq!(load_oauth_credentials().expect("load cleared"), None);
        let cleared = std::fs::read_to_string(&path).expect("read cleared file");
        assert!(cleared.contains("\"other\": \"value\""));
        assert!(!cleared.contains("\"oauth\""));

        std::env::remove_var("CLAW_CONFIG_HOME");
        std::fs::remove_dir_all(config_home).expect("cleanup temp dir");
    }

    #[test]
    fn parses_callback_query_and_target() {
        let params =
            parse_oauth_callback_query("code=abc123&state=state-1&error_description=needs%20login")
                .expect("parse query");
        assert_eq!(params.code.as_deref(), Some("abc123"));
        assert_eq!(params.state.as_deref(), Some("state-1"));
        assert_eq!(params.error_description.as_deref(), Some("needs login"));

        let params =
            parse_oauth_callback_request_target_unvalidated("/callback?code=abc&state=xyz")
                .expect("parse callback target");
        assert_eq!(params.code.as_deref(), Some("abc"));
        assert_eq!(params.state.as_deref(), Some("xyz"));
        assert!(parse_oauth_callback_request_target_unvalidated("/wrong?code=abc").is_err());
    }

    #[test]
    fn valid_state_exchanges() {
        let store = OAuthFlowStore::new(Duration::from_mins(10));
        store.register(PendingOAuthFlow {
            state: "state-123".to_string(),
            code_verifier: "verifier-abc".to_string(),
            redirect_uri: "http://localhost:4545/callback".to_string(),
            created_at: Instant::now(),
        });
        let params =
            parse_oauth_callback_request_target("/callback?code=abc&state=state-123", &store)
                .expect("should validate state");
        assert_eq!(params.code.as_deref(), Some("abc"));
    }

    #[test]
    fn mismatched_state_rejected() {
        let store = OAuthFlowStore::new(Duration::from_mins(10));
        store.register(PendingOAuthFlow {
            state: "state-real".to_string(),
            code_verifier: "verifier-abc".to_string(),
            redirect_uri: "http://localhost:4545/callback".to_string(),
            created_at: Instant::now(),
        });
        let result =
            parse_oauth_callback_request_target("/callback?code=abc&state=state-fake", &store);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn missing_state_rejected() {
        let store = OAuthFlowStore::new(Duration::from_mins(10));
        let result = parse_oauth_callback_request_target("/callback?code=abc", &store);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing state"));
    }

    #[test]
    fn expired_state_rejected() {
        let store = OAuthFlowStore::new(Duration::from_millis(1));
        store.register(PendingOAuthFlow {
            state: "old-state".to_string(),
            code_verifier: "verifier-abc".to_string(),
            redirect_uri: "http://localhost:4545/callback".to_string(),
            created_at: Instant::now(),
        });
        std::thread::sleep(Duration::from_millis(5));
        let result =
            parse_oauth_callback_request_target("/callback?code=abc&state=old-state", &store);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expired"));
    }

    #[test]
    fn replay_state_rejected() {
        let store = OAuthFlowStore::new(Duration::from_mins(10));
        store.register(PendingOAuthFlow {
            state: "replay-state".to_string(),
            code_verifier: "verifier-abc".to_string(),
            redirect_uri: "http://localhost:4545/callback".to_string(),
            created_at: Instant::now(),
        });
        parse_oauth_callback_request_target("/callback?code=abc&state=replay-state", &store)
            .expect("first use succeeds");
        let result =
            parse_oauth_callback_request_target("/callback?code=abc&state=replay-state", &store);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn credentials_file_mode_is_restricted() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _guard = env_lock();
            let config_home = temp_config_home();
            std::env::set_var("CLAW_CONFIG_HOME", &config_home);
            let path = credentials_path().expect("credentials path");
            std::fs::create_dir_all(path.parent().expect("parent")).expect("create parent");

            let token_set = OAuthTokenSet {
                access_token: "at".to_string(),
                refresh_token: None,
                expires_at: None,
                scopes: vec![],
            };
            save_oauth_credentials(&token_set).expect("save");

            let meta = std::fs::metadata(&path).expect("stat");
            let mode = meta.permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "credentials file must be 0o600");

            let parent_meta = std::fs::metadata(path.parent().unwrap()).expect("stat dir");
            let parent_mode = parent_meta.permissions().mode() & 0o777;
            assert_eq!(parent_mode, 0o700, ".claw directory must be 0o700");

            std::env::remove_var("CLAW_CONFIG_HOME");
            let _ = std::fs::remove_dir_all(&config_home);
        }
    }

    #[test]
    fn generate_random_token_non_empty_and_unique() {
        let a = generate_random_token(32).expect("token a");
        let b = generate_random_token(32).expect("token b");
        assert!(!a.is_empty());
        assert!(!b.is_empty());
        assert_ne!(a, b, "random tokens should be unique");
    }
}
