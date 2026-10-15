//! `atr service install` writes its files under $HOME without loading the
//! job (ATR_INIT_SKIP_LOAD=1). Own test binary: it changes HOME.

#[test]
fn service_install_dry_run_writes_launcher_and_unit_under_home() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", home.path());
    std::env::set_var("ATR_INIT_SKIP_LOAD", "1");
    std::env::remove_var("ATR_CONFIG");

    let out = atr::service::install(None, true, None).unwrap();

    let launcher = home.path().join(".config/atr/atr-serve.sh");
    let sh = std::fs::read_to_string(&launcher).unwrap();
    assert!(!sh.contains("__ATR_BIN__"), "{sh}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&launcher).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }
    if cfg!(target_os = "macos") {
        let plist = home.path().join("Library/LaunchAgents/dev.atr.serve.plist");
        let text = std::fs::read_to_string(&plist).unwrap();
        assert!(text.contains("dev.atr.serve"), "{text}");
        assert!(
            !text.contains("__HOME__") && !text.contains("__LABEL__"),
            "{text}"
        );
        assert!(out.contains("installed LaunchAgent"), "{out}");
    } else {
        let unit = home.path().join(".config/systemd/user/atr.service");
        let text = std::fs::read_to_string(&unit).unwrap();
        assert!(text.contains("[Service]"), "{text}");
        assert!(out.contains("installed systemd user unit"), "{out}");
    }
    // Idempotent: a second install rewrites the same files.
    atr::service::install(None, true, None).unwrap();
}
