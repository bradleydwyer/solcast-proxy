use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tokio::time::Instant;

/// A single cached response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    pub body: String,
    pub content_type: String,
    pub fetched_at: DateTime<Utc>,
}

/// Serializable form for disk persistence (without Instant fields).
#[derive(Debug, Serialize, Deserialize)]
struct DiskCache {
    entries: HashMap<String, CacheEntry>,
}

/// Cache key: (rooftop_id, endpoint_type) serialized as "rooftop_id:endpoint_type".
fn cache_key(rooftop_id: &str, endpoint: &str) -> String {
    format!("{rooftop_id}:{endpoint}")
}

/// In-memory + file-backed cache with TTL and rate limiting.
pub struct ProxyCache {
    entries: RwLock<HashMap<String, CacheEntry>>,
    /// Tracks when we last attempted an upstream fetch per key (for rate limiting).
    last_attempt: RwLock<HashMap<String, Instant>>,
    cache_path: PathBuf,
}

impl ProxyCache {
    /// Create a new cache, loading persisted entries from disk if available.
    pub fn new(cache_dir: &Path) -> Self {
        let cache_path = cache_dir.join("cache.json");
        let entries = Self::load_from_disk(&cache_path).unwrap_or_default();
        let count = entries.len();
        if count > 0 {
            tracing::info!("Loaded {} cache entries from disk", count);
        }
        Self {
            entries: RwLock::new(entries),
            last_attempt: RwLock::new(HashMap::new()),
            cache_path,
        }
    }

    /// Get a cached entry.
    pub async fn get(&self, rooftop_id: &str, endpoint: &str) -> Option<(CacheEntry, i64)> {
        let key = cache_key(rooftop_id, endpoint);
        let entries = self.entries.read().await;
        entries.get(&key).map(|e| {
            let age = Utc::now().signed_duration_since(e.fetched_at).num_seconds();
            (e.clone(), age)
        })
    }

    /// Check if cached entry is fresh (within TTL).
    pub async fn is_fresh(&self, rooftop_id: &str, endpoint: &str, ttl_secs: u64) -> bool {
        let key = cache_key(rooftop_id, endpoint);
        let entries = self.entries.read().await;
        match entries.get(&key) {
            Some(e) => {
                let age = Utc::now().signed_duration_since(e.fetched_at).num_seconds();
                age >= 0 && (age as u64) < ttl_secs
            }
            None => false,
        }
    }

    /// Check if rate limit allows a new upstream fetch.
    pub async fn can_fetch(&self, rooftop_id: &str, endpoint: &str, rate_limit_secs: u64) -> bool {
        let key = cache_key(rooftop_id, endpoint);
        let attempts = self.last_attempt.read().await;
        match attempts.get(&key) {
            Some(last) => last.elapsed().as_secs() >= rate_limit_secs,
            None => true,
        }
    }

    /// Record that we attempted an upstream fetch (for rate limiting).
    pub async fn mark_attempt(&self, rooftop_id: &str, endpoint: &str) {
        let key = cache_key(rooftop_id, endpoint);
        let mut attempts = self.last_attempt.write().await;
        attempts.insert(key, Instant::now());
    }

    /// Store a response in cache and persist to disk.
    pub async fn set(&self, rooftop_id: &str, endpoint: &str, body: String, content_type: String) {
        let key = cache_key(rooftop_id, endpoint);
        let entry = CacheEntry {
            body,
            content_type,
            fetched_at: Utc::now(),
        };
        {
            let mut entries = self.entries.write().await;
            entries.insert(key, entry);
        }
        self.save_to_disk().await;
    }

    /// Number of cached entries.
    pub async fn entry_count(&self) -> usize {
        self.entries.read().await.len()
    }

    async fn save_to_disk(&self) {
        let entries = self.entries.read().await;
        let disk = DiskCache {
            entries: entries.clone(),
        };
        let json = match serde_json::to_string_pretty(&disk) {
            Ok(j) => j,
            Err(e) => {
                tracing::error!("Failed to serialize cache: {}", e);
                return;
            }
        };
        if let Err(e) = tokio::fs::write(&self.cache_path, json).await {
            tracing::error!("Failed to write cache to {}: {}", self.cache_path.display(), e);
        } else {
            tracing::debug!("Cache saved to {}", self.cache_path.display());
        }
    }

    fn load_from_disk(path: &Path) -> Option<HashMap<String, CacheEntry>> {
        let data = std::fs::read_to_string(path).ok()?;
        let disk: DiskCache = serde_json::from_str(&data).ok()?;
        Some(disk.entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_cache_miss_then_hit() {
        let dir = TempDir::new().unwrap();
        let cache = ProxyCache::new(dir.path());

        // Initially empty
        assert!(cache.get("site1", "forecasts").await.is_none());
        assert!(!cache.is_fresh("site1", "forecasts", 7200).await);

        // Insert
        cache.set("site1", "forecasts", "{}".into(), "application/json".into()).await;

        // Now fresh
        assert!(cache.is_fresh("site1", "forecasts", 7200).await);
        let (entry, age) = cache.get("site1", "forecasts").await.unwrap();
        assert_eq!(entry.body, "{}");
        assert!(age < 2);
    }

    #[tokio::test]
    async fn test_rate_limiting() {
        let dir = TempDir::new().unwrap();
        let cache = ProxyCache::new(dir.path());

        // Can fetch initially
        assert!(cache.can_fetch("site1", "forecasts", 9000).await);

        // Mark attempt
        cache.mark_attempt("site1", "forecasts").await;

        // Now rate limited
        assert!(!cache.can_fetch("site1", "forecasts", 9000).await);

        // But with 0s rate limit, can fetch
        assert!(cache.can_fetch("site1", "forecasts", 0).await);
    }

    #[tokio::test]
    async fn test_different_endpoints_independent() {
        let dir = TempDir::new().unwrap();
        let cache = ProxyCache::new(dir.path());

        cache.set("site1", "forecasts", "{\"f\":1}".into(), "application/json".into()).await;

        assert!(cache.is_fresh("site1", "forecasts", 7200).await);
        assert!(!cache.is_fresh("site1", "estimated_actuals", 7200).await);
    }

    #[tokio::test]
    async fn test_disk_persistence() {
        let dir = TempDir::new().unwrap();

        // Write to cache
        {
            let cache = ProxyCache::new(dir.path());
            cache.set("site1", "forecasts", "{\"data\":true}".into(), "application/json".into()).await;
            assert_eq!(cache.entry_count().await, 1);
        }

        // Load from disk in new instance
        {
            let cache = ProxyCache::new(dir.path());
            assert_eq!(cache.entry_count().await, 1);
            let (entry, _) = cache.get("site1", "forecasts").await.unwrap();
            assert_eq!(entry.body, "{\"data\":true}");
        }
    }
}
