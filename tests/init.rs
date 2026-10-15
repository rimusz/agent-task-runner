use atr::config::load_config;
use atr::init::{run, InitOpts};
use std::fs;

#[test]
fn init_yes_in_temp_home_writes_files() {
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("ATR_SKILL_HOME", dir.path());
    // Skills are only written for installed CLIs: provide a fake claude.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.path().join("fakebin");
        fs::create_dir_all(&bin).unwrap();
        let p = bin.join("claude");
        fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        let old = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(&old));
        std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
    }
    let cfg_path = dir.path().join("config.toml");
    let text = run(InitOpts {
        yes: true,
        force: true,
        no_service: true,
        force_service: false,
        config_path: cfg_path.clone(),
    })
    .unwrap();
    assert!(cfg_path.exists(), "{text}");
    assert!(dir.path().join("system-prompt.md").exists());
    assert!(dir.path().join("atr-serve.sh").exists());
    let raw = fs::read_to_string(&cfg_path).unwrap();
    for banned in ["acme", "for:legacy", "api_key", "apikey"] {
        assert!(
            !raw.to_ascii_lowercase().contains(banned),
            "generated config must stay neutral, found {banned} in\n{raw}"
        );
    }
    let cfg = load_config(&cfg_path).unwrap();
    let names: Vec<_> = cfg.providers.iter().map(|p| p.name.as_str()).collect();
    assert!(names.contains(&"claude"), "{names:?}");
    assert!(names.contains(&"codex"), "{names:?}");
    assert!(raw.contains("[[provider]]"));
    assert!(raw.contains("[telegram.assistant]"), "{raw}");
    assert!(raw.contains("[dashboard]"), "{raw}");
    assert!(raw.contains("bind = \"127.0.0.1\""), "{raw}");
    assert!(raw.contains("label = \"for:atr\""), "{raw}");
    assert_eq!(cfg.github.label, "for:atr");
    assert!(
        !cfg.github.enabled,
        "default label must not enable the poller"
    );
    let skill = dir.path().join(".claude/skills/atr/SKILL.md");
    assert!(skill.exists(), "{text}");
    let skill_text = fs::read_to_string(&skill).unwrap();
    assert!(skill_text.contains("atr init"), "{skill_text}");
    assert!(skill_text.contains("kimi"), "{skill_text}");
    assert_eq!(skill_text, atr::skill::SKILL_MD);
    assert!(raw.contains("[mcp]"), "{raw}");
    assert!(raw.contains("ATR_MCP_TOKEN"), "{raw}");
    let token_path = dir.path().join("mcp.token");
    assert!(token_path.exists(), "{text}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&token_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "mcp.token must be 0600");
    }
    let token = fs::read_to_string(&token_path).unwrap();
    let token = token.trim();
    assert!(!token.is_empty(), "mcp token file empty");
    assert!(
        !text.contains(token),
        "atr init must never print the MCP token:\n{text}"
    );
    assert!(
        text.to_ascii_lowercase().contains("never prints") || text.contains("mcp token written"),
        "{text}"
    );
    assert_eq!(cfg.mcp.token_file, token_path);
}

#[test]
fn init_force_keeps_existing_mcp_token() {
    let dir = tempfile::tempdir().unwrap();
    // No ATR_SKILL_HOME: it is process-global and races the other init test;
    // a non-default config dir skips skill install anyway.
    let cfg_path = dir.path().join("config.toml");
    let opts = || InitOpts {
        yes: true,
        force: true,
        no_service: true,
        force_service: false,
        config_path: cfg_path.clone(),
    };
    run(opts()).unwrap();
    let token_path = dir.path().join("mcp.token");
    let first = fs::read_to_string(&token_path).unwrap();
    let text = run(opts()).unwrap();
    assert_eq!(
        fs::read_to_string(&token_path).unwrap(),
        first,
        "re-running init must not rotate an existing token"
    );
    assert!(!text.contains(first.trim()), "init printed the token");
    assert!(text.contains("not shown"), "{text}");
    assert_eq!(load_config(&cfg_path).unwrap().mcp.token_file, token_path);
}
