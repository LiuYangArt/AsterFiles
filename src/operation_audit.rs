use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

static AUDIT: OnceLock<Mutex<Option<AuditWriter>>> = OnceLock::new();

/// Queue evidence without filesystem access on the caller (including the UI thread).
pub fn record(event: &str, detail: impl AsRef<str>) {
    let state = AUDIT.get_or_init(|| Mutex::new(None));
    let mut state = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if state.is_none() {
        match AuditWriter::start(default_path()) {
            Ok(writer) => *state = Some(writer),
            Err(error) => {
                eprintln!("file operation audit worker could not start: {error}");
                return;
            }
        }
    }
    if let Some(writer) = state.as_ref()
        && let Err(error) = writer.record(event, detail.as_ref())
    {
        eprintln!("file operation audit could not queue evidence: {error}");
    }
}

/// Drain all queued evidence and join the worker; call after producers stop at exit.
/// A later record starts a fresh worker, allowing repeated headless scenarios.
pub fn flush() -> io::Result<()> {
    let Some(state) = AUDIT.get() else {
        return Ok(());
    };
    let mut state = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match state.take() {
        Some(mut writer) => writer.flush(),
        None => Ok(()),
    }
}

fn default_path() -> PathBuf {
    if cfg!(debug_assertions) {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("artifacts/logs/file-operation-audit.jsonl")
    } else {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("AsterFiles/logs/file-operation-audit.jsonl")
    }
}

struct AuditWriter {
    sender: Option<mpsc::Sender<String>>,
    worker: Option<JoinHandle<io::Result<()>>>,
}

impl AuditWriter {
    fn start(path: PathBuf) -> io::Result<Self> {
        let (sender, receiver) = mpsc::channel::<String>();
        let worker = thread::Builder::new()
            .name("file-operation-audit".into())
            .spawn(move || {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut file = OpenOptions::new().create(true).append(true).open(path)?;
                for line in receiver {
                    file.write_all(line.as_bytes())?;
                    // Do not retain operation evidence in a userspace buffer between events.
                    file.flush()?;
                }
                file.sync_data()
            })?;
        Ok(Self {
            sender: Some(sender),
            worker: Some(worker),
        })
    }

    fn record(&self, event: &str, detail: &str) -> io::Result<()> {
        self.sender
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "audit writer stopped"))?
            .send(json_line(event, detail))
            .map_err(|error| io::Error::new(io::ErrorKind::BrokenPipe, error))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sender.take();
        match self.worker.take() {
            Some(worker) => worker
                .join()
                .map_err(|_| io::Error::other("file operation audit worker panicked"))?,
            None => Ok(()),
        }
    }
}

impl Drop for AuditWriter {
    fn drop(&mut self) {
        if let Err(error) = self.flush() {
            eprintln!("file operation audit could not persist evidence: {error}");
        }
    }
}

fn json_line(event: &str, detail: &str) -> String {
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!(
        "{{\"timestamp_ms\":{timestamp_ms},\"pid\":{},\"event\":\"{}\",\"detail\":\"{}\"}}\n",
        std::process::id(),
        json_escape(event),
        json_escape(detail)
    )
}

fn json_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character <= '\u{1f}' => {
                let _ = write!(escaped, "\\u{:04x}", character as u32);
            }
            character => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

    fn temporary_path() -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "asterfiles-operation-audit-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT_TEST.fetch_add(1, Ordering::Relaxed)
            ))
            .join("audit.jsonl")
    }

    #[test]
    fn flush_persists_ordered_chinese_evidence_and_joins_worker() {
        let path = temporary_path();
        let mut writer = AuditWriter::start(path.clone()).unwrap();
        writer
            .record(
                "导航\"开始",
                "来源=C:\\临时\\Bridge\n目标=Whitebox\t\r\u{0}\u{1f}",
            )
            .unwrap();
        for index in 0..64 {
            writer
                .record("operation_result", &format!("结果={index}"))
                .unwrap();
        }
        writer.flush().unwrap();
        assert!(writer.worker.is_none());
        assert!(writer.sender.is_none());
        let text = fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 65);
        assert!(lines[0].starts_with("{\"timestamp_ms\":"));
        assert!(lines[0].contains(&format!("\"pid\":{}", std::process::id())));
        assert!(lines[0].ends_with(
            "\"event\":\"导航\\\"开始\",\"detail\":\"来源=C:\\\\临时\\\\Bridge\\n目标=Whitebox\\t\\r\\u0000\\u001f\"}"
        ));
        for index in 0..64 {
            assert!(lines[index + 1].ends_with(&format!("\"detail\":\"结果={index}\"}}")));
        }
        writer.flush().unwrap();
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn append_preserves_previous_session_evidence() {
        let path = temporary_path();
        for event in ["session_one", "session_two"] {
            let mut writer = AuditWriter::start(path.clone()).unwrap();
            writer.record(event, "完成").unwrap();
            writer.flush().unwrap();
        }
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains("session_one"));
        assert!(text.contains("session_two"));
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
