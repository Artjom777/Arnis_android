//! Android runtime support module for Arnis.
//! Provides memory protections, eager memory purging, Android storage resolution,
//! crash logging, and Bedrock LevelDB adaptations.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

extern "C" {
    fn mi_collect(force: bool);
}

/// Calls mimalloc's C API `mi_collect` to purge freed memory pages back to the OS.
#[inline]
pub fn collect_memory(force: bool) {
    unsafe {
        mi_collect(force);
    }
}

/// Configures runtime memory protections against premature killing by Android lmkd.
/// Eagerly returns freed mimalloc pages to the OS to minimize RSS bloat.
pub fn protect_process_memory() {
    unsafe {
        std::env::set_var("MIMALLOC_PURGE_DELAY", "0");
        std::env::set_var("MIMALLOC_RESET_DELAY", "0");
        std::env::set_var("MIMALLOC_LARGE_OS_PAGES", "0");
        std::env::set_var("MIMALLOC_VERBOSE", "0");
    }

    #[cfg(unix)]
    {
        // Set cooperative nice level (+5) so UI / audio / system services stay responsive
        unsafe {
            libc::setpriority(libc::PRIO_PROCESS, 0, 5);
        }

        // Remove OS-level virtual memory limits
        unsafe {
            let unlimited = libc::rlimit {
                rlim_cur: libc::RLIM_INFINITY,
                rlim_max: libc::RLIM_INFINITY,
            };
            let _ = libc::setrlimit(libc::RLIMIT_AS, &unlimited);
            let _ = libc::setrlimit(libc::RLIMIT_DATA, &unlimited);

            // Increase open file limits for LevelDB and ZipWriter
            let mut nofile = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::getrlimit(libc::RLIMIT_NOFILE, &mut nofile) == 0 {
                nofile.rlim_cur = nofile.rlim_max.min(65536);
                let _ = libc::setrlimit(libc::RLIMIT_NOFILE, &nofile);
            }
        }

        // Lower oom_score_adj to protect foreground generation process from premature lmkd SIGKILL
        let candidates = ["-1000", "-900", "-500", "0"];
        for val in &candidates {
            if let Ok(mut f) = fs::OpenOptions::new().write(true).open("/proc/self/oom_score_adj") {
                if f.write_all(format!("{val}\n").as_bytes()).is_ok() {
                    break;
                }
            }
        }
    }
}

/// Discovers the first existing, writable download directory on Android.
pub fn resolve_android_download_dir() -> PathBuf {
    let candidates = [
        PathBuf::from("/storage/emulated/0/Download"),
        PathBuf::from("/sdcard/Download"),
        PathBuf::from("/storage/self/primary/Download"),
        PathBuf::from("/storage/emulated/0"),
        PathBuf::from("/sdcard"),
        PathBuf::from("/data/data/com.louisdev.arnis/files"),
        PathBuf::from("/data/user/0/com.louisdev.arnis/files"),
    ];
    for dir in &candidates {
        if dir.is_dir() || fs::create_dir_all(dir).is_ok() {
            let test_file = dir.join(".arnis_write_probe");
            if fs::write(&test_file, b"").is_ok() {
                let _ = fs::remove_file(test_file);
                return dir.clone();
            }
        }
    }
    PathBuf::from("/sdcard/Download")
}

/// Appends a crash or diagnostic message to local Android log files across fallback directories.
pub fn write_android_crash_log(message: &str) {
    let candidates = [
        resolve_android_download_dir().join("arnis_crash.log"),
        PathBuf::from("/storage/emulated/0/Download/arnis_crash.log"),
        PathBuf::from("/sdcard/Download/arnis_crash.log"),
        PathBuf::from("/data/data/com.louisdev.arnis/cache/arnis_crash.log"),
        PathBuf::from("/data/user/0/com.louisdev.arnis/cache/arnis_crash.log"),
    ];
    for path in &candidates {
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = f.write_all(message.as_bytes());
            break;
        }
    }
}
