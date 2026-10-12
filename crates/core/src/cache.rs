//! Metadata cache: holds each upstream's last-known `tools/list` result,
//! refreshed lazily on first access and eagerly in response to
//! `notifications/tools/list_changed`. Each upstream gets its own lock, so
//! refreshing one upstream never blocks reads or refreshes of another.
//!
//! This module is deliberately decoupled from `Supervisor`: callers pass a
//! `fetch` closure that performs the actual `tools/list` round-trip. Wiring
//! a supervisor's notification stream into `refresh`/`invalidate` calls is
//! the Gateway's job (a later step).

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex as SyncMutex};

use tokio::sync::Mutex as AsyncMutex;

use mcp_mesh_protocol::mcp::Tool;

use crate::UpstreamId;

type Entry = Arc<AsyncMutex<Option<Vec<Tool>>>>;

/// Per-upstream cache of `tools/list` results.
#[derive(Debug, Default)]
pub struct MetadataCache {
    entries: SyncMutex<HashMap<UpstreamId, Entry>>,
}

impl MetadataCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn entry_for(&self, upstream: &str) -> Entry {
        let mut entries = self.entries.lock().expect("cache mutex is never poisoned");
        entries
            .entry(upstream.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(None)))
            .clone()
    }

    /// Returns the cached tools for `upstream` without fetching, if any.
    pub async fn peek(&self, upstream: &str) -> Option<Vec<Tool>> {
        self.entry_for(upstream).lock().await.clone()
    }

    /// Returns the cached tools for `upstream`, fetching and caching them
    /// via `fetch` only if nothing is cached yet.
    pub async fn get_or_fetch<F, Fut, E>(&self, upstream: &str, fetch: F) -> Result<Vec<Tool>, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Vec<Tool>, E>>,
    {
        let entry = self.entry_for(upstream);
        let mut guard = entry.lock().await;
        if let Some(tools) = guard.as_ref() {
            return Ok(tools.clone());
        }
        let tools = fetch().await?;
        *guard = Some(tools.clone());
        Ok(tools)
    }

    /// Unconditionally re-fetches `upstream`'s tools (e.g. in response to a
    /// `listChanged` notification) and replaces the cached entry on success.
    /// A failed fetch leaves any previously cached value intact, so a
    /// transient upstream error doesn't blank out otherwise-good metadata.
    pub async fn refresh<F, Fut, E>(&self, upstream: &str, fetch: F) -> Result<Vec<Tool>, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Vec<Tool>, E>>,
    {
        let entry = self.entry_for(upstream);
        let mut guard = entry.lock().await;
        let tools = fetch().await?;
        *guard = Some(tools.clone());
        Ok(tools)
    }

    /// Drops the cached entry for `upstream` without fetching a
    /// replacement; the next `get_or_fetch` call will repopulate it.
    pub async fn invalidate(&self, upstream: &str) {
        let entry = self.entry_for(upstream);
        *entry.lock().await = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    fn tool(name: &str) -> Tool {
        Tool {
            name: name.to_string(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    async fn fetch_once(calls: Arc<AtomicUsize>, tools: Vec<Tool>) -> Result<Vec<Tool>, &'static str> {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(tools)
    }

    #[tokio::test]
    async fn peek_on_an_unknown_upstream_returns_none() {
        let cache = MetadataCache::new();
        assert_eq!(cache.peek("ghost").await, None);
    }

    #[tokio::test]
    async fn cache_hit_avoids_a_second_fetch() {
        let cache = MetadataCache::new();
        let calls = Arc::new(AtomicUsize::new(0));

        let first = cache
            .get_or_fetch("pg", || fetch_once(calls.clone(), vec![tool("query")]))
            .await
            .unwrap();
        let second = cache
            .get_or_fetch("pg", || fetch_once(calls.clone(), vec![tool("query")]))
            .await
            .unwrap();

        assert_eq!(first, second);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn list_changed_triggers_exactly_one_refresh() {
        let cache = MetadataCache::new();
        let calls = Arc::new(AtomicUsize::new(0));

        // Simulates the cache's reaction to one `listChanged` notification.
        cache
            .refresh("pg", || fetch_once(calls.clone(), vec![tool("query")]))
            .await
            .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn refresh_failure_preserves_the_previous_cached_value() {
        let cache = MetadataCache::new();
        cache
            .refresh("pg", || async { Ok::<_, &'static str>(vec![tool("query")]) })
            .await
            .unwrap();

        let err = cache
            .refresh("pg", || async { Err::<Vec<Tool>, _>("upstream unreachable") })
            .await;
        assert_eq!(err, Err("upstream unreachable"));

        assert_eq!(cache.peek("pg").await, Some(vec![tool("query")]));
    }

    #[tokio::test]
    async fn refresh_on_one_upstream_does_not_affect_another() {
        let cache = MetadataCache::new();
        cache
            .refresh("pg", || async { Ok::<_, &'static str>(vec![tool("query")]) })
            .await
            .unwrap();
        cache
            .refresh("fs", || async { Ok::<_, &'static str>(vec![tool("read")]) })
            .await
            .unwrap();

        cache
            .refresh("pg", || async { Ok::<_, &'static str>(vec![tool("migrate")]) })
            .await
            .unwrap();

        assert_eq!(cache.peek("pg").await, Some(vec![tool("migrate")]));
        assert_eq!(cache.peek("fs").await, Some(vec![tool("read")]));
    }

    #[tokio::test]
    async fn invalidate_forces_a_refetch_on_the_next_access() {
        let cache = MetadataCache::new();
        let calls = Arc::new(AtomicUsize::new(0));

        cache
            .get_or_fetch("pg", || fetch_once(calls.clone(), vec![tool("query")]))
            .await
            .unwrap();
        cache.invalidate("pg").await;
        cache
            .get_or_fetch("pg", || fetch_once(calls.clone(), vec![tool("query")]))
            .await
            .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn refreshing_different_upstreams_runs_concurrently_not_serially() {
        let cache = MetadataCache::new();
        let delay = Duration::from_millis(150);

        let start = Instant::now();
        let (pg, fs) = tokio::join!(
            cache.refresh("pg", || async move {
                tokio::time::sleep(delay).await;
                Ok::<_, &'static str>(vec![tool("query")])
            }),
            cache.refresh("fs", || async move {
                tokio::time::sleep(delay).await;
                Ok::<_, &'static str>(vec![tool("read")])
            }),
        );
        let elapsed = start.elapsed();

        assert!(pg.is_ok() && fs.is_ok());
        assert!(
            elapsed < delay * 2,
            "expected concurrent, non-blocking refreshes, took {elapsed:?}"
        );
    }
}
