//! TEMPORARY review probe — delete before committing.
use lk_core::markdown::{CredentialForm, scan_credentials};

fn forms(line: &str) -> Vec<CredentialForm> {
    scan_credentials(line).into_iter().map(|(_, f)| f).collect()
}

/// Deterministic base64url stream.
struct Rng(u64);
impl Rng {
    fn next(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) as usize) % n
    }
}

const B64URL: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

#[test]
fn real_key_miss_rate_under_min_unbroken_run_20() {
    let mut rng = Rng(0xC0FFEE);
    // Anthropic: sk-ant-api03- + 95 chars of base64url (the width the issuer mints).
    for (label, prefix, body_len) in [
        ("sk-ant-api03- (95)", "sk-ant-api03-", 95usize),
        ("sk-ant-api03- (40)", "sk-ant-api03-", 40),
        ("sk-ant-api03- (24, floor)", "sk-ant-api03-", 24),
        ("sk-proj- (95)", "sk-proj-", 95),
        ("pypi- (140)", "pypi-", 140),
    ] {
        let (mut miss, trials) = (0usize, 20_000);
        for _ in 0..trials {
            let body: String = (0..body_len)
                .map(|_| B64URL[rng.next(B64URL.len())] as char)
                .collect();
            if forms(&format!("{prefix}{body}")).is_empty() {
                miss += 1;
            }
        }
        println!(
            "{label:<28} missed {miss}/{trials}  ({:.2}%)",
            miss as f64 * 100.0 / trials as f64
        );
    }
}

#[test]
fn slack_real_shapes() {
    // Slack's published shapes; the secret segment length is what min_unbroken_run 20 tests.
    let cases: Vec<(&str, String)> = vec![
        ("xoxb 24-char secret", format!("xoxb-{}-{}-{}", "123456789012", "1234567890123", "a".repeat(24))),
        ("xoxb 16-char secret", format!("xoxb-{}-{}-{}", "123456789012", "1234567890123", "a".repeat(16))),
        ("xoxb 19-char secret", format!("xoxb-{}-{}-{}", "123456789012", "1234567890123", "a".repeat(19))),
        ("xoxp 32-char secret", format!("xoxp-{}-{}-{}-{}", "12345678901", "12345678901", "123456789012", "a".repeat(32))),
        ("xapp 64-hex secret", format!("xapp-1-A0123456789-1234567890123-{}", "b".repeat(64))),
        ("xapp 16-char secret", format!("xapp-1-A0123456789-1234567890123-{}", "b".repeat(16))),
        ("xoxe-1 rotating", format!("xoxe-1-My{}", "c".repeat(30))),
        ("xoxe.xoxb rotating", format!("xoxe.xoxb-1-{}", "d".repeat(140))),
        ("xoxb hyphenated secret <20", format!("xoxb-1-2-{}", "ee-ff-gg-hh-ii-jj-kk-ll-mm-nn")),
    ];
    for (label, line) in cases {
        println!("{label:<28} {:?}", forms(&line));
    }
}

#[test]
fn remaining_false_positives() {
    let cases: Vec<(&str, String)> = vec![
        // Placeholders / redactions in docs — >=20 unbroken run.
        ("anthropic placeholder x32", format!("ANTHROPIC_API_KEY=sk-ant-api03-{}", "x".repeat(32))),
        ("anthropic YOUR_KEY_HERE", "set sk-ant-api03-YOUR_API_KEY_GOES_HERE in the env".into()),
        ("slack redacted", "token: xoxb-000000000000-000000000000-REDACTED_TOKEN_VALUE_HERE".into()),
        ("slack all-zero example", format!("xoxb-{}-{}-{}", "000000000000", "000000000000", "0".repeat(24))),
        // Prose the earlier version false-positived on.
        ("prose anthropic", "we are moving to sk-ant-api03-scoped-keys-per-service-this-quarter".into()),
        ("prose slack", "the runbook is at xoxb-2026-q1-token-rotation-plan-for-the-workspace".into()),
        ("prose xapp", "xapp-2-level-token-rotation-runbook-lives-in-the-ops-wiki-page".into()),
        // Long single-token identifiers after a prefix.
        ("slack + long identifier", "see xoxb-2026-migrationrunbookforsharedworkspace".into()),
        ("anthropic + long ident", "see sk-ant-api03-migrationrunbookforsharedworkspaces".into()),
        // Ordered-list marker handling.
        ("ordered marker pem", "1. -----BEGIN RSA PRIVATE KEY-----".into()),
        ("ordered marker paren", "12) -----BEGIN DSA PRIVATE KEY-----".into()),
        ("nested ordered/quote", "> 1. - -----BEGIN EC PRIVATE KEY-----".into()),
        ("date-ish line", "2026. -----BEGIN RSA PRIVATE KEY----- was pasted".into()),
        ("no space after dot", "1.-----BEGIN RSA PRIVATE KEY-----".into()),
        ("table cell pem", "| step | -----BEGIN RSA PRIVATE KEY----- |".into()),
    ];
    for (label, line) in cases {
        println!("{label:<28} {:?}   << {line}", forms(&line));
    }
}
