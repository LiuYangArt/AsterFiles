use std::{io, ptr};

pub(crate) struct KillOnCloseJob(windows_sys::Win32::Foundation::HANDLE);

impl KillOnCloseJob {
    pub(crate) fn create() -> io::Result<Self> {
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };

        // SAFETY: Both optional security and name pointers are null; a successful call transfers an owned job handle to this function.
        let job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: job is a live owned handle; limits has the JOBOBJECT_EXTENDED_LIMIT_INFORMATION layout and exact supplied byte length.
        if unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            // SAFETY: The job handle was created successfully, has not escaped, and is closed once on this setup failure path.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(job) };
            return Err(io::Error::last_os_error());
        }
        Ok(Self(job))
    }

    pub(crate) fn assign(&self, child: &std::process::Child) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

        // SAFETY: self owns the live job handle and the borrowed Child keeps its process handle open through assignment.
        if unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle() as _) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for KillOnCloseJob {
    fn drop(&mut self) {
        // SAFETY: KillOnCloseJob uniquely owns this successful job handle and releases it once after users have finished.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}
