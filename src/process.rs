use std::ops::{Deref, DerefMut};
use std::os::windows::io::AsRawHandle;
use std::process::{Child, Command};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};

pub struct ManagedChild {
    child: Child,
    job: HANDLE,
}

impl ManagedChild {
    // The job owns helper descendants too, including those started by a CLI shim.
    pub fn spawn(command: &mut Command) -> Result<Self, String> {
        let job = unsafe { CreateJobObjectW(None, None) }
            .map_err(|_| "Could not create a helper process job.".to_string())?;
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        }
        .is_err()
        {
            unsafe { CloseHandle(job) }.ok();
            return Err("Could not configure helper process cleanup.".into());
        }
        let child = match command.spawn() {
            Ok(child) => child,
            Err(_) => {
                unsafe { CloseHandle(job) }.ok();
                return Err(
                    "CLI was not found or could not start. Check installation and PATH.".into(),
                );
            }
        };
        let managed = Self { child, job };
        unsafe { AssignProcessToJobObject(job, HANDLE(managed.child.as_raw_handle())) }
            .map_err(|_| "Could not manage helper process cleanup.".to_string())?;
        Ok(managed)
    }
}

impl Deref for ManagedChild {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.child
    }
}

impl DerefMut for ManagedChild {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.child
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.job) }.ok();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
