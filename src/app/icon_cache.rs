//! Shared image storage; preview eviction follows write order, not read access (#163).
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
};

use crate::platform::windows::shell_icons::{ShellIconRgba, ShellTypeIconKey};

type ImageKey = (PathBuf, u32);
const PREVIEW_CACHE_CAPACITY: usize = 128;

pub(super) struct IconCache {
    pub(super) ordinary: HashMap<PathBuf, ShellIconRgba>,
    pub(super) types: HashMap<(ShellTypeIconKey, u32), ShellIconRgba>,
    pub(super) thumbnails: BoundedImages,
    pub(super) grid_icons: BoundedImages,
}

impl Default for IconCache {
    fn default() -> Self {
        Self {
            ordinary: HashMap::new(),
            types: HashMap::new(),
            thumbnails: BoundedImages::new(PREVIEW_CACHE_CAPACITY),
            grid_icons: BoundedImages::new(PREVIEW_CACHE_CAPACITY),
        }
    }
}

impl IconCache {
    pub(super) fn rename_paths(&mut self, old: &Path, new: &Path, retain: bool) {
        self.thumbnails.rename_paths(old, new, retain);
        self.grid_icons.rename_paths(old, new, retain);
        let old_icon = self.ordinary.remove(old);
        self.ordinary.remove(new);
        if retain && let Some(icon) = old_icon {
            self.ordinary.insert(new.to_path_buf(), icon);
        }
    }
}

pub(super) struct BoundedImages {
    images: HashMap<ImageKey, ShellIconRgba>,
    order: VecDeque<ImageKey>,
    capacity: usize,
}

impl BoundedImages {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            images: HashMap::new(),
            order: VecDeque::new(),
            capacity,
        }
    }

    pub(super) fn get(&self, key: &ImageKey) -> Option<&ShellIconRgba> {
        self.images.get(key)
    }

    pub(super) fn snapshot(&self) -> HashMap<ImageKey, ShellIconRgba> {
        self.images.clone()
    }

    pub(super) fn contains_key(&self, key: &ImageKey) -> bool {
        self.images.contains_key(key)
    }

    pub(super) fn insert(&mut self, key: ImageKey, image: ShellIconRgba) {
        if let std::collections::hash_map::Entry::Occupied(mut existing) =
            self.images.entry(key.clone())
        {
            self.order.retain(|candidate| candidate != &key);
            self.order.push_back(key);
            existing.insert(image);
            return;
        }
        while self.images.len() >= self.capacity {
            let Some(oldest) = self.order.pop_front() else {
                self.images.clear();
                break;
            };
            self.images.remove(&oldest);
        }
        self.order.push_back(key.clone());
        self.images.insert(key, image);
    }

    fn rename_paths(&mut self, old: &Path, new: &Path, retain: bool) {
        let keys = self
            .images
            .keys()
            .filter(|(path, _)| path == old || path == new)
            .cloned()
            .collect::<Vec<_>>();
        let images = keys
            .iter()
            .filter_map(|key| {
                let image = self.images.remove(key)?;
                (retain && key.0 == old).then_some(((new.to_path_buf(), key.1), image))
            })
            .collect::<Vec<_>>();
        self.order.retain(|(path, _)| path != old && path != new);
        for (key, image) in images {
            self.insert(key, image);
        }
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.images.len()
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.images.is_empty()
    }
}
