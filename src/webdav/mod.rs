//! OpenList-oriented WebDAV. Locations stay as connection id plus resource path.
#![allow(dead_code, unused_imports)]
mod credentials;
mod download;
mod http;
mod path;
mod slots;
#[cfg(test)]
mod tests;
mod xml;

pub use credentials::{delete as delete_password, load as load_password, store as store_password};
pub use download::{TreeProgress, download_tree};
pub use http::{
    DownloadFinish, Endpoint, Failure, ListFinish, RedirectReject, decide_redirect, download_file,
    list_children, open_file_body, redact_url,
};
pub use path::{PathError, ResourcePath, endpoint_url};
pub use slots::{SlotGuard, SlotPool};
pub use xml::ListedItem;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryIdentity {
    pub connection_id: u64,
    pub revision: u64,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    Name,
    Host,
    Port,
    Root,
    Username,
}

pub fn connection_id(
    https: bool,
    host: &str,
    port: u16,
    root: &ResourcePath,
    username: &str,
) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in format!(
        "{}|{}|{}|{}|{}",
        u8::from(https),
        host.to_ascii_lowercase(),
        port,
        root.canonical(),
        username
    )
    .bytes()
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash.max(1)
}

pub fn validate_endpoint(
    display_name: &str,
    https: bool,
    host: &str,
    port: u16,
    root: &str,
    username: &str,
    revision: u64,
) -> Result<Endpoint, ConfigError> {
    if display_name.trim().is_empty() {
        return Err(ConfigError::Name);
    }
    if username.trim().is_empty() {
        return Err(ConfigError::Username);
    }
    if port == 0 {
        return Err(ConfigError::Port);
    }
    let root = ResourcePath::parse_root(root).map_err(|_| ConfigError::Root)?;
    if endpoint_url(https, host.trim(), port, &root, true).is_err() {
        return Err(ConfigError::Host);
    }
    let host = host.trim().to_owned();
    let username = username.trim().to_owned();
    Ok(Endpoint {
        connection_id: connection_id(https, &host, port, &root, &username),
        revision: revision.max(1),
        https,
        host,
        port,
        root,
        username,
        display_name: display_name.trim().to_owned(),
    })
}
