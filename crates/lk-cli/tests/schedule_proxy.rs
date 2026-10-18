//! Schedule generation preserves only explicitly selected network environment.
use std::process::Command;

const KEYS: [&str; 8] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
];

fn generate(format: &str, values: &[(&str, &std::ffi::OsStr)]) -> std::process::Output {
    generate_with_extra(format, values, "")
}

fn generate_with_extra(
    format: &str,
    values: &[(&str, &std::ffi::OsStr)],
    extra: &str,
) -> std::process::Output {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("config.yaml"),
        "vault:\n  root: vault\nidentity:\n  name: Test\n  email: test@example.invalid\ningest:\n  schedule: '0 9 * * *'\nsources:\n  notes:\n    type: manual\n    params:\n      inbox_dir: inbox\n").unwrap();
    if !extra.is_empty() {
        let config = root.path().join("config.yaml");
        let mut body = std::fs::read_to_string(&config).unwrap();
        body.push_str(extra);
        std::fs::write(config, body).unwrap();
    }
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lore"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("LORE_") {
            cmd.env_remove(key);
        }
    }
    cmd.env_remove("CLAUDE_CONFIG_DIR");
    for key in KEYS {
        cmd.env_remove(key);
    }
    for (key, value) in values {
        cmd.env(key, value);
    }
    let output = cmd
        .current_dir(root.path())
        .args([
            "schedule",
            "--format",
            format,
            "--bin",
            env!("CARGO_BIN_EXE_lore"),
        ])
        .env("ANTHROPIC_API_KEY", "never-export-this")
        .output()
        .unwrap();
    assert!(!root.path().join("vault").exists());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    output
}

#[test]
fn explicit_proxy_values_keep_case_empty_and_escape_in_both_formats() {
    for format in ["cron", "launchd"] {
        let values = [
            ("HTTP_PROXY", "http://upper.invalid:8080"),
            ("HTTPS_PROXY", "https://secure.invalid:443/"),
            ("ALL_PROXY", "socks5h://socks.invalid:1080"),
            ("NO_PROXY", "localhost,.example.invalid,10.0.0.0/8,&<'\""),
            ("http_proxy", "http://lower.invalid:8081"),
            ("https_proxy", ""),
            ("all_proxy", ""),
            ("no_proxy", "*"),
        ];
        let borrowed: Vec<_> = values
            .iter()
            .map(|(k, v)| (*k, std::ffi::OsStr::new(v)))
            .collect();
        let out = generate(format, &borrowed);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = String::from_utf8(out.stdout).unwrap();
        for (key, _) in values {
            assert!(text.contains(key));
        }
        assert!(text.contains("upper.invalid:8080"));
        assert!(text.contains("lower.invalid:8081"));
        assert!(!text.contains("never-export-this"));
        if format == "launchd" {
            let mut documents = 0;
            for document in text.split("<?xml").skip(1) {
                let environment = document
                    .split_once("<key>EnvironmentVariables</key>")
                    .expect("direct job has proxy environment")
                    .1;
                let environment = environment
                    .split_once("<dict>")
                    .unwrap()
                    .1
                    .split_once("</dict>")
                    .unwrap()
                    .0;
                let actual: std::collections::BTreeMap<_, _> = environment
                    .lines()
                    .filter_map(|line| line.trim().strip_prefix("<key>"))
                    .map(|line| {
                        let (key, rest) = line.split_once("</key><string>").unwrap();
                        let value = rest
                            .strip_suffix("</string>")
                            .unwrap()
                            .replace("&lt;", "<")
                            .replace("&gt;", ">")
                            .replace("&#13;", "\r")
                            .replace("&#10;", "\n")
                            .replace("&#9;", "\t")
                            .replace("&amp;", "&");
                        (key.to_owned(), value)
                    })
                    .collect();
                let expected: std::collections::BTreeMap<_, _> = values
                    .iter()
                    .map(|(key, value)| (key.to_string(), value.to_string()))
                    .collect();
                assert_eq!(
                    actual, expected,
                    "direct job retains exactly all eight raw values"
                );
                documents += 1;
            }
            assert!(documents > 0);
        } else {
            assert!(text.contains("https_proxy=''"));
            assert!(text.contains("'\\''"));
        }
        let absent = generate(format, &[]);
        assert!(absent.status.success());
        let text = String::from_utf8(absent.stdout).unwrap();
        for key in KEYS {
            assert!(!text.contains(key));
        }
    }
}

#[test]
fn invalid_proxy_endpoints_refuse_without_value_or_partial_output() {
    for format in ["cron", "launchd"] {
        for value in [
            "http://private-user:private-pass@proxy.invalid",
            "http://@private-host",
            "http://proxy.invalid/?private-query",
            "http://proxy.invalid/#private-fragment",
            "http://proxy.invalid/private-path",
            "private-host:8080",
            "http:private-host",
            " http://private-host",
            "http://proxy.invalid\nprivate-control",
        ] {
            let out = generate(format, &[("HTTP_PROXY", value.as_ref())]);
            assert!(!out.status.success());
            assert!(out.stdout.is_empty());
            let error = String::from_utf8_lossy(&out.stderr);
            assert!(error.contains("HTTP_PROXY"));
            assert!(!error.contains("private-"));
        }
        if format == "launchd" {
            for value in ["private-\u{fffe}", "private-\u{ffff}"] {
                let out = generate(format, &[("NO_PROXY", value.as_ref())]);
                assert!(!out.status.success());
                assert!(out.stdout.is_empty());
                assert!(!String::from_utf8_lossy(&out.stderr).contains("private-"));
            }
        }
        let out = generate(
            format,
            &[("NO_PROXY", "localhost\nprivate-control".as_ref())],
        );
        assert!(!out.status.success());
        assert!(out.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&out.stderr).contains("private-control"));
    }
}

#[cfg(unix)]
#[test]
fn non_utf8_proxy_refuses_without_lossy_values() {
    use std::os::unix::ffi::OsStrExt;
    let out = generate(
        "cron",
        &[("http_proxy", std::ffi::OsStr::from_bytes(b"private-\xff"))],
    );
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&out.stderr).contains("private-"));
}

#[test]
fn unsupported_later_launchd_calendar_emits_no_earlier_job_or_header() {
    let out = generate_with_extra(
        "launchd",
        &[],
        "\nsynthesis:\n  weekly:\n    enabled: true\n    schedule: '*/5 * * * *'\n",
    );
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("launchd"));
}
