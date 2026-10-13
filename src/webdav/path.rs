//! Remote resource identity. Segments are decoded once and never treated as a Windows path.
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};

const URL_SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'+')
    .add(b'/')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'\\')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'\x7f');

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    Empty,
    Relative,
    UserInfo,
    Fragment,
    Encoding,
    DotSegment,
    OutsideRoot,
}

/// Decoded absolute resource path. Comparison is case-sensitive and does not fold separators.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResourcePath {
    segments: Vec<String>,
}

impl ResourcePath {
    pub fn root() -> Self {
        Self {
            segments: Vec::new(),
        }
    }

    pub fn segments(&self) -> &[String] {
        &self.segments
    }

    pub fn is_root(&self) -> bool {
        self.segments.is_empty()
    }

    pub fn name(&self) -> Option<&str> {
        self.segments.last().map(String::as_str)
    }

    pub fn canonical(&self) -> String {
        if self.segments.is_empty() {
            return "/".to_owned();
        }
        let mut out = String::new();
        for segment in &self.segments {
            out.push('/');
            out.push_str(segment);
        }
        out
    }

    pub fn parent(&self) -> Option<Self> {
        let mut segments = self.segments.clone();
        segments.pop()?;
        Some(Self { segments })
    }

    pub fn starts_with(&self, root: &Self) -> bool {
        self.segments.starts_with(&root.segments)
    }

    pub fn direct_child_name(&self, parent: &Self) -> Option<&str> {
        (self.segments.len() == parent.segments.len() + 1 && self.starts_with(parent))
            .then(|| self.segments.last().map(String::as_str))
            .flatten()
    }

    /// Breadcrumb chain from the connection root through this path.
    pub fn chain_from(&self, root: &Self) -> Option<Vec<Self>> {
        if !self.starts_with(root) {
            return None;
        }
        let mut chain = Vec::new();
        let mut cursor = root.clone();
        chain.push(cursor.clone());
        for segment in &self.segments[root.segments.len()..] {
            cursor.segments.push(segment.clone());
            chain.push(cursor.clone());
        }
        Some(chain)
    }

    pub fn join_child(&self, name: &str) -> Result<Self, PathError> {
        validate_segment(name)?;
        let mut segments = self.segments.clone();
        segments.push(name.to_owned());
        Ok(Self { segments })
    }

    /// User-entered root such as `/dav/挂载/`. A missing leading slash is accepted once.
    pub fn parse_root(input: &str) -> Result<Self, PathError> {
        let input = input.trim();
        if input.is_empty() || input == "/" {
            return Ok(Self::root());
        }
        let input = input.strip_prefix('/').unwrap_or(input);
        parse_decoded_segments(input, false)
    }

    /// Server href. Absolute URLs must match the connection origin; query strings are ignored.
    pub fn parse_href(
        href: &str,
        origin_host: &str,
        origin_port: u16,
        origin_https: bool,
        connection_root: &Self,
    ) -> Result<Self, PathError> {
        let href = href.trim();
        if href.is_empty() {
            return Err(PathError::Empty);
        }
        if href.contains('#') {
            return Err(PathError::Fragment);
        }
        let path = if href.contains("://") {
            let url = url::Url::parse(href).map_err(|_| PathError::Encoding)?;
            if !url.username().is_empty() || url.password().is_some() {
                return Err(PathError::UserInfo);
            }
            let host = url.host_str().ok_or(PathError::Encoding)?;
            if !host.eq_ignore_ascii_case(origin_host)
                || url.port_or_known_default() != Some(origin_port)
            {
                return Err(PathError::OutsideRoot);
            }
            let https = url.scheme() == "https";
            if https != origin_https || (url.scheme() != "http" && url.scheme() != "https") {
                return Err(PathError::OutsideRoot);
            }
            url.path().to_owned()
        } else if let Some(path) = href.strip_prefix('/') {
            path.to_owned()
        } else {
            return Err(PathError::Relative);
        };
        let parsed = parse_decoded_segments(&path, true)?;
        if parsed.starts_with(connection_root) {
            Ok(parsed)
        } else {
            Err(PathError::OutsideRoot)
        }
    }

    pub fn encode_url_path(&self, trailing_slash: bool) -> String {
        let mut out = String::from("/");
        for (index, segment) in self.segments.iter().enumerate() {
            if index > 0 {
                out.push('/');
            }
            out.push_str(&utf8_percent_encode(segment, URL_SEGMENT).to_string());
        }
        if trailing_slash && !out.ends_with('/') {
            out.push('/');
        }
        if self.segments.is_empty() {
            out = "/".to_owned();
        }
        out
    }
}

fn parse_decoded_segments(
    path: &str,
    already_absolute_body: bool,
) -> Result<ResourcePath, PathError> {
    let path = path.trim_matches('/');
    if path.is_empty() {
        return Ok(ResourcePath::root());
    }
    let _ = already_absolute_body;
    let mut segments = Vec::new();
    for raw in path.split('/') {
        if raw.is_empty() {
            return Err(PathError::Encoding);
        }
        let decoded = percent_decode_str(raw)
            .decode_utf8()
            .map_err(|_| PathError::Encoding)?;
        validate_segment(&decoded)?;
        segments.push(decoded.into_owned());
    }
    Ok(ResourcePath { segments })
}

fn validate_segment(segment: &str) -> Result<(), PathError> {
    if segment.is_empty() || segment == "." || segment == ".." {
        return Err(PathError::DotSegment);
    }
    if segment.contains('\0') || segment.contains('/') || segment.contains('\\') {
        return Err(PathError::Encoding);
    }
    Ok(())
}

pub fn endpoint_url(
    https: bool,
    host: &str,
    port: u16,
    path: &ResourcePath,
    trailing_slash: bool,
) -> Result<url::Url, PathError> {
    if host.is_empty()
        || host.contains(['/', '\\', ' ', '@', '?'])
        || host.starts_with('[')
        || host.ends_with('.')
    {
        return Err(PathError::Encoding);
    }
    let host_part = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    let scheme = if https { "https" } else { "http" };
    let raw = format!(
        "{scheme}://{host_part}:{port}{}",
        path.encode_url_path(trailing_slash)
    );
    url::Url::parse(&raw).map_err(|_| PathError::Encoding)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> ResourcePath {
        ResourcePath::parse_root("/dav/挂载").unwrap()
    }

    #[test]
    fn webdav_path_decodes_reserved_characters_once_and_keeps_case() {
        let root = root();
        let href = "/dav/%E6%8C%82%E8%BD%BD/%E4%B8%AD%E6%96%87%20a%23b%25c%2Bd/File";
        let path = ResourcePath::parse_href(href, "nas.example", 5244, false, &root).unwrap();
        assert_eq!(path.canonical(), "/dav/挂载/中文 a#b%c+d/File");
        let lower = ResourcePath::parse_href(
            "/dav/%E6%8C%82%E8%BD%BD/%E4%B8%AD%E6%96%87%20a%23b%25c%2Bd/file",
            "nas.example",
            5244,
            false,
            &root,
        )
        .unwrap();
        assert_ne!(path, lower);
        let twice = "/dav/%E6%8C%82%E8%BD%BD/%252e%252e";
        let once = ResourcePath::parse_href(twice, "nas.example", 5244, false, &root).unwrap();
        assert_eq!(once.name(), Some("%2e%2e"));
        assert!(
            ResourcePath::parse_href(
                "/dav/%E6%8C%82%E8%BD%BD/%2e%2e/secret",
                "nas.example",
                5244,
                false,
                &root
            )
            .is_err()
        );
    }

    #[test]
    fn webdav_path_rejects_root_escape_relative_and_encoded_slash() {
        let root = root();
        assert!(
            ResourcePath::parse_href("/dav/other/file", "nas.example", 5244, false, &root).is_err()
        );
        assert!(ResourcePath::parse_href("挂载/file", "nas.example", 5244, false, &root).is_err());
        assert!(
            ResourcePath::parse_href(
                "/dav/%E6%8C%82%E8%BD%BD/a%2Fb",
                "nas.example",
                5244,
                false,
                &root
            )
            .is_err()
        );
        assert!(
            ResourcePath::parse_href(
                "http://user:secret@nas.example:5244/dav/%E6%8C%82%E8%BD%BD/",
                "nas.example",
                5244,
                false,
                &root
            )
            .is_err()
        );
        let child = ResourcePath::parse_href(
            "http://NAS.example:5244/dav/%E6%8C%82%E8%BD%BD/%E4%B8%AD%E6%96%87?sig=secret",
            "nas.example",
            5244,
            false,
            &root,
        )
        .unwrap();
        assert_eq!(child.direct_child_name(&root), Some("中文"));
        assert_eq!(child.canonical(), "/dav/挂载/中文");
    }

    #[test]
    fn webdav_url_encodes_reserved_characters_without_double_encoding() {
        let path = ResourcePath::parse_root("/dav/挂载/中文 a#b%c+d").unwrap();
        let url = endpoint_url(false, "nas.example", 5244, &path, true).unwrap();
        let text = url.as_str();
        assert!(text.starts_with("http://nas.example:5244/dav/"));
        assert!(text.contains("%E4%B8%AD%E6%96%87%20a%23b%25c%2Bd/"));
        assert!(!text.contains("%2520"));
        assert!(!text.contains('+'));
        assert!(!text.contains('#'));
    }
}
