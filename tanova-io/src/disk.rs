//! Crash-proof EXR disk cache with one background loader.
//!
//! A viewer scrub can request the same frame dozens of times while the file is
//! still on disk. [`DiskCache`] collapses those requests into a single load:
//! the first caller marks the path `Loading` and queues it, every later caller
//! observes `Loading` and returns `None`. Exactly one I/O thread drains the
//! queue, so a missing or corrupt file can only ever fail one load.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use crossbeam_channel::{Sender, unbounded};
use tanova_kernel::{KernelMessage, MemoryGovernor, NodeError};
use tanova_render::Raster2D;

use crate::exr::load_exr;

/// Maximum number of successfully loaded frames kept in memory.
pub const CACHE_CAPACITY: usize = 32;

/// The state of one path in the cache.
#[derive(Debug, Clone)]
pub enum CacheEntry {
    /// A worker is loading this path right now.
    Loading,
    /// The pixels are available.
    Ready(Arc<Raster2D>),
    /// The file was missing, corrupt, or denied by the governor.
    Failed,
}

/// A read-only snapshot of one path's state.
#[derive(Debug, Clone)]
pub enum CacheStatus {
    /// The path has never been requested.
    Missing,
    /// A worker is loading the path.
    Loading,
    /// The pixels are available.
    Ready(Arc<Raster2D>),
    /// The last load attempt failed.
    Failed,
}

struct DiskCacheState {
    map: HashMap<String, CacheEntry>,
    history: VecDeque<String>,
}

struct LoadRequest {
    path: String,
    governor: Option<MemoryGovernor>,
}

type Loader = dyn Fn(&str, Option<&MemoryGovernor>) -> Result<Raster2D, NodeError> + Send + Sync;

/// Shared frame cache backed by exactly one background I/O thread.
#[derive(Clone)]
pub struct DiskCache {
    state: Arc<Mutex<DiskCacheState>>,
    requests: Sender<LoadRequest>,
}

impl DiskCache {
    /// Creates a cache whose worker loads real EXR files.
    ///
    /// `kernel_tx` receives a [`KernelMessage::TriggerRender`] after every
    /// completed load so the viewer can pick up newly ready pixels.
    pub fn new(kernel_tx: Sender<KernelMessage>) -> Self {
        Self::with_loader(kernel_tx, load_exr)
    }

    /// Creates a cache with an injectable loader.
    ///
    /// This is primarily a test seam; production code uses [`DiskCache::new`].
    pub fn with_loader<F>(kernel_tx: Sender<KernelMessage>, loader: F) -> Self
    where
        F: Fn(&str, Option<&MemoryGovernor>) -> Result<Raster2D, NodeError> + Send + Sync + 'static,
    {
        let (requests, receiver) = unbounded::<LoadRequest>();
        let state = Arc::new(Mutex::new(DiskCacheState {
            map: HashMap::new(),
            history: VecDeque::new(),
        }));

        let worker_state = Arc::clone(&state);
        let loader: Arc<Loader> = Arc::new(loader);

        let _ = thread::Builder::new()
            .name("tanova-io".to_string())
            .spawn(move || {
                while let Ok(request) = receiver.recv() {
                    // A cartridge bug must not kill the only I/O thread.
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        loader(&request.path, request.governor.as_ref())
                    }));

                    let mut state = lock_state(&worker_state);
                    match result {
                        Ok(Ok(raster)) => {
                            state
                                .map
                                .insert(request.path.clone(), CacheEntry::Ready(Arc::new(raster)));
                            state.history.push_back(request.path.clone());
                            while state.history.len() > CACHE_CAPACITY {
                                let Some(oldest) = state.history.pop_front() else {
                                    break;
                                };
                                state.map.remove(&oldest);
                            }
                        }
                        Ok(Err(_)) | Err(_) => {
                            state.map.insert(request.path.clone(), CacheEntry::Failed);
                        }
                    }
                    drop(state);

                    let _ = kernel_tx.send(KernelMessage::TriggerRender);
                }
            });

        Self { state, requests }
    }

    /// Returns cached pixels, or queues a load and returns `None`.
    ///
    /// A second request for a path that is already `Loading` or `Failed`
    /// returns `None` without queuing another load. This is the anti-thundering
    /// herd guarantee.
    pub fn get_or_load(
        &self,
        path: String,
        governor: Option<MemoryGovernor>,
    ) -> Option<Arc<Raster2D>> {
        let mut state = lock_state(&self.state);
        match state.map.get(&path) {
            Some(CacheEntry::Ready(raster)) => return Some(Arc::clone(raster)),
            Some(CacheEntry::Loading) | Some(CacheEntry::Failed) => return None,
            None => {}
        }

        state.map.insert(path.clone(), CacheEntry::Loading);
        drop(state);

        if self
            .requests
            .send(LoadRequest {
                path: path.clone(),
                governor,
            })
            .is_err()
        {
            let mut state = lock_state(&self.state);
            state.map.insert(path, CacheEntry::Failed);
        }

        None
    }

    /// Snapshots the current state of one path.
    pub fn status(&self, path: &str) -> CacheStatus {
        let state = lock_state(&self.state);
        match state.map.get(path) {
            Some(CacheEntry::Loading) => CacheStatus::Loading,
            Some(CacheEntry::Ready(raster)) => CacheStatus::Ready(Arc::clone(raster)),
            Some(CacheEntry::Failed) => CacheStatus::Failed,
            None => CacheStatus::Missing,
        }
    }

    /// Returns `true` if the last load for `path` failed.
    pub fn is_failed(&self, path: &str) -> bool {
        matches!(self.status(path), CacheStatus::Failed)
    }

    /// Returns `true` if `path` is currently cached in any state.
    pub fn contains(&self, path: &str) -> bool {
        lock_state(&self.state).map.contains_key(path)
    }

    /// Number of tracked paths (including `Loading` and `Failed`).
    pub fn len(&self) -> usize {
        lock_state(&self.state).map.len()
    }

    /// Returns `true` when no path is tracked.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn lock_state(state: &Mutex<DiskCacheState>) -> MutexGuard<'_, DiskCacheState> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use crossbeam_channel::unbounded;
    use tanova_render::{BBox, EdgeMode, Format};

    use super::*;

    fn empty_raster() -> Raster2D {
        Raster2D::new(
            Format::new("test", 1, 1, 1.0),
            BBox::new(0, 0, 1, 1),
            EdgeMode::BlackOutside,
        )
    }

    fn wait_until(mut predicate: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if predicate() {
                return true;
            }
            thread::sleep(Duration::from_millis(1));
        }
        predicate()
    }

    fn wait_ready(cache: &DiskCache, path: &str) {
        assert!(
            wait_until(|| matches!(cache.status(path), CacheStatus::Ready(_))),
            "`{path}` never became ready"
        );
    }

    #[test]
    fn thundering_herd_loads_only_once() {
        let (kernel_tx, _kernel_rx) = unbounded();
        let loads = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&loads);

        let cache = DiskCache::with_loader(kernel_tx, move |_path, _governor| {
            counter.fetch_add(1, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(40));
            Ok(empty_raster())
        });

        assert!(cache.get_or_load("a.exr".to_string(), None).is_none());
        assert!(cache.get_or_load("a.exr".to_string(), None).is_none());
        assert!(matches!(cache.status("a.exr"), CacheStatus::Loading));

        wait_ready(&cache, "a.exr");
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        assert!(cache.get_or_load("a.exr".to_string(), None).is_some());
    }

    #[test]
    fn evicts_the_oldest_frame_at_capacity() {
        let (kernel_tx, _kernel_rx) = unbounded();
        let cache = DiskCache::with_loader(kernel_tx, |_path, _governor| Ok(empty_raster()));

        let mut paths = Vec::new();
        for index in 0..(CACHE_CAPACITY + 1) {
            let path = format!("frame_{index:04}.exr");
            paths.push(path.clone());
            let _ = cache.get_or_load(path.clone(), None);
            wait_ready(&cache, &path);
        }

        assert_eq!(cache.len(), CACHE_CAPACITY);
        assert!(!cache.contains(&paths[0]), "oldest frame should be evicted");
        assert!(cache.contains(&paths[CACHE_CAPACITY]));
    }

    #[test]
    fn missing_file_becomes_failed_without_panicking() {
        let (kernel_tx, _kernel_rx) = unbounded();
        let cache = DiskCache::new(kernel_tx);

        let path = std::env::temp_dir().join(format!(
            "tanova-missing-{}-{}.exr",
            std::process::id(),
            "disk-cache"
        ));
        let path = path.to_string_lossy().into_owned();
        let _ = std::fs::remove_file(&path);

        assert!(cache.get_or_load(path.clone(), None).is_none());
        assert!(wait_until(|| cache.is_failed(&path)));
        assert!(matches!(cache.status(&path), CacheStatus::Failed));
    }

    #[test]
    fn a_panicking_loader_is_contained() {
        let (kernel_tx, _kernel_rx) = unbounded();
        let cache = DiskCache::with_loader(kernel_tx, |_path, _governor| {
            panic!("loader exploded");
        });

        assert!(cache.get_or_load("boom.exr".to_string(), None).is_none());
        assert!(wait_until(|| cache.is_failed("boom.exr")));
    }
}
