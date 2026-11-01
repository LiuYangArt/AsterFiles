use crate::domain::file_operations::CancellationToken;
use std::{
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub(crate) mod temporary_copy;

#[derive(Clone)]
enum CopyMode {
    Local,
    Isolated(temporary_copy::Registration),
}

/// One copy operation owns its cancellation, recovery state and temporary-file registry.
#[derive(Clone)]
pub(crate) struct CopyExecution {
    cancel: CancellationToken,
    recovering: Arc<AtomicBool>,
    mode: CopyMode,
}

impl CopyExecution {
    pub(crate) fn local(cancel: CancellationToken) -> Self {
        Self {
            cancel,
            recovering: Arc::new(AtomicBool::new(false)),
            mode: CopyMode::Local,
        }
    }

    pub(crate) fn isolated(
        cancel: CancellationToken,
        registry_path: Option<PathBuf>,
    ) -> io::Result<Self> {
        let path = registry_path
            .filter(|path| !path.as_os_str().is_empty())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "isolated copy requires a temporary-file registry",
                )
            })?;
        Ok(Self {
            cancel,
            recovering: Arc::new(AtomicBool::new(false)),
            mode: CopyMode::Isolated(temporary_copy::Registration::new(path)),
        })
    }

    pub(crate) fn recovery_scope(&self) -> RecoveryScope<'_> {
        RecoveryScope {
            execution: self,
            previous: self.is_recovering(),
        }
    }

    pub(crate) fn cancel(&self) -> &CancellationToken {
        &self.cancel
    }

    pub(crate) fn set_recovering(&self, recovering: bool) {
        self.recovering.store(recovering, Ordering::Release);
    }

    pub(crate) fn is_recovering(&self) -> bool {
        self.recovering.load(Ordering::Acquire)
    }

    pub(crate) fn register_temporary(&self, path: &Path) -> io::Result<()> {
        match &self.mode {
            CopyMode::Local => Ok(()),
            CopyMode::Isolated(registration) => registration.register(path, None),
        }
    }

    pub(crate) fn register_staging(&self, path: &Path, staged: &Path) -> io::Result<()> {
        match &self.mode {
            CopyMode::Local => Ok(()),
            CopyMode::Isolated(registration) => registration.register(path, Some(staged)),
        }
    }

    pub(crate) fn retire_staging(&self, path: &Path, staged: &Path) -> io::Result<()> {
        match &self.mode {
            CopyMode::Local => Ok(()),
            CopyMode::Isolated(registration) => registration.retire_staging(path, staged),
        }
    }
}

pub(crate) struct RecoveryScope<'a> {
    execution: &'a CopyExecution,
    previous: bool,
}
impl Drop for RecoveryScope<'_> {
    fn drop(&mut self) {
        // A nested query must preserve recovery still owned by its enclosing copy.
        self.execution.set_recovering(self.previous);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_149_local_copy_needs_no_registry() {
        let execution = CopyExecution::local(CancellationToken::new());
        let path = Path::new("not-created");
        execution.register_temporary(path).unwrap();
        execution.register_staging(path, path).unwrap();
        execution.retire_staging(path, path).unwrap();
    }

    #[test]
    fn issue_149_isolated_copy_requires_nonempty_registry() {
        for path in [None, Some(PathBuf::new())] {
            assert!(matches!(
                CopyExecution::isolated(CancellationToken::new(), path),
                Err(error) if error.kind() == io::ErrorKind::InvalidInput
            ));
        }
    }

    #[test]
    fn issue_149_recovery_and_cancellation_follow_explicit_context_across_threads() {
        let first = CopyExecution::local(CancellationToken::new());
        let other = CopyExecution::local(CancellationToken::new());
        let worker = first.clone();
        std::thread::spawn(move || {
            worker.set_recovering(true);
            worker.cancel().cancel();
        })
        .join()
        .unwrap();
        assert!(first.is_recovering());
        assert!(first.cancel().is_cancelled());
        assert!(!other.is_recovering());
        assert!(!other.cancel().is_cancelled());
        // Dropping a clone must not implicitly change another owner's recovery report.
        drop(first.clone());
        assert!(first.is_recovering());
        first.set_recovering(false);
        assert!(!first.is_recovering());
    }
}
