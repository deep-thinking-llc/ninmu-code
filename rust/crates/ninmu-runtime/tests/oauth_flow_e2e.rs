#![allow(clippy::doc_markdown, clippy::uninlined_format_args, unused_imports)]
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ninmu_runtime::{
    clear_oauth_credentials, credentials_path, generate_pkce_pair, generate_state,
    load_oauth_credentials, loopback_redirect_uri, parse_oauth_callback_query,
    parse_oauth_callback_request_target_unvalidated, save_oauth_credentials,
    OAuthAuthorizationRequest, OAuthCallbackParams, OAuthRefreshRequest, OAuthTokenExchangeRequest,
    OAuthTokenSet, PkceChallengeMethod,
};
use serde_json::json;

static COUNTER: AtomicU64 = AtomicU64::new(0);

#[allow(clippy::cast_possible_truncation)]
fn unique_id() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    COUNTER.fetch_add(1, Ordering::Relaxed) + nanos as u64
}

fn with_isolated_config<F, R>(f: F) -> R
where
    F: FnOnce(&PathBuf) -> R,
{
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    let guard = LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let dir = std::env::temp_dir().join(format!("oauth-e2e-{}", unique_id()));
    fs::create_dir_all(&dir).unwrap();
    std::env::set_var("CLAW_CONFIG_HOME", &dir);
    let result = f(&dir);
    std::env::remove_var("CLAW_CONFIG_HOME");
    let _ = fs::remove_dir_all(&dir);
    drop(guard);
    result
}

#[test]
fn pkce_s256_challenge_is_deterministic() {
    let pair = generate_pkce_pair().unwrap();
    assert_eq!(pair.challenge_method, PkceChallengeMethod::S256);
    assert!(!pair.verifier.is_empty());
    assert!(!pair.challenge.is_empty());
    assert_ne!(pair.verifier, pair.challenge);
}

#[test]
fn authorization_url_contains_all_required_params() {
    let pkce = generate_pkce_pair().unwrap();
    let state = generate_state().unwrap();
    let req = OAuthAuthorizationRequest {
        authorize_url: "https://auth.example.com/authorize".to_string(),
        client_id: "test-client".to_string(),
        redirect_uri: loopback_redirect_uri(8080),
        scopes: vec!["openid".to_string(), "profile".to_string()],
        state: state.clone(),
        code_challenge: pkce.challenge.clone(),
        code_challenge_method: pkce.challenge_method,
        extra_params: BTreeMap::new(),
    };
    let url_str = req.build_url();
    assert!(
        url_str.contains("response_type=code"),
        "missing response_type: {}",
        url_str
    );
    assert!(
        url_str.contains("client_id=test-client"),
        "missing client_id: {}",
        url_str
    );
    assert!(
        url_str.contains(&format!("state={}", state)),
        "missing state: {}",
        url_str
    );
    assert!(
        url_str.contains("code_challenge="),
        "missing code_challenge: {}",
        url_str
    );
    assert!(
        url_str.contains("code_challenge_method=S256"),
        "missing method: {}",
        url_str
    );
    assert!(
        url_str.contains("redirect_uri="),
        "missing redirect_uri: {}",
        url_str
    );
    assert!(url_str.contains("scope="), "missing scope: {}", url_str);
}

#[test]
fn callback_parsing_success_and_error() {
    let success = parse_oauth_callback_query("code=abc123&state=xyz789").unwrap();
    assert_eq!(success.code, Some("abc123".to_string()));
    assert_eq!(success.state, Some("xyz789".to_string()));
    assert!(success.error.is_none());

    let error =
        parse_oauth_callback_query("error=access_denied&error_description=User+denied+access")
            .unwrap();
    assert!(error.code.is_none());
    assert_eq!(error.error, Some("access_denied".to_string()));
    assert_eq!(
        error.error_description,
        Some("User denied access".to_string())
    );

    let from_target =
        parse_oauth_callback_request_target_unvalidated("/callback?code=def&state=ghi").unwrap();
    assert_eq!(from_target.code, Some("def".to_string()));
    assert_eq!(from_target.state, Some("ghi".to_string()));
}

#[test]
fn token_exchange_request_form_params() {
    let req = OAuthTokenExchangeRequest {
        grant_type: "authorization_code",
        code: "auth-code-123".to_string(),
        redirect_uri: "http://localhost:8080/callback".to_string(),
        client_id: "test-client".to_string(),
        code_verifier: "verifier-abc".to_string(),
        state: "state-xyz".to_string(),
    };
    let params = req.form_params();
    assert_eq!(params.get("grant_type").unwrap(), "authorization_code");
    assert_eq!(params.get("code").unwrap(), "auth-code-123");
    assert_eq!(
        params.get("redirect_uri").unwrap(),
        "http://localhost:8080/callback"
    );
    assert_eq!(params.get("client_id").unwrap(), "test-client");
    assert_eq!(params.get("code_verifier").unwrap(), "verifier-abc");
    assert_eq!(params.get("state").unwrap(), "state-xyz");
}

#[test]
fn token_refresh_request_form_params() {
    let req = OAuthRefreshRequest {
        grant_type: "refresh_token",
        refresh_token: "rt-123".to_string(),
        client_id: "test-client".to_string(),
        scopes: vec!["openid".to_string()],
    };
    let params = req.form_params();
    assert_eq!(params.get("grant_type").unwrap(), "refresh_token");
    assert_eq!(params.get("refresh_token").unwrap(), "rt-123");
    assert_eq!(params.get("client_id").unwrap(), "test-client");
}

#[test]
fn credential_persistence_round_trip() {
    with_isolated_config(|_dir| {
        let token = OAuthTokenSet {
            access_token: "at-123".to_string(),
            refresh_token: Some("rt-456".to_string()),
            expires_at: Some(9_999_999_999),
            scopes: vec!["openid".to_string()],
        };
        save_oauth_credentials(&token).unwrap();
        let loaded = load_oauth_credentials().unwrap().expect("should load");
        assert_eq!(loaded.access_token, "at-123");
        assert_eq!(loaded.refresh_token, Some("rt-456".to_string()));
        assert_eq!(loaded.expires_at, Some(9_999_999_999));
        assert_eq!(loaded.scopes, vec!["openid"]);
    });
}

#[test]
fn credential_persistence_preserves_other_keys() {
    with_isolated_config(|_dir| {
        let path = credentials_path().unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"other_key":"preserved","oauth":null}"#).unwrap();

        save_oauth_credentials(&OAuthTokenSet {
            access_token: "new-at".to_string(),
            refresh_token: None,
            expires_at: None,
            scopes: vec![],
        })
        .unwrap();

        let raw: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["other_key"], "preserved");
        assert!(raw["oauth"]["accessToken"].is_string());
    });
}

#[test]
fn clear_credentials_removes_only_oauth() {
    with_isolated_config(|_dir| {
        save_oauth_credentials(&OAuthTokenSet {
            access_token: "at".to_string(),
            refresh_token: None,
            expires_at: None,
            scopes: vec![],
        })
        .unwrap();
        let path = credentials_path().unwrap();
        let mut root =
            serde_json::from_str::<serde_json::Value>(&fs::read_to_string(&path).unwrap()).unwrap();
        root.as_object_mut()
            .unwrap()
            .insert("keep_me".to_string(), json!(true));
        fs::write(&path, root.to_string()).unwrap();

        clear_oauth_credentials().unwrap();

        let after =
            serde_json::from_str::<serde_json::Value>(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            !after.as_object().unwrap().contains_key("oauth"),
            "oauth should be removed"
        );
        assert_eq!(after["keep_me"], true);
    });
}

#[test]
fn load_credentials_returns_none_when_no_file() {
    with_isolated_config(|_dir| {
        let result = load_oauth_credentials().unwrap();
        assert!(
            result.is_none(),
            "should be None when no file: {:?}",
            result
        );
    });
}
