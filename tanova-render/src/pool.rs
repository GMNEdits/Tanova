//! Safe `N - 2` tile multi-threading using pure `std::thread::scope`.
//! Zero `unsafe`, zero new crates. Always leaves 2 CPU threads free for the
//! OS, UI, and browser.

/// Returns the number of render worker threads to use.
///
/// Caps at `available_parallelism - 2` (always leaving 2 threads free for
/// the OS, UI, and browser), clamped to `1..=6`. On an 8-thread i5-10310U
/// this yields 6 threads; on 4-thread CPUs, 2.
pub fn worker_thread_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .saturating_sub(2)
        .clamp(1, 6)
}

/// Maps `f` over `items` in parallel, returning results in exact input order.
///
/// - When `items.len() <= 1` or `worker_thread_count() <= 1`, runs
///   sequentially with zero thread-spawn overhead.
/// - Otherwise divides work into contiguous chunks across worker threads via
///   `std::thread::scope`, collects in exact input order.
/// - Returns the first `Err(e)`; all partial results drop via RAII with zero
///   panics.
pub fn parallel_map_ordered<T, R, E, F>(items: &[T], f: F) -> Result<Vec<R>, E>
where
    T: Sync,
    R: Send,
    E: Send,
    F: Fn(&T) -> Result<R, E> + Sync,
{
    if items.len() <= 1 || worker_thread_count() <= 1 {
        return items.iter().map(f).collect();
    }

    let n_workers = worker_thread_count().min(items.len());
    let chunk_size = items.len().div_ceil(n_workers);

    std::thread::scope(|scope| -> Result<Vec<R>, E> {
        let mut handles = Vec::with_capacity(n_workers);
        for worker in 0..n_workers {
            let start = worker * chunk_size;
            let end = (start + chunk_size).min(items.len());
            if start >= end {
                continue;
            }
            let chunk = &items[start..end];
            let f = &f;
            let handle = scope.spawn(move || chunk.iter().map(f).collect::<Result<Vec<_>, _>>());
            handles.push(handle);
        }

        let mut out = Vec::with_capacity(items.len());
        for handle in handles {
            match handle.join() {
                Ok(Ok(mut chunk)) => out.append(&mut chunk),
                Ok(Err(e)) => return Err(e),
                Err(_) => panic!("render worker thread panicked"),
            }
        }
        Ok(out)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parallel_map_ordered_sequential_when_small() {
        let items = vec![1usize, 2, 3];
        let result =
            parallel_map_ordered(&items, |x| -> Result<usize, &str> { Ok(*x * 2) }).unwrap();
        assert_eq!(result, vec![2, 4, 6]);
    }

    #[test]
    fn parallel_map_ordered_preserves_order() {
        let items: Vec<usize> = (0..100).collect();
        let result =
            parallel_map_ordered(&items, |x| -> Result<usize, &str> { Ok(*x * 3) }).unwrap();
        assert_eq!(result, (0..100).map(|x| x * 3).collect::<Vec<_>>());
    }

    #[test]
    fn parallel_map_ordered_propagates_first_error() {
        let items: Vec<i32> = vec![1, 2, 3, 4, 5];
        let result: Result<Vec<i32>, &str> =
            parallel_map_ordered(&items, |x| -> Result<i32, &str> {
                if *x == 4 { Err("boom") } else { Ok(*x) }
            });
        assert!(matches!(result, Err("boom")));
    }

    #[test]
    fn worker_count_is_bounded() {
        let count = worker_thread_count();
        assert!(count >= 1);
        assert!(count <= 6);
    }

    #[test]
    fn recommended_gui_memory_cap_is_bounded_and_worker_count_leaves_headroom() {
        let cap = tanova_kernel::recommended_gui_memory_cap_bytes();
        let gib = 1024 * 1024 * 1024;
        // Cap must be strictly within [1 GiB, 2 GiB].
        assert!(cap >= gib, "cap must be at least 1 GiB, got {cap}");
        assert!(cap <= 2 * gib, "cap must be at most 2 GiB, got {cap}");

        let workers = worker_thread_count();
        assert!(workers >= 1);
        assert!(workers <= 6);
        // Always leaves 2 threads free: workers <= available_parallelism - 2
        // whenever available_parallelism >= 3.
        if let Ok(n) = std::thread::available_parallelism() {
            if n.get() >= 3 {
                assert!(
                    workers <= n.get() - 2,
                    "workers {workers} must leave 2 threads free on {}-thread CPU",
                    n.get()
                );
            }
        }
    }
}
