//! In-memory cache of the newest captures in the output directory.
//!
//! The directory is the source of truth: the cache is seeded by a disk scan, keeps session captures
//! at the front, and drops entries whose files disappeared.

use crate::storage::{self, CaptureItem};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const LIMIT: usize = 8;

pub struct RecentCaptureCache {
    directory: Option<PathBuf>,
    items: VecDeque<CaptureItem>,
    refreshed: bool,
}

impl Default for RecentCaptureCache {
    fn default() -> Self {
        Self {
            directory: None,
            items: VecDeque::with_capacity(LIMIT),
            refreshed: false,
        }
    }
}

impl RecentCaptureCache {
    pub fn invalidate(&mut self) {
        *self = Self::default();
    }

    pub fn push(&mut self, item: CaptureItem) {
        let Some((path, item)) = normalized_item(item) else {
            return;
        };
        let Some(directory) = path.parent().map(Path::to_path_buf) else {
            return;
        };
        if !self.directory_matches(&directory) {
            self.directory = Some(directory);
            self.items.clear();
        }
        self.items
            .retain(|existing| !item_has_path(existing, &path));
        self.items.push_back(item);
        sort_and_limit(&mut self.items);
        // A capture can arrive before the first disk scan; it must not make an incomplete cache
        // authoritative for this directory.
        self.refreshed = false;
    }

    pub fn refresh(&mut self, directory: &Path, scanned: Vec<CaptureItem>) {
        let directory = storage::normalize_path(directory);
        let cached = if self.directory_matches(&directory) {
            std::mem::take(&mut self.items)
        } else {
            VecDeque::new()
        };

        let mut items = VecDeque::with_capacity(LIMIT * 2);
        for item in cached.into_iter().chain(scanned) {
            let Some((path, item)) = normalized_item(item) else {
                continue;
            };
            let fresh = !items.iter().any(|existing| item_has_path(existing, &path));
            if fresh && is_live_file_in(&path, &directory) {
                items.push_back(item);
            }
        }

        sort_and_limit(&mut items);
        self.directory = Some(directory);
        self.items = items;
        self.refreshed = true;
    }

    pub fn snapshot(&mut self, directory: &Path) -> Option<Vec<CaptureItem>> {
        let directory = storage::normalize_path(directory);
        if !self.refreshed || !self.directory_matches(&directory) {
            return None;
        }
        self.items
            .retain(|item| is_live_file_in(Path::new(&item.path), &directory));
        Some(self.items.iter().cloned().collect())
    }

    pub fn remove(&mut self, path: &Path) {
        let path = storage::normalize_path(path);
        self.items.retain(|item| !item_has_path(item, &path));
    }

    fn directory_matches(&self, directory: &Path) -> bool {
        self.directory
            .as_deref()
            .is_some_and(|cached| storage::paths_match(cached, directory))
    }
}

/// Returns the cached recents for `directory`, scanning the disk with `scan` on a cache miss.
pub fn load<F>(
    cache: &Mutex<RecentCaptureCache>,
    directory: &Path,
    scan: F,
) -> Result<Vec<CaptureItem>, String>
where
    F: FnOnce(&Path, usize) -> Vec<CaptureItem>,
{
    if let Some(items) = crate::lock(cache, "recent captures")?.snapshot(directory) {
        return Ok(items);
    }
    // An unreachable folder (a disconnected share) is not cached as "no snips": the next request
    // looks again.
    let readable = directory.is_dir();
    let scanned = scan(directory, LIMIT);
    if !readable {
        return Ok(scanned);
    }
    let mut cache = crate::lock(cache, "recent captures")?;
    cache.refresh(directory, scanned);
    Ok(cache.snapshot(directory).unwrap_or_default())
}

fn normalized_item(mut item: CaptureItem) -> Option<(PathBuf, CaptureItem)> {
    let path = storage::normalize_path(Path::new(&item.path));
    path.parent()?;
    item.path = path.to_string_lossy().into_owned();
    Some((path, item))
}

fn is_live_file_in(path: &Path, directory: &Path) -> bool {
    path.parent()
        .is_some_and(|parent| storage::paths_match(parent, directory))
        && path.is_file()
}

fn item_has_path(item: &CaptureItem, path: &Path) -> bool {
    storage::paths_match(Path::new(&item.path), path)
}

fn sort_and_limit(items: &mut VecDeque<CaptureItem>) {
    items.make_contiguous().sort_by(|left, right| {
        right
            .timestamp
            .cmp(&left.timestamp)
            .then_with(|| left.path.cmp(&right.path))
    });
    items.truncate(LIMIT);
}

#[cfg(test)]
mod tests {
    use super::{load, RecentCaptureCache};
    use crate::storage::{self, CaptureItem};
    use image::{Rgba, RgbaImage};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    static TEST_DIRECTORY_ID: AtomicU64 = AtomicU64::new(0);

    fn test_directory(prefix: &str) -> PathBuf {
        let id = TEST_DIRECTORY_ID.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!("{prefix}-{}-{id}", std::process::id()));
        std::fs::create_dir(&directory).unwrap_or_else(|error| panic!("{error}"));
        directory
    }

    fn item(path: &std::path::Path, width: u32, timestamp: u64) -> CaptureItem {
        CaptureItem {
            path: path.to_string_lossy().into_owned(),
            name: path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            width,
            height: 1,
            timestamp,
            thumbnail: String::new(),
        }
    }

    #[test]
    fn cache_hit_survives_extended_length_prefix() {
        let directory = test_directory("snippy-cached-recents");
        let capture_path = directory.join("capture.png");
        std::fs::write(&capture_path, []).unwrap_or_else(|error| panic!("{error}"));
        let extended_directory = PathBuf::from(format!(r"\\?\{}", directory.display()));
        let mut cache = RecentCaptureCache::default();
        cache.refresh(&extended_directory, vec![item(&capture_path, 1, 0)]);
        let cache = Mutex::new(cache);

        let items = load(&cache, &extended_directory, |_, _| {
            panic!("expected a cache hit")
        })
        .unwrap_or_else(|error| panic!("{error}"));

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].path, capture_path.to_string_lossy());
        std::fs::remove_dir_all(&directory).unwrap_or_else(|error| panic!("{error}"));
    }

    #[test]
    fn session_capture_is_merged_with_a_fresh_disk_scan() {
        let directory = test_directory("snippy-session-recents");
        for index in 0..8_u32 {
            RgbaImage::from_pixel(1, 1, Rgba([index as u8, 0, 0, 255]))
                .save(directory.join(format!("older-{index}.png")))
                .unwrap_or_else(|error| panic!("{error}"));
        }
        let session_path = directory.join("session.png");
        RgbaImage::from_pixel(2, 1, Rgba([255, 0, 0, 255]))
            .save(&session_path)
            .unwrap_or_else(|error| panic!("{error}"));
        let mut cache = RecentCaptureCache::default();
        cache.refresh(&directory, Vec::new());
        cache.push(item(&session_path, 2, u64::MAX));
        let cache = Mutex::new(cache);

        let items = load(&cache, &directory, storage::scan_recent_captures)
            .unwrap_or_else(|error| panic!("{error}"));

        assert_eq!(items.len(), 8);
        assert_eq!(items[0].name, "session.png");
        assert!(items.iter().any(|item| item.name.starts_with("older-")));
        assert!(items
            .windows(2)
            .all(|pair| pair[0].timestamp >= pair[1].timestamp));
        std::fs::remove_dir_all(&directory).unwrap_or_else(|error| panic!("{error}"));
    }

    #[test]
    fn removed_items_disappear_from_snapshot() {
        let directory = test_directory("snippy-remove-recents");
        let path = directory.join("gone.png");
        std::fs::write(&path, []).unwrap_or_else(|error| panic!("{error}"));
        let mut cache = RecentCaptureCache::default();
        cache.refresh(&directory, vec![item(&path, 1, 1)]);

        cache.remove(&path);

        assert_eq!(cache.snapshot(&directory).map(|items| items.len()), Some(0));
        std::fs::remove_dir_all(&directory).unwrap_or_else(|error| panic!("{error}"));
    }
}
