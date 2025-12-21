use crate::{defs, ksucalls, utils};
use log::{debug, warn};
use std::path::{Path, PathBuf};
use std::{fs::OpenOptions, io::Write, sync::Once, thread, time::Duration};
use std::os::unix::io::AsRawFd;
use libc;

const DEFAULT_POLL_INTERVAL_SECS: u64 = 30;
const ROTATE_SIZE_BYTES: u64 = 32 * 1024 * 1024;
const SULOG_FILENAME: &str = "sulog.log";
const SULOG_OLD_FILENAME: &str = "sulog.old.log";

/// Return canonical paths for current sulog and rotated sulog.
fn sulog_paths() -> (PathBuf, PathBuf) {
    let logdir = Path::new(defs::LOG_DIR);
    (logdir.join(SULOG_FILENAME), logdir.join(SULOG_OLD_FILENAME))
}

/// Rotate sulog file if it exceeds configured threshold.
fn rotate_if_needed(sulog_path: &Path, old_path: &Path) {
    if let Ok(meta) = std::fs::metadata(sulog_path) {
        if meta.len() > ROTATE_SIZE_BYTES {
            if let Err(e) = std::fs::rename(sulog_path, old_path) {
                debug!("sulog poller: rotate failed: {e}");
            }
        }
    }
}

/// Persist fetched content to sulog file; returns Ok(()) on success.
fn persist_sulog_content(content: &str) -> Result<(), ()> {
    if content.is_empty() {
        return Ok(());
    }

    let logdir = Path::new(defs::LOG_DIR);
    utils::ensure_dir_exists(logdir).map_err(|_| {
        warn!("sulog poller: ensure log dir failed");
    })?;

    let (sulog_path, old_path) = sulog_paths();
    rotate_if_needed(&sulog_path, &old_path);

    match OpenOptions::new().create(true).append(true).open(&sulog_path) {
        Ok(mut f) => {
            if let Err(e) = f.write_all(content.as_bytes()) {
                warn!("sulog poller: failed to write sulog: {e}");
                return Err(());
            }
            Ok(())
        }
        Err(e) => {
            warn!("sulog poller: failed to open sulog file: {e}");
            Err(())
        }
    }
}

fn fetch_and_persist_once() {
    match ksucalls::fetch_sulog() {
        Ok(content) => {
            if let Err(_) = persist_sulog_content(&content) {
                debug!("sulog poller: persist failed");
            }
        }
        Err(e) => debug!("sulog poller: fetch sulog failed: {:?}", e),
    }
}

/// Start the sulog background poller (idempotent).
/// It will be spawned once per process and will poll kernel periodically.
pub fn start() {
    static START: Once = Once::new();
    START.call_once(|| {
        // immediate fetch at startup
        fetch_and_persist_once();

        let poll_interval = std::env::var("KSU_SULOG_POLL_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(DEFAULT_POLL_INTERVAL_SECS);
        // Daemonize into a separate process so the poller survives session changes.
        unsafe {
            match libc::fork() {
                -1 => {
                    warn!("sulog poller: fork failed");
                    // fallback to threaded poller
                    let _ = thread::Builder::new()
                        .name("ksud-sulog-poller".to_string())
                        .spawn(move || loop {
                            fetch_and_persist_once();
                            thread::sleep(Duration::from_secs(poll_interval));
                        });
                }
                0 => {
                    // Child: create new session
                    if libc::setsid() < 0 {
                        warn!("sulog poller: setsid failed");
                    }
                    // second fork to fully detach
                    match libc::fork() {
                        -1 => {
                            libc::_exit(1);
                        }
                        0 => {
                            // Grandchild: redirect stdio to /dev/null
                            if let Ok(devnull) = OpenOptions::new().read(true).write(true).open("/dev/null") {
                                let fd = devnull.as_raw_fd();
                                let _ = libc::dup2(fd, libc::STDIN_FILENO);
                                let _ = libc::dup2(fd, libc::STDOUT_FILENO);
                                let _ = libc::dup2(fd, libc::STDERR_FILENO);
                            }

                            // Poll loop - this is the persistent daemon process
                            loop {
                                fetch_and_persist_once();
                                thread::sleep(Duration::from_secs(poll_interval));
                            }
                        }
                        _ => {
                            // Child exits
                            libc::_exit(0);
                        }
                    }
                }
                _pid => {
                    // Parent: return immediately
                }
            }
        }
    });
}
