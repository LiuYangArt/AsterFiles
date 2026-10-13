//! Incremental PROPFIND parser. Each response is finished before the next one is kept.
use std::{
    io::{self, BufRead},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use quick_xml::{events::Event, name::ResolveResult, reader::NsReader};

use crate::webdav::path::{PathError, ResourcePath};

const MAX_DEPTH: usize = 32;
const MAX_TEXT: usize = 64 * 1024;
const DAV: &[u8] = b"DAV:";

#[derive(Debug)]
pub enum XmlFailure {
    Io(io::Error),
    Invalid { emitted: bool },
    Limit { emitted: bool },
}

impl XmlFailure {
    pub fn is_interrupt(&self) -> bool {
        matches!(
            self,
            Self::Io(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::TimedOut
                )
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedItem {
    pub path: ResourcePath,
    pub name: String,
    pub is_directory: bool,
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseFinish {
    Complete,
    Incomplete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseStats {
    pub finish: ParseFinish,
    pub error_responses: usize,
}

#[derive(Clone)]
struct PropStat {
    success: bool,
    is_directory: bool,
    size: Option<u64>,
    modified: Option<SystemTime>,
}

struct ResponseBuild {
    href: Option<String>,
    propstats: Vec<PropStat>,
    current: PropStat,
    in_success_prop: bool,
}

pub fn parse_propfind<R, F>(
    reader: R,
    origin_host: &str,
    origin_port: u16,
    origin_https: bool,
    connection_root: &ResourcePath,
    directory: &ResourcePath,
    mut on_item: F,
) -> Result<ParseStats, XmlFailure>
where
    R: BufRead,
    F: FnMut(ListedItem) -> io::Result<()>,
{
    let mut reader = NsReader::from_reader(reader);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut depth = 0_usize;
    let mut stack: Vec<&'static str> = Vec::new();
    let mut text = String::new();
    let mut response: Option<ResponseBuild> = None;
    let mut saw_multistatus = false;
    let mut emitted = false;
    let mut error_responses = 0_usize;
    let mut seen = std::collections::HashSet::<String>::new();

    loop {
        let (namespace, event) = reader.read_resolved_event_into(&mut buf).map_err(|error| {
            if let quick_xml::Error::Io(io_error) = &error {
                XmlFailure::Io(io::Error::new(io_error.kind(), io_error.to_string()))
            } else {
                XmlFailure::Invalid { emitted }
            }
        })?;
        match event {
            Event::DocType(_) => return Err(XmlFailure::Invalid { emitted }),
            Event::Start(element) => {
                depth += 1;
                if depth > MAX_DEPTH {
                    return Err(XmlFailure::Limit { emitted });
                }
                text.clear();
                let name = local_name(&element.local_name());
                if is_dav(namespace) {
                    match name {
                        "multistatus" => saw_multistatus = true,
                        "response" => {
                            response = Some(ResponseBuild {
                                href: None,
                                propstats: Vec::new(),
                                current: empty_prop(),
                                in_success_prop: false,
                            });
                        }
                        "propstat" => {
                            if let Some(response) = response.as_mut() {
                                response.current = empty_prop();
                                response.in_success_prop = false;
                            }
                        }
                        "prop" => {
                            if let Some(response) = response.as_mut() {
                                response.in_success_prop = response.current.success;
                            }
                        }
                        "resourcetype" | "collection" | "href" | "status" | "getcontentlength"
                        | "getlastmodified" => {}
                        _ => {}
                    }
                    if name == "collection"
                        && stack.last() == Some(&"resourcetype")
                        && let Some(response) = response.as_mut()
                    {
                        response.current.is_directory = true;
                    }
                    stack.push(remember(name));
                } else {
                    stack.push("other");
                }
            }
            Event::Empty(element) => {
                let name = local_name(&element.local_name());
                if is_dav(namespace)
                    && name == "collection"
                    && stack.last() == Some(&"resourcetype")
                    && let Some(response) = response.as_mut()
                {
                    response.current.is_directory = true;
                }
            }
            Event::Text(value) => {
                let value = value
                    .xml10_content()
                    .map_err(|_| XmlFailure::Invalid { emitted })?;
                if text.len().saturating_add(value.len()) > MAX_TEXT {
                    return Err(XmlFailure::Limit { emitted });
                }
                text.push_str(&value);
            }
            Event::End(_) => {
                let frame = stack.pop().unwrap_or("other");
                depth = depth.saturating_sub(1);
                if let Some(current) = response.as_mut() {
                    match frame {
                        "href" => current.href = Some(text.trim().to_owned()),
                        "status" => {
                            if let Some(code) = http_status(&text) {
                                current.current.success = (200..300).contains(&code);
                            }
                        }
                        "getcontentlength" => {
                            current.current.size = text.trim().parse().ok();
                        }
                        "getlastmodified" => {
                            current.current.modified = http_date(text.trim());
                        }
                        "propstat" => {
                            if !current.current.success {
                                current.current.is_directory = false;
                                current.current.size = None;
                                current.current.modified = None;
                            }
                            let finished = current.current.clone();
                            current.propstats.push(finished);
                            current.current = empty_prop();
                            current.in_success_prop = false;
                        }
                        "response" => {
                            let finished = response.take();
                            if let Some(finished) = finished {
                                match accept_response(
                                    finished,
                                    origin_host,
                                    origin_port,
                                    origin_https,
                                    connection_root,
                                    directory,
                                    &mut seen,
                                ) {
                                    ResponseClass::Item(item) => {
                                        emitted = true;
                                        on_item(item).map_err(XmlFailure::Io)?;
                                    }
                                    ResponseClass::Error => {
                                        emitted = true;
                                        error_responses += 1;
                                    }
                                    ResponseClass::Ignore => {}
                                }
                            }
                        }
                        _ => {}
                    }
                }
                text.clear();
            }
            Event::Eof => break,
            Event::CData(value) => {
                let value = value
                    .decode()
                    .map_err(|_| XmlFailure::Invalid { emitted })?;
                if text.len().saturating_add(value.len()) > MAX_TEXT {
                    return Err(XmlFailure::Limit { emitted });
                }
                text.push_str(&value);
            }
            Event::Comment(_) | Event::Decl(_) | Event::PI(_) => {}
            _ => {}
        }
        buf.clear();
    }

    if !saw_multistatus {
        return Err(XmlFailure::Invalid { emitted });
    }
    let finish = if error_responses > 0 {
        ParseFinish::Incomplete
    } else {
        ParseFinish::Complete
    };
    Ok(ParseStats {
        finish,
        error_responses,
    })
}

enum ResponseClass {
    Item(ListedItem),
    Error,
    Ignore,
}

fn accept_response(
    response: ResponseBuild,
    origin_host: &str,
    origin_port: u16,
    origin_https: bool,
    connection_root: &ResourcePath,
    directory: &ResourcePath,
    seen: &mut std::collections::HashSet<String>,
) -> ResponseClass {
    let Some(href) = response.href else {
        return ResponseClass::Error;
    };
    let path = match ResourcePath::parse_href(
        &href,
        origin_host,
        origin_port,
        origin_https,
        connection_root,
    ) {
        Ok(path) => path,
        Err(PathError::OutsideRoot) | Err(PathError::DotSegment) | Err(PathError::Relative) => {
            return ResponseClass::Ignore;
        }
        Err(_) => return ResponseClass::Error,
    };
    let success = response.propstats.iter().find(|prop| prop.success);
    if path == *directory {
        return if success.is_some() {
            ResponseClass::Ignore
        } else {
            ResponseClass::Error
        };
    }
    let Some(name) = path.direct_child_name(directory) else {
        return ResponseClass::Ignore;
    };
    let Some(success) = success else {
        return ResponseClass::Error;
    };
    if !seen.insert(path.canonical()) {
        return ResponseClass::Ignore;
    }
    ResponseClass::Item(ListedItem {
        name: name.to_owned(),
        path,
        is_directory: success.is_directory,
        size: success.size,
        modified: success.modified,
    })
}

fn empty_prop() -> PropStat {
    PropStat {
        success: false,
        is_directory: false,
        size: None,
        modified: None,
    }
}

fn is_dav(namespace: ResolveResult<'_>) -> bool {
    match namespace {
        ResolveResult::Bound(namespace) => namespace.as_ref() == DAV,
        ResolveResult::Unbound | ResolveResult::Unknown(_) => false,
    }
}

fn local_name(name: &quick_xml::name::LocalName<'_>) -> &'static str {
    let bytes = name.as_ref();
    match bytes {
        b"multistatus" => "multistatus",
        b"response" => "response",
        b"href" => "href",
        b"propstat" => "propstat",
        b"status" => "status",
        b"prop" => "prop",
        b"resourcetype" => "resourcetype",
        b"collection" => "collection",
        b"getcontentlength" => "getcontentlength",
        b"getlastmodified" => "getlastmodified",
        b"displayname" => "displayname",
        _ => "other",
    }
}

fn remember(name: &str) -> &'static str {
    match name {
        "multistatus" => "multistatus",
        "response" => "response",
        "href" => "href",
        "propstat" => "propstat",
        "status" => "status",
        "prop" => "prop",
        "resourcetype" => "resourcetype",
        "collection" => "collection",
        "getcontentlength" => "getcontentlength",
        "getlastmodified" => "getlastmodified",
        "displayname" => "displayname",
        _ => "other",
    }
}

fn http_status(text: &str) -> Option<u16> {
    let mut parts = text.split_whitespace();
    let version = parts.next()?;
    if !version.starts_with("HTTP/") {
        return None;
    }
    parts.next()?.parse().ok()
}

fn http_date(text: &str) -> Option<SystemTime> {
    let rest = text.split_once(',')?.1.trim();
    let mut parts = rest.split_whitespace();
    let day: u32 = parts.next()?.parse().ok()?;
    let month = month_index(parts.next()?)?;
    let year: i32 = parts.next()?.parse().ok()?;
    let time = parts.next()?;
    if parts
        .next()
        .is_some_and(|zone| !zone.eq_ignore_ascii_case("GMT"))
    {
        return None;
    }
    let mut clock = time.split(':');
    let hour: u32 = clock.next()?.parse().ok()?;
    let minute: u32 = clock.next()?.parse().ok()?;
    let second: u32 = clock.next()?.parse().ok()?;
    if !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let days = days_from_civil(year, month, day)?;
    let seconds =
        days * 86_400 + i64::from(hour) * 3_600 + i64::from(minute) * 60 + i64::from(second);
    if seconds < 0 {
        return None;
    }
    UNIX_EPOCH.checked_add(Duration::from_secs(seconds as u64))
}

fn month_index(month: &str) -> Option<u32> {
    Some(match month {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    })
}

fn days_from_civil(year: i32, month: u32, day: u32) -> Option<i64> {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = (year - era * 400) as u32;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(i64::from(era) * 146_097 + i64::from(day_of_era) - 719_468)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn root() -> ResourcePath {
        ResourcePath::parse_root("/dav/mount").unwrap()
    }

    #[test]
    fn webdav_propfind_keeps_mixed_success_and_marks_failed_children_incomplete() {
        let xml = r#"<?xml version="1.0"?>
            <D:multistatus xmlns:D="DAV:">
              <D:response>
                <D:href>/dav/mount/</D:href>
                <D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
                <D:propstat><D:prop><D:getcontentlength/></D:prop><D:status>HTTP/1.1 404 Not Found</D:status></D:propstat>
              </D:response>
              <D:response>
                <D:href>/dav/mount/ok.txt</D:href>
                <D:propstat><D:prop><D:getcontentlength>4</D:getcontentlength><D:getlastmodified>Tue, 13 Oct 2026 00:00:00 GMT</D:getlastmodified><D:resourcetype/></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
              </D:response>
              <D:response>
                <D:href>/dav/mount/hidden</D:href>
                <D:propstat><D:status>HTTP/1.1 403 Forbidden</D:status></D:propstat>
              </D:response>
            </D:multistatus>"#;
        let mut items = Vec::new();
        let stats = parse_propfind(
            Cursor::new(xml.as_bytes()),
            "nas.example",
            5244,
            false,
            &root(),
            &root(),
            |item| {
                items.push(item);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "ok.txt");
        assert_eq!(items[0].size, Some(4));
        assert!(items[0].modified.is_some());
        assert!(!items[0].is_directory);
        assert_eq!(stats.finish, ParseFinish::Incomplete);
        assert_eq!(stats.error_responses, 1);
    }

    #[test]
    fn webdav_propfind_rejects_doctype_and_broken_xml_after_an_item() {
        let broken = br#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:">
            <D:response><D:href>/dav/mount/a</D:href><D:propstat><D:prop><D:resourcetype/></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
            <D:response><D:href"#;
        let mut count = 0;
        let error = parse_propfind(
            Cursor::new(&broken[..]),
            "nas.example",
            5244,
            false,
            &root(),
            &root(),
            |_| {
                count += 1;
                Ok(())
            },
        );
        assert!(error.is_err());
        assert_eq!(count, 1);
        let doctype = br#"<!DOCTYPE foo [<!ENTITY x SYSTEM "file:///secret">]><D:multistatus xmlns:D="DAV:"></D:multistatus>"#;
        assert!(
            parse_propfind(
                Cursor::new(&doctype[..]),
                "nas.example",
                5244,
                false,
                &root(),
                &root(),
                |_| Ok(())
            )
            .is_err()
        );
    }
}
