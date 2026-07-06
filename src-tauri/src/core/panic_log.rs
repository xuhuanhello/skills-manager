use std::backtrace::Backtrace;
use std::fs;
use std::io::Write;
use std::panic;
use std::path::PathBuf;
use std::sync::{Once, OnceLock};

use tauri::{AppHandle, Manager};

static LOG_DIR: OnceLock<PathBuf> = OnceLock::new();
static HOOK_INSTALLED: Once = Once::new();

pub fn last_panic_path(app: &AppHandle) -> Option<PathBuf> {
    LOG_DIR
        .get()
        .cloned()
        .or_else(|| app.path().app_log_dir().ok())
        .map(|dir| dir.join("last_panic.log"))
}

/// Install a panic hook *before* the Tauri runtime exists, writing crashes to
/// a directory we can resolve without an `AppHandle` (the central repo's logs
/// dir). This closes the window where a panic during `initialize_store` — e.g.
/// a corrupt DB or a metadata/skills mismatch produced by multi-machine git
/// sync — would otherwise leave no `last_panic.log` at all because the real
/// hook isn't installed until `tauri::Builder::setup`. Idempotent: the later
/// [`install_panic_hook`] only refines the log directory.
pub fn install_early_panic_hook(log_dir: PathBuf) {
    let _ = fs::create_dir_all(&log_dir);
    let _ = LOG_DIR.set(log_dir);
    set_panic_hook();
}

pub fn install_panic_hook(app: AppHandle) {
    if let Ok(dir) = app.path().app_log_dir() {
        let _ = fs::create_dir_all(&dir);
        // No-op when the early hook already claimed LOG_DIR; keeps a single,
        // stable location that `last_panic_path` reads back.
        let _ = LOG_DIR.set(dir);
    }
    set_panic_hook();
}

/// Register the panic hook exactly once, regardless of how many times the
/// early/late installers are called.
fn set_panic_hook() {
    HOOK_INSTALLED.call_once(|| {
        install_hook_body();
    });
}

fn install_hook_body() {
    let prev = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let backtrace = Backtrace::capture();
        let timestamp = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%:z");
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".into());
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "<non-string panic payload>".to_string()
        };

        let body = format!(
            "[{timestamp}] PANIC at {location}\n{payload}\n\nBacktrace:\n{backtrace}\n"
        );

        log::error!("panic: {payload} at {location}");

        if let Some(dir) = LOG_DIR.get() {
            let path = dir.join("last_panic.log");
            if let Ok(mut f) = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&path)
            {
                let _ = f.write_all(body.as_bytes());
            }
        }

        prev(info);
    }));
}
