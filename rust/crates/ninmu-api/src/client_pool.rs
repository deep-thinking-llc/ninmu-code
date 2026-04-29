use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::providers::anthropic::AnthropicClient;
use crate::providers::ProviderKind;

/// Process-wide key for deduplicating API clients.
///
/// Clients are shared when they have the same provider, auth source, and cache
/// scope. This lets TUI turns, RPC sessions, and agent team members reuse the
/// same HTTP connection pool, `last_request_time`, and `PromptCache`.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct ClientKey {
    pub provider: ProviderKind,
    /// Stable hash of the auth source (API key, bearer token, etc.).
    pub auth_hash: u64,
    /// Scope identifier for cache sharing.
    ///
    /// For single-user sessions this is the `session_id`. For agent teams it
    /// may be a `team_id` so that all agents share global-scope cache entries.
    pub cache_scope_id: String,
}

impl ClientKey {
    #[must_use]
    pub fn new(
        provider: ProviderKind,
        auth_hash: u64,
        cache_scope_id: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            auth_hash,
            cache_scope_id: cache_scope_id.into(),
        }
    }
}

/// A process-wide registry of shareable `AnthropicClient` instances.
///
/// `AnthropicClient` is already `Clone` (all fields are `Clone` or `Arc`), so
/// wrapping it in `Arc` lets multiple threads / runtimes / sessions share the
/// same underlying `last_request_time`, `PromptCache`, and `reqwest::Client`
/// without copying the heavy HTTP state.
#[derive(Debug, Default)]
pub struct ApiClientPool {
    clients: Mutex<HashMap<ClientKey, Arc<AnthropicClient>>>,
}

impl ApiClientPool {
    #[must_use]
    pub fn new() -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// Retrieve an existing client for `key`, or create one via `build` and
    /// store it for subsequent callers.
    pub fn get_or_create(
        &self,
        key: ClientKey,
        build: impl FnOnce() -> AnthropicClient,
    ) -> Arc<AnthropicClient> {
        let mut clients = self
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clients
            .entry(key)
            .or_insert_with(|| Arc::new(build()))
            .clone()
    }

    /// Remove a client from the pool.
    ///
    /// Useful when the auth source changes (e.g. token refresh) and the old
    /// client must not be reused.
    pub fn evict(&self, key: &ClientKey) -> Option<Arc<AnthropicClient>> {
        let mut clients = self
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clients.remove(key)
    }

    /// Number of clients currently held in the pool.
    #[must_use]
    pub fn len(&self) -> usize {
        let clients = self
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clients.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_client() -> AnthropicClient {
        AnthropicClient::new("sk-ant-test-key")
    }

    #[test]
    fn get_or_create_returns_same_arc_for_identical_keys() {
        let pool = ApiClientPool::new();
        let key = ClientKey::new(ProviderKind::Anthropic, 42, "session-1");

        let first = pool.get_or_create(key.clone(), dummy_client);
        let second = pool.get_or_create(key, dummy_client);

        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn get_or_create_returns_different_arc_for_different_keys() {
        let pool = ApiClientPool::new();
        let key_a = ClientKey::new(ProviderKind::Anthropic, 42, "session-a");
        let key_b = ClientKey::new(ProviderKind::Anthropic, 42, "session-b");

        let client_a = pool.get_or_create(key_a, dummy_client);
        let client_b = pool.get_or_create(key_b, dummy_client);

        assert!(!Arc::ptr_eq(&client_a, &client_b));
    }

    #[test]
    fn evict_removes_client_from_pool() {
        let pool = ApiClientPool::new();
        let key = ClientKey::new(ProviderKind::Anthropic, 42, "session-1");

        let _ = pool.get_or_create(key.clone(), dummy_client);
        assert_eq!(pool.len(), 1);

        pool.evict(&key);
        assert_eq!(pool.len(), 0);
    }

    #[test]
    fn evict_and_recreate_gives_new_arc() {
        let pool = ApiClientPool::new();
        let key = ClientKey::new(ProviderKind::Anthropic, 42, "session-1");

        let first = pool.get_or_create(key.clone(), dummy_client);
        pool.evict(&key);
        let second = pool.get_or_create(key, dummy_client);

        assert!(!Arc::ptr_eq(&first, &second));
    }
}
