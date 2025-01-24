/*
Thread-safe in-memory cache, with optional disk persistence

Writing to disk is just for debugging and to avoid using up the free API quota, not for production use
*/

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::path::Path;
use std::fs;
use anyhow;

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub(crate) struct CachedResponse {
    pub response: String,
    pub timestamp: u64,
}

pub(crate) struct WebSearchCache {
    inner: Arc<RwLock<HashMap<String, CachedResponse>>>,
    cache_file: Option<String>,
}

impl WebSearchCache {
    pub fn new() -> Self {
        WebSearchCache {
            inner: Arc::new(RwLock::new(HashMap::new())),
            cache_file: None,
        }
    }

    pub fn with_cache_file<P: AsRef<Path>>(cache_file: P) -> Self {
        let cache_path = cache_file.as_ref().to_string_lossy().to_string();
        let mut cache = Self::new();
        cache.cache_file = Some(cache_path.clone());

        // Try to load existing cache
        if let Ok(contents) = fs::read_to_string(&cache_path) {
            if let Ok(map) = serde_json::from_str(&contents) {
                cache.inner = Arc::new(RwLock::new(map));
            }
        }

        cache
    }

    pub fn set(&self, key: String, value: CachedResponse) {
        let _old_value = self.inner.write().unwrap().insert(key, value);
    }

    pub fn get(&self, key: &str) -> Option<CachedResponse> {
        self.inner.read().unwrap().get(key).cloned()
    }

    pub fn save_to_disk(&self) -> Result<(), anyhow::Error> {
        if let Some(cache_file) = &self.cache_file {
            let guard = self.inner.read().map_err(|e| anyhow::anyhow!("Failed to acquire read lock: {}", e))?;
            let serialized = serde_json::to_string(&*guard)?;
            fs::write(cache_file, serialized)?;
        }
        Ok(())
    }
}

impl Clone for WebSearchCache {
    fn clone(&self) -> Self {
        WebSearchCache {
            inner: Arc::clone(&self.inner),
            cache_file: self.cache_file.clone(),
        }
    }
}
