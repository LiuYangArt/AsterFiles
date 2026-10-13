//! Directory downloads enumerate one level at a time and only delete files this task created.
use std::{
    collections::{HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
};

use crate::webdav::{
    http::{self, DownloadFinish, Endpoint, Failure},
    path::ResourcePath,
    xml::ListedItem,
};

const PENDING_DIRECTORIES: usize = 1024;
const VISITED_DIRECTORIES: usize = 100_000;

pub struct TreeProgress {
    pub bytes: u64,
    pub current: String,
}

pub fn download_tree(
    endpoint: &Endpoint,
    password: &str,
    source: &ResourcePath,
    destination_parent: &Path,
    cancel: &Arc<AtomicBool>,
    on_progress: &mut dyn FnMut(TreeProgress),
) -> Result<DownloadFinish, Failure> {
    let name = source.name().ok_or(Failure::Protocol("missing_name"))?;
    validate_stored_name(name)?;
    let destination = destination_parent.join(name);
    if destination.exists() {
        return Err(Failure::Conflict);
    }
    std::fs::create_dir(&destination).map_err(storage_or_network)?;
    let mut created = vec![destination.clone()];
    let mut pending = VecDeque::from([(source.clone(), destination.clone())]);
    let mut visited = HashSet::from([source.canonical()]);
    let mut bytes = 0_u64;
    let mut partial = Vec::new();
    while let Some((remote, local)) = pending.pop_front() {
        if cancel.load(std::sync::atomic::Ordering::Acquire) {
            remove_empty_created(&created);
            return Err(Failure::Cancelled);
        }
        let listed = match http::list_children(endpoint, password, &remote, cancel, &mut |items| {
            for item in items {
                match store_child(
                    endpoint,
                    password,
                    &item,
                    &local,
                    cancel,
                    &mut Walk {
                        pending: &mut pending,
                        visited: &mut visited,
                        created: &mut created,
                        bytes: &mut bytes,
                        on_progress,
                    },
                ) {
                    Ok(()) => {}
                    Err(Failure::Cancelled) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::Interrupted,
                            "cancelled",
                        ));
                    }
                    Err(error) => partial.push(format!("{}:{}", item.name, error.code())),
                }
            }
            Ok(())
        }) {
            Ok(finish) => finish,
            Err(Failure::Cancelled) => {
                remove_empty_created(&created);
                return Err(Failure::Cancelled);
            }
            Err(error) => {
                partial.push(format!("{}:{}", remote.canonical(), error.code()));
                continue;
            }
        };
        if let Some(reason) = listed.incomplete {
            partial.push(format!("{}:{reason}", remote.canonical()));
        }
    }
    if !partial.is_empty() {
        return Err(Failure::Incomplete(partial.join(";")));
    }
    Ok(DownloadFinish {
        path: destination_parent.join(name),
        bytes,
        length_known: false,
    })
}

struct Walk<'a> {
    pending: &'a mut VecDeque<(ResourcePath, PathBuf)>,
    visited: &'a mut HashSet<String>,
    created: &'a mut Vec<PathBuf>,
    bytes: &'a mut u64,
    on_progress: &'a mut dyn FnMut(TreeProgress),
}

fn store_child(
    endpoint: &Endpoint,
    password: &str,
    item: &ListedItem,
    local_parent: &Path,
    cancel: &Arc<AtomicBool>,
    walk: &mut Walk<'_>,
) -> Result<(), Failure> {
    validate_stored_name(&item.name)?;
    if !item.path.starts_with(&endpoint.root) {
        return Err(Failure::Protocol("outside_root"));
    }
    let local = local_parent.join(&item.name);
    if item.is_directory {
        let canonical = item.path.canonical();
        if !walk.visited.insert(canonical) {
            return Ok(());
        }
        if walk.visited.len() > VISITED_DIRECTORIES || walk.pending.len() >= PENDING_DIRECTORIES {
            return Err(Failure::Protocol("directory_limit"));
        }
        if local.exists() {
            return Err(Failure::Conflict);
        }
        std::fs::create_dir(&local).map_err(storage_or_network)?;
        walk.created.push(local.clone());
        walk.pending.push_back((item.path.clone(), local));
        return Ok(());
    }
    if local.exists() {
        return Err(Failure::Conflict);
    }
    let before = *walk.bytes;
    let finish = http::download_file(
        endpoint,
        password,
        &item.path,
        &local,
        cancel,
        &mut |count, _total| {
            (walk.on_progress)(TreeProgress {
                bytes: before.saturating_add(count),
                current: item.name.clone(),
            });
        },
    )?;
    *walk.bytes = before.saturating_add(finish.bytes);
    Ok(())
}

fn validate_stored_name(name: &str) -> Result<(), Failure> {
    crate::fs::file_operations::validate_name(std::ffi::OsStr::new(name))
        .map_err(|_| Failure::InvalidName)
}

fn storage_or_network(error: std::io::Error) -> Failure {
    if error.raw_os_error() == Some(112) || error.kind() == std::io::ErrorKind::StorageFull {
        Failure::InsufficientSpace
    } else {
        Failure::Network
    }
}

fn remove_empty_created(created: &[PathBuf]) {
    for directory in created.iter().rev() {
        let _ = std::fs::remove_dir(directory);
    }
}
