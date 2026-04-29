//! Process-wide cache for `GdkTexture` objects loaded from disk.
//!
//! Without this, every bubble that renders an avatar or thumbnail calls
//! `gtk4::gdk::Texture::from_filename`, which re-opens the file, re-decodes
//! the image, and uploads a fresh pixel buffer to the GPU — even when a
//! group chat shows the same sender's avatar 50 times in a row.
//!
//! The cache is keyed by (canonical path, mtime). The mtime check means
//! that when a fresh avatar lands at the same path the next request reloads
//! it instead of serving the stale texture. No explicit invalidation is
//! needed for the common "file was overwritten" case.
//!
//! Lives on the GTK main thread (thread_local) — `GdkTexture` is reference
//! counted by the toolkit, so handing out clones is just a pointer bump.
//! Eviction is approximate-LRU via a monotonic access counter; bounded at
//! `MAX_ENTRIES` to keep RAM predictable on long sessions.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use gtk4::gdk::Texture;

const MAX_ENTRIES: usize = 1024;

struct Entry {
    texture: Texture,
    mtime: SystemTime,
    last_used: u64,
}

struct Cache {
    map: HashMap<PathBuf, Entry>,
    counter: u64,
    hits: u64,
    misses: u64,
}

impl Cache {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            counter: 0,
            hits: 0,
            misses: 0,
        }
    }

    fn evict_one(&mut self) {
        if let Some(victim) = self
            .map
            .iter()
            .min_by_key(|(_, e)| e.last_used)
            .map(|(k, _)| k.clone())
        {
            self.map.remove(&victim);
        }
    }
}

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::new(Cache::new());
}

/// Return a `GdkTexture` for `path`, reusing a cached copy when the file's
/// mtime matches. Falls back to fresh `Texture::from_filename` on any error.
/// Returns `None` if the file can't be read or decoded.
pub fn texture_from_filename<P: AsRef<Path>>(path: P) -> Option<Texture> {
    let path = path.as_ref();
    let mtime = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());

    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        c.counter += 1;
        let counter = c.counter;

        // Hit if we have an entry whose mtime matches the file on disk.
        let hit = match (c.map.get_mut(path), mtime) {
            (Some(e), Some(m)) if e.mtime == m => {
                e.last_used = counter;
                Some(e.texture.clone())
            }
            _ => None,
        };
        if let Some(tex) = hit {
            c.hits += 1;
            return Some(tex);
        }

        c.misses += 1;
        let texture = Texture::from_filename(path).ok()?;
        if let Some(m) = mtime {
            c.map.insert(
                path.to_path_buf(),
                Entry {
                    texture: texture.clone(),
                    mtime: m,
                    last_used: counter,
                },
            );
            if c.map.len() > MAX_ENTRIES {
                c.evict_one();
            }
        }
        Some(texture)
    })
}

/// Drop the entry for `path`, if any. Use this when you know the file
/// at `path` was just rewritten and you want the next render to pick
/// up the new content immediately without waiting on the mtime check.
pub fn invalidate<P: AsRef<Path>>(path: P) {
    CACHE.with(|c| {
        c.borrow_mut().map.remove(path.as_ref());
    });
}

/// Diagnostic: (entries, hits, misses). Useful for one-shot logging.
#[allow(dead_code)]
pub fn stats() -> (usize, u64, u64) {
    CACHE.with(|c| {
        let c = c.borrow();
        (c.map.len(), c.hits, c.misses)
    })
}
