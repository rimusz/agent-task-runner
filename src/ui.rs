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

/// The dashboard link to share with other devices, and whether it is
/// local-only. Order: `[dashboard] public_url`, then a Tailscale Serve
/// HTTPS URL proxying to the `listen` port, then the local `atr ui` URL.
pub fn shareable_url(public_url: &str, listen: &str) -> (String, bool) {
    let p = public_url.trim();
    if !p.is_empty() {
        return (p.to_string(), false);
    }
    let port = listen_port(listen);
    if let Some(u) = tailscale_serve_json().and_then(|j| serve_url_from_json(&j, &port)) {
        return (u, false);
    }
    (dashboard_url(listen), true)
}

fn listen_port(listen: &str) -> String {
    match listen.trim().rsplit_once(':') {
        Some((_, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => p.to_string(),
        _ => "7420".into(),
    }
}

fn tailscale_serve_json() -> Option<String> {
    for cli in [
        "tailscale",
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    ] {
        let Ok(out) = std::process::Command::new(cli)
            .args(["serve", "status", "--json"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
        else {
            continue;
        };
        if out.status.success() {
            return Some(String::from_utf8_lossy(&out.stdout).into_owned());
        }
    }
    None
}

/// `https://<host>/` for the first Serve web handler that proxies to `port`
/// on loopback, from `tailscale serve status --json`.
pub fn serve_url_from_json(json: &str, port: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let web = v.get("Web")?.as_object()?;
    let mut hosts: Vec<&String> = web.keys().collect();
    hosts.sort();
    for host in hosts {
        let handlers = web[host].get("Handlers").and_then(|h| h.as_object());
        let Some(handlers) = handlers else { continue };
        let hits = handlers.values().any(|h| {
            h.get("Proxy").and_then(|p| p.as_str()).is_some_and(|p| {
                let p = p.trim_end_matches('/');
                ["127.0.0.1", "localhost", "[::1]"]
                    .iter()
                    .any(|lo| p.ends_with(&format!("{lo}:{port}")))
                    || p == port
            })
        });
        if !hits {
            continue;
        }
        let name = host.strip_suffix(":443").unwrap_or(host);
        return Some(format!("https://{name}/"));
    }
    None
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

    #[test]
    fn serve_url_from_status_json() {
        let j = r#"{"TCP":{"443":{"HTTPS":true}},"Web":{"ai-stack.tail1.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:7420"}}}}}"#;
        assert_eq!(
            super::serve_url_from_json(j, "7420").as_deref(),
            Some("https://ai-stack.tail1.ts.net/")
        );
        assert_eq!(super::serve_url_from_json(j, "9999"), None);
        assert_eq!(super::serve_url_from_json("{}", "7420"), None);
        assert_eq!(super::serve_url_from_json("not json", "7420"), None);
    }

    #[test]
    fn shareable_url_prefers_public_url() {
        assert_eq!(
            super::shareable_url(" https://x.ts.net/ ", "127.0.0.1:7420"),
            ("https://x.ts.net/".to_string(), false)
        );
    }
}
