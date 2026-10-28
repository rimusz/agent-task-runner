use crate::config::LogConfig;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tracing_subscriber::fmt::MakeWriter;

/// When true, the console tracing layer drops writes (TUI owns the tty).
static TUI_CONSOLE_QUIET: AtomicBool = AtomicBool::new(false);

/// Silence stderr tracing while the ratatui host owns the terminal.
/// Restores the previous flag on drop so line-mode / `atr ask` still log.
pub struct TuiConsoleQuiet(bool);

impl TuiConsoleQuiet {
    pub fn enter() -> Self {
        let prev = TUI_CONSOLE_QUIET.swap(true, Ordering::Relaxed);
        Self(prev)
    }
}

impl Drop for TuiConsoleQuiet {
    fn drop(&mut self) {
        TUI_CONSOLE_QUIET.store(self.0, Ordering::Relaxed);
    }
}

pub fn tui_console_is_quiet() -> bool {
    TUI_CONSOLE_QUIET.load(Ordering::Relaxed)
}

/// Default file when `[log] path` is empty (TUI / ask still need a sink).
pub fn default_cli_log_path() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) if cfg!(target_os = "macos") => {
            PathBuf::from(home).join("Library/Logs/atr/atr.log")
        }
        Some(home) => PathBuf::from(home).join(".local/share/atr/atr.log"),
        None => PathBuf::from("atr.log"),
    }
}

/// File + gated stderr. Empty path uses [`default_cli_log_path`].
pub fn init_session_tracing(log: &LogConfig) {
    let mut log = log.clone();
    if log.path.as_os_str().is_empty() {
        log.path = default_cli_log_path();
    }
    init_tracing(Some(&log));
}

/// Stderr writer that no-ops while [`TuiConsoleQuiet`] is held.
#[derive(Clone, Copy, Debug, Default)]
struct GatedStderr;

impl Write for GatedStderr {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if TUI_CONSOLE_QUIET.load(Ordering::Relaxed) {
            return Ok(buf.len());
        }
        io::stderr().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        if TUI_CONSOLE_QUIET.load(Ordering::Relaxed) {
            return Ok(());
        }
        io::stderr().flush()
    }
}

impl<'a> MakeWriter<'a> for GatedStderr {
    type Writer = GatedStderr;

    fn make_writer(&'a self) -> Self::Writer {
        GatedStderr
    }
}

struct Inner {
    path: PathBuf,
    max_bytes: u64,
    keep: u32,
    file: Option<File>,
    written: u64,
}

impl Inner {
    fn open(cfg: &LogConfig) -> io::Result<Self> {
        if let Some(dir) = cfg.path.parent() {
            if !dir.as_os_str().is_empty() {
                fs::create_dir_all(dir)?;
            }
        }
        let written = fs::metadata(&cfg.path).map(|m| m.len()).unwrap_or(0);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&cfg.path)?;
        Ok(Self {
            path: cfg.path.clone(),
            max_bytes: cfg.max_bytes.max(1),
            keep: cfg.keep.max(1),
            file: Some(file),
            written,
        })
    }

    fn rotated_path(path: &Path, n: u32) -> PathBuf {
        let mut s = path.as_os_str().to_os_string();
        s.push(format!(".{n}"));
        PathBuf::from(s)
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file = None;
        let keep = self.keep;
        let _ = fs::remove_file(Self::rotated_path(&self.path, keep));
        for i in (1..keep).rev() {
            let from = Self::rotated_path(&self.path, i);
            let to = Self::rotated_path(&self.path, i + 1);
            if from.exists() {
                let _ = fs::rename(&from, &to);
            }
        }
        if self.path.exists() {
            let _ = fs::rename(&self.path, Self::rotated_path(&self.path, 1));
        }
        self.file = Some(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?,
        );
        self.written = 0;
        Ok(())
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        if self.written.saturating_add(buf.len() as u64) > self.max_bytes && self.written > 0 {
            self.rotate()?;
        }
        if let Some(f) = self.file.as_mut() {
            f.write_all(buf)?;
            f.flush()?;
        }
        self.written = self.written.saturating_add(buf.len() as u64);
        if self.written > self.max_bytes {
            self.rotate()?;
        }
        Ok(())
    }
}

/// Size-and-count rotating log file. Cloneable for tracing's MakeWriter.
#[derive(Clone)]
pub struct RotatingFile {
    inner: Arc<Mutex<Inner>>,
}

impl RotatingFile {
    pub fn open(cfg: &LogConfig) -> io::Result<Self> {
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner::open(cfg)?)),
        })
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(f) = self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .file
            .as_mut()
        {
            f.flush()?;
        }
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for RotatingFile {
    type Writer = RotatingFile;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

pub fn init_tracing(log: Option<&LogConfig>) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::EnvFilter;

    let env = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let stderr = tracing_subscriber::fmt::layer()
        .with_writer(GatedStderr)
        .with_target(false)
        .compact();
    if let Some(cfg) = log {
        if !cfg.path.as_os_str().is_empty() {
            if let Ok(file) = RotatingFile::open(cfg) {
                let file_layer = tracing_subscriber::fmt::layer()
                    .with_writer(file)
                    .with_ansi(false)
                    .with_target(false)
                    .compact();
                let _ = tracing_subscriber::registry()
                    .with(env)
                    .with(stderr)
                    .with(file_layer)
                    .try_init();
                return;
            }
        }
    }
    let _ = tracing_subscriber::registry()
        .with(env)
        .with(stderr)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_by_size_and_keeps_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("atr.log");
        let cfg = LogConfig {
            path: path.clone(),
            max_bytes: 32,
            keep: 2,
        };
        let mut w = RotatingFile::open(&cfg).unwrap();
        for i in 0..20 {
            writeln!(w, "line {i} xxxxxxxxxxxxxxxxxxxx").unwrap();
        }
        assert!(path.exists(), "current log");
        assert!(
            Inner::rotated_path(&path, 1).exists(),
            "first rotated file missing"
        );
        assert!(
            !Inner::rotated_path(&path, 3).exists(),
            "must not keep more than `keep` files"
        );
        let total = fs::read_to_string(&path).unwrap().len()
            + fs::read_to_string(Inner::rotated_path(&path, 1))
                .unwrap_or_default()
                .len()
            + fs::read_to_string(Inner::rotated_path(&path, 2))
                .unwrap_or_default()
                .len();
        assert!(total > 32, "writes must survive rotation ({total})");
    }

    #[test]
    fn tui_quiet_drops_console_tracing_info() {
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl Write for Buf {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                if tui_console_is_quiet() {
                    return Ok(buf.len());
                }
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for Buf {
            type Writer = Buf;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let buf = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(Buf(buf.clone()))
            .with_target(false)
            .compact()
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let _quiet = TuiConsoleQuiet::enter();
            assert!(tui_console_is_quiet());
            tracing::info!(
                task = "cli-default",
                source = "cli",
                "queued follow-up; live run not interrupted"
            );
        });
        let text = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            !text.contains("queued follow-up"),
            "TUI console must not paint INFO: {text:?}"
        );
        assert!(!tui_console_is_quiet());
    }

    #[test]
    fn default_cli_log_path_is_under_home() {
        let p = default_cli_log_path();
        let s = p.to_string_lossy();
        assert!(
            s.ends_with("atr.log"),
            "expected a file named atr.log, got {s}"
        );
    }

    #[test]
    fn log_file_has_no_ansi() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("atr.log");
        let cfg = LogConfig {
            path: path.clone(),
            max_bytes: 64 * 1024,
            keep: 2,
        };
        let file = RotatingFile::open(&cfg).unwrap();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(file)
            .with_ansi(false)
            .with_target(false)
            .compact()
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("plain line");
            tracing::warn!("a warning");
        });
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("plain line"), "{text}");
        assert!(text.contains("a warning"), "{text}");
        assert!(
            !text.contains('\u{1b}'),
            "log file must not contain ANSI colour: {text:?}"
        );
    }

    #[test]
    fn launchagent_does_not_capture_rotated_logs() {
        let text = include_str!("../contrib/dev.atr.serve.plist");
        assert!(
            text.contains("<string>/dev/null</string>"),
            "stdout must go to /dev/null when [log] path is set"
        );
        assert!(
            text.contains("atr.boot.err"),
            "keep only a tiny stderr for crashes before tracing"
        );
        assert!(
            !text.contains("<string>__HOME__/Library/Logs/atr.log</string>"),
            "must not tee the rotated log through launchd"
        );
        assert!(
            !text.contains("<string>__HOME__/Library/Logs/atr.err.log</string>"),
            "must not tee unrotated stderr through launchd"
        );
        assert!(
            text.contains("__HOME__/.config/atr/atr-serve.sh"),
            "plist must run the launcher so secrets stay out of launchd"
        );
        assert!(
            !text.contains("<string>serve</string>"),
            "plist must not invoke atr serve directly"
        );
        assert!(
            !text.contains("TOKEN"),
            "plist must not embed token env keys"
        );
    }

    #[test]
    fn systemd_user_unit_runs_launcher() {
        let text = include_str!("../contrib/atr.service");
        assert!(
            text.contains("ExecStart=%h/.config/atr/atr-serve.sh"),
            "unit must run the launcher so secrets stay out of the unit file"
        );
        assert!(
            !text.contains("TOKEN"),
            "unit must not embed token env keys"
        );
        assert!(
            !text.contains("atr serve"),
            "unit must not invoke atr serve directly"
        );
    }

    #[test]
    fn atr_serve_sources_shadow_env_then_execs() {
        let text = include_str!("../contrib/atr-serve.sh");
        assert!(text.contains("set -a"));
        assert!(text.contains(".config/atr/shadow.env"));
        assert!(text.contains("exec atr serve"));

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join(".config/atr")).unwrap();
        std::fs::write(
            home.join(".config/atr/shadow.env"),
            "ATR_SHADOW_GH_TOKEN=from-env-file\n",
        )
        .unwrap();
        let bin = home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let atr = bin.join("atr");
        std::fs::write(
            &atr,
            "#!/bin/sh\nif [ \"$1\" = serve ]; then printf '%s\\n' \"${ATR_SHADOW_GH_TOKEN:-}\"; exit 0; fi\nexit 1\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&atr, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("contrib/atr-serve.sh");
        let out = std::process::Command::new("sh")
            .arg(&script)
            .env("HOME", home)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env_remove("ATR_SHADOW_GH_TOKEN")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "from-env-file");
    }
}
