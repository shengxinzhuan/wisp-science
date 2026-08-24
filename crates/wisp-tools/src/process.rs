//! Keep subprocesses from opening a console on Windows GUI builds.

use std::sync::{Mutex, OnceLock};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

// Git for Windows can fail during DLL initialization when several windows probe
// it at once. Serialize every git.exe spawn through this lock.
static GIT_COMMAND_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn git_command_lock() -> &'static Mutex<()> {
    GIT_COMMAND_LOCK.get_or_init(|| Mutex::new(()))
}

pub struct GitCommandGuard {
    _guard: std::sync::MutexGuard<'static, ()>,
}

/// Hold only while starting Git; do not keep this guard across `.await`.
pub fn lock_git_command() -> GitCommandGuard {
    GitCommandGuard {
        _guard: git_command_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner()),
    }
}

#[cfg_attr(not(windows), allow(unused_variables))]
pub fn hide_console(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
}

#[cfg_attr(not(windows), allow(unused_variables))]
pub fn hide_console_async(cmd: &mut tokio::process::Command) {
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(test)]
mod tests {
    #[test]
    fn git_command_lock_is_reentrant_safe_across_threads() {
        use super::lock_git_command;
        use std::sync::Arc;
        use std::thread;

        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let started_for_thread = Arc::clone(&started);
        let done_for_thread = Arc::clone(&done);
        let _guard = lock_git_command();
        let worker = thread::spawn(move || {
            started_for_thread.store(true, std::sync::atomic::Ordering::SeqCst);
            let _guard = lock_git_command();
            done_for_thread.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        while !started.load(std::sync::atomic::Ordering::SeqCst) {
            thread::yield_now();
        }
        assert!(
            !done.load(std::sync::atomic::Ordering::SeqCst),
            "git lock must serialize concurrent spawns"
        );
        drop(_guard);
        worker.join().expect("git lock worker");
        assert!(done.load(std::sync::atomic::Ordering::SeqCst));
    }
}
