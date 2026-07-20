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
//! Eviction is approximate-LRU via a monotonic access counter. The important
//! bound is decoded bytes, not file count: a 12 MP JPEG is only a few MB on
//! disk but roughly 48 MB once uploaded as an RGBA texture.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use gtk4::gdk::Texture;
use gtk4::prelude::TextureExt;

const MAX_ENTRIES: usize = 256;
const MAX_DECODED_BYTES: usize = 96 * 1024 * 1024;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct CacheKey {
    path: PathBuf,
    max_dimension: Option<i32>,
}

struct Entry {
    texture: Texture,
    mtime: SystemTime,
    last_used: u64,
    decoded_bytes: usize,
}

struct Cache {
    map: HashMap<CacheKey, Entry>,
    decoded_bytes: usize,
    counter: u64,
    hits: u64,
    misses: u64,
}

impl Cache {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            decoded_bytes: 0,
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
            if let Some(removed) = self.map.remove(&victim) {
                self.decoded_bytes = self.decoded_bytes.saturating_sub(removed.decoded_bytes);
            }
        }
    }
}

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::new(Cache::new());
}

/// Load a full-resolution texture for a short-lived fullscreen viewer.
/// Full images are deliberately not cached: the viewer owns one at a time and
/// dropping it must release those decoded pixels immediately.
pub fn texture_from_filename<P: AsRef<Path>>(path: P) -> Option<Texture> {
    Texture::from_filename(path.as_ref()).ok()
}

/// Load a display-sized texture instead of retaining the source image at full
/// camera resolution. Use this for avatars, message bubbles, grids, and other
/// previews; reserve [`texture_from_filename`] for the fullscreen viewer.
pub fn texture_thumbnail<P: AsRef<Path>>(path: P, max_dimension: i32) -> Option<Texture> {
    load_texture(path.as_ref(), Some(max_dimension.max(1)))
}

fn load_texture(path: &Path, max_dimension: Option<i32>) -> Option<Texture> {
    let path: &Path = path.as_ref();
    let mtime = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());
    let key = CacheKey {
        path: path.to_path_buf(),
        max_dimension,
    };

    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        c.counter += 1;
        let counter = c.counter;

        // Hit if we have an entry whose mtime matches the file on disk.
        let hit = match (c.map.get_mut(&key), mtime) {
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
        let texture = match max_dimension {
            Some(max) => {
                let pixbuf =
                    gtk4::gdk_pixbuf::Pixbuf::from_file_at_scale(path, max, max, true).ok()?;
                Texture::for_pixbuf(&pixbuf)
            }
            None => Texture::from_filename(path).ok()?,
        };
        let decoded_bytes = (texture.width().max(0) as usize)
            .saturating_mul(texture.height().max(0) as usize)
            .saturating_mul(4);
        if let Some(m) = mtime {
            if let Some(stale) = c.map.remove(&key) {
                c.decoded_bytes = c.decoded_bytes.saturating_sub(stale.decoded_bytes);
            }
            // Huge originals are returned to the caller but not retained by
            // the process-wide cache. The fullscreen view owns them only for
            // as long as it is open.
            if decoded_bytes <= MAX_DECODED_BYTES {
                c.decoded_bytes = c.decoded_bytes.saturating_add(decoded_bytes);
                c.map.insert(
                    key,
                    Entry {
                        texture: texture.clone(),
                        mtime: m,
                        last_used: counter,
                        decoded_bytes,
                    },
                );
            }
            while c.map.len() > MAX_ENTRIES || c.decoded_bytes > MAX_DECODED_BYTES {
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
        let mut c = c.borrow_mut();
        let keys: Vec<_> = c
            .map
            .keys()
            .filter(|key| key.path == path.as_ref())
            .cloned()
            .collect();
        for key in keys {
            if let Some(removed) = c.map.remove(&key) {
                c.decoded_bytes = c.decoded_bytes.saturating_sub(removed.decoded_bytes);
            }
        }
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
