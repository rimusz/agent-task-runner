//! `atr ui`: open the local dashboard in a browser.

use std::time::Duration;

/// Dashboard URL for a `listen` value such as `127.0.0.1:7420` or `0.0.0.0:7420`.
/// Wildcard binds are reached through loopback.
pub fn dashboard_url(listen: &str) -> String {
    let listen = listen.trim();
    let (host, port) = match listen.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => (h, p),
        _ if listen.is_empty() => ("", "7420"),
        _ => (listen, "7420"),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let host = match host {
        "" | "0.0.0.0" | "::" | "localhost" => "127.0.0.1".to_string(),
        h if h.contains(':') => format!("[{h}]"),
        h => h.to_string(),
    };
    format!("http://{host}:{port}/")
}

fn opener() -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        return Some("open");
    }
    let has_display = std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty())
        || std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty());
    if !has_display {
        return None;
    }
    Some("xdg-open")
}

fn is_ssh() -> bool {
    std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some()
}

/// Returns the lines to print. Opens a browser unless `print_only` or headless.
pub fn run(listen: &str, print_only: bool) -> String {
    let url = dashboard_url(listen);
    if print_only {
        return url;
    }
    let up = ureq::get(&format!("{url}health"))
        .timeout(Duration::from_secs(2))
        .call()
        .is_ok();
    if !up {
        return format!(
            "{url}\natr is not running at {url} — start it with `atr serve` or `atr service install`"
        );
    }
    let opener = if cfg!(target_os = "macos") || !is_ssh() {
        opener()
    } else {
        None
    };
    match opener {
        Some(cmd) => match std::process::Command::new(cmd)
            .arg(&url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
        {
            Ok(s) if s.success() => format!("opened {url}"),
            _ => format!("{url}\n(could not run {cmd}; open the URL yourself)"),
        },
        None => url,
    }
}

#[cfg(test)]
mod tests {
    use super::dashboard_url;

    #[test]
    fn url_from_listen() {
        assert_eq!(dashboard_url("127.0.0.1:7420"), "http://127.0.0.1:7420/");
        assert_eq!(dashboard_url("0.0.0.0:7421"), "http://127.0.0.1:7421/");
        assert_eq!(dashboard_url("[::]:7420"), "http://127.0.0.1:7420/");
        assert_eq!(dashboard_url("localhost:8000"), "http://127.0.0.1:8000/");
        assert_eq!(dashboard_url("100.64.0.5:7420"), "http://100.64.0.5:7420/");
        assert_eq!(dashboard_url("[::1]:7420"), "http://[::1]:7420/");
        assert_eq!(dashboard_url(""), "http://127.0.0.1:7420/");
    }
    #[test]
    fn print_only_never_probes() {
        assert_eq!(super::run("127.0.0.1:7420", true), "http://127.0.0.1:7420/");
    }

    #[test]
    fn not_running_prints_hint() {
        // Grab a free port, then release it so nothing listens there.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let out = super::run(&format!("127.0.0.1:{port}"), false);
        assert!(
            out.starts_with(&format!("http://127.0.0.1:{port}/")),
            "{out}"
        );
        assert!(out.contains("not running"), "{out}");
    }
}
