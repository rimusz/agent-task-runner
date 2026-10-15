//! Regression: `atr init --config <dir>/config.toml` must not install skills
//! into the user's real CLI dirs, and must point system_prompt_file at the
//! custom dir. Own test binary: it changes HOME and ATR_SKILL_HOME.

use atr::init::{run, InitOpts};

#[test]
fn init_custom_config_dir_skips_skill_install_and_uses_local_prompt() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", home.path());
    std::env::remove_var("ATR_SKILL_HOME");
    std::env::remove_var("ATR_CONFIG");
    std::env::set_var("ATR_INIT_SKIP_LOAD", "1");
    let side = tempfile::tempdir().unwrap();
    let cfg_path = side.path().join("config.toml");
    let text = run(InitOpts {
        yes: true,
        force: true,
        no_service: true,
        force_service: false,
        config_path: cfg_path.clone(),
    })
    .unwrap();
    assert!(text.contains("skill install skipped"), "{text}");
    for d in [".claude", ".codex", ".grok", ".cursor"] {
        assert!(
            !home.path().join(d).join("skills").exists(),
            "skill written into {d}: {text}"
        );
    }
    let cfg = atr::config::load_config(&cfg_path).unwrap();
    assert_eq!(
        cfg.system_prompt_file.canonicalize().unwrap(),
        side.path().join("system-prompt.md").canonicalize().unwrap()
    );
    // Re-running without --force must not clobber the config.
    let again = run(InitOpts {
        yes: true,
        force: false,
        no_service: true,
        force_service: false,
        config_path: cfg_path.clone(),
    });
    if let Ok(t) = again {
        assert!(cfg_path.exists(), "{t}");
    }
}
