//! Shared transport settings for the five HTTP quota collectors.

use std::time::Duration;

const PROXY_ENV: [&str; 7] = [
    "HERDR_AGENT_QUOTA_HTTP_PROXY",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "HTTP_PROXY",
    "http_proxy",
];

pub(super) fn agent_builder() -> ureq::AgentBuilder {
    let mut builder = ureq::AgentBuilder::new()
        // Resolve precedence ourselves; `off` must also override inherited env.
        .try_proxy_from_env(false)
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(Duration::from_secs(10))
        .timeout_write(Duration::from_secs(10));
    if let Some(proxy) = configured_proxy(crate::prefs::read(crate::prefs::HTTP_PROXY), |name| {
        std::env::var(name).ok()
    }) {
        builder = builder.proxy(proxy);
    }
    builder
}

fn configured_proxy(
    file: Option<String>,
    env: impl Fn(&str) -> Option<String>,
) -> Option<ureq::Proxy> {
    let value = file.filter(|value| !value.trim().is_empty()).or_else(|| {
        PROXY_ENV
            .iter()
            .find_map(|name| env(name).filter(|value| !value.trim().is_empty()))
    })?;
    let value = value.trim();
    if value.eq_ignore_ascii_case("off") || value.chars().any(char::is_whitespace) {
        return None;
    }
    // Proxy::new accepts malformed ports and hosts. Use ureq's URL parser to
    // validate first (no request is sent), then pass a normalized HTTP URL.
    let parsed = ureq::get(value).request_url().ok()?;
    let url = parsed.as_url();
    if url.scheme() != "http"
        || url.host_str()?.contains(':') // ureq 2's proxy parser cannot handle IPv6.
        || url.port() == Some(0)
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    // Invalid/unsupported configuration means direct, without logging a URL
    // that may contain proxy credentials or trying a lower-priority proxy.
    ureq::Proxy::new(url.as_str()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_transport_reaches_proxy_or_direct_endpoint() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        use std::process::Command;
        use std::time::Instant;

        const TARGET: &str = "QUOTA_TEST_HTTP_TARGET";
        if let Ok(target) = std::env::var(TARGET) {
            let result = agent_builder()
                .build()
                .get(&target)
                .set("Authorization", "Bearer test-provider-secret")
                .call();
            if target.starts_with("https:") {
                assert!(result.is_err()); // The local proxy deliberately rejects CONNECT.
            } else {
                assert_eq!(result.unwrap().status(), 200);
            }
            return;
        }

        for mode in ["file", "env", "off", "invalid", "unset"] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let proxied = matches!(mode, "file" | "env");
            let server = std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(15);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(e) => panic!("local test server: {e}"),
                    }
                };
                // Windows accepted sockets inherit the listener's nonblocking mode.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut headers = String::new();
                let mut reader = BufReader::new(&mut stream);
                loop {
                    let mut line = String::new();
                    assert!(reader.read_line(&mut line).unwrap() > 0);
                    headers.push_str(&line);
                    if line == "\r\n" {
                        break;
                    }
                }
                if proxied {
                    assert!(headers.starts_with("CONNECT quota.invalid:443 HTTP/1.1\r\n"));
                    assert!(headers.contains("Proxy-Authorization: basic "));
                    assert!(!headers.contains("test-provider-secret"));
                    stream.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n").unwrap();
                } else {
                    assert!(headers.starts_with("GET /usage HTTP/1.1\r\n"));
                    assert!(!headers.contains("Proxy-Authorization"));
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                        .unwrap();
                }
            });
            let config = tempfile::tempdir().unwrap();
            let proxy = format!("http://test-user:test-password@{address}");
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "providers::http::tests::configured_transport_reaches_proxy_or_direct_endpoint",
                ])
                .env("HERDR_PLUGIN_CONFIG_DIR", config.path())
                .env(
                    TARGET,
                    if proxied {
                        "https://quota.invalid/usage".into()
                    } else {
                        format!("http://{address}/usage")
                    },
                );
            for name in PROXY_ENV {
                command.env_remove(name);
            }
            match mode {
                "file" => {
                    std::fs::write(config.path().join(crate::prefs::HTTP_PROXY), &proxy).unwrap();
                    command.env("HERDR_AGENT_QUOTA_HTTP_PROXY", "http://127.0.0.1:1");
                }
                "env" => {
                    command.env("HTTPS_PROXY", &proxy);
                }
                "off" | "invalid" => {
                    std::fs::write(
                        config.path().join(crate::prefs::HTTP_PROXY),
                        if mode == "off" {
                            "off"
                        } else {
                            "http://localhost:bad"
                        },
                    )
                    .unwrap();
                    command.env("HTTPS_PROXY", "http://127.0.0.1:1");
                }
                _ => {}
            }
            crate::platform::detach_process_group(&mut command);
            let output = command.output().unwrap();
            server.join().unwrap();
            assert!(output.status.success(), "{mode}: {output:?}");
            assert!(!String::from_utf8_lossy(&output.stdout).contains("test-password"));
            assert!(!String::from_utf8_lossy(&output.stderr).contains("test-password"));
        }
    }

    #[test]
    fn proxy_precedence_and_invalid_values() {
        let expected = |port| ureq::Proxy::new(format!("http://localhost:{port}")).ok();
        let values: Vec<_> = PROXY_ENV
            .iter()
            .enumerate()
            .map(|(i, name)| (*name, format!("http://localhost:{}", 8000 + i)))
            .collect();
        let lookup = |name: &str| values.iter().find(|v| v.0 == name).map(|v| v.1.clone());
        assert_eq!(
            configured_proxy(Some(" http://localhost:9000\n".into()), lookup),
            expected(9000)
        );
        for start in 0..PROXY_ENV.len() {
            assert_eq!(
                configured_proxy(None, |name| {
                    values[start..]
                        .iter()
                        .find(|v| v.0 == name)
                        .map(|v| v.1.clone())
                }),
                expected(8000 + start)
            );
        }
        assert_eq!(configured_proxy(Some(" \n".into()), lookup), expected(8000));
        assert_eq!(configured_proxy(None, |_| Some(" ".into())), None);
        assert_eq!(configured_proxy(None, |_| None), None);
        for value in [
            "off",
            " OFF ",
            "garbage",
            "http://",
            "http://localhost:bad",
            "http://localhost:65536",
            "http://localhost:0",
            "http://local host:8080",
            "http://localhost/path",
            "http://localhost?x=1",
            "http://localhost#x",
            "https://localhost:8080",
            "socks5://localhost:1080",
            "http://[::1]:8080",
        ] {
            assert_eq!(
                configured_proxy(Some(value.into()), lookup),
                None,
                "{value}"
            );
            assert_eq!(
                configured_proxy(None, |name| if name == PROXY_ENV[0] {
                    Some(value.into())
                } else {
                    lookup(name)
                }),
                None,
                "{value}"
            );
        }
    }
}
