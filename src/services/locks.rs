use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

/// Serializes mutations for a keyed Discord resource and releases after every outcome.
#[derive(Default)]
pub struct KeyedLock {
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl KeyedLock {
    pub async fn run<T, F: Future<Output = T>>(&self, key: &str, work: F) -> T {
        let lock = self.locks.lock().unwrap().entry(key.to_string()).or_default().clone();
        let result = {
            let _guard = lock.lock().await;
            work.await
        };
        let mut locks = self.locks.lock().unwrap();
        // Two refs left (map + ours) means no other waiter: drop the entry.
        if locks.get(key).is_some_and(|l| Arc::strong_count(l) <= 2) {
            locks.remove(key);
        }
        result
    }
}
