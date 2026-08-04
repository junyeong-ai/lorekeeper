//! Markdown structure primitives shared across crates: fenced-code tracking,
//! heading demotion, and the vault-text cleanliness contract. Single source of
//! truth — `lk-vault::section` (section locate/replace) and
//! `lk-pipeline::normalize` (source-body sanitisation) build on the fence/heading
//! parsing instead of re-implementing it, and the `scan_defects` contract is shared
//! by the converters that uphold it, the property tests that assert it, and
//! `lore doctor` that checks it on pages at rest.

/// A way a materialized vault page violates the text-cleanliness contract the
/// pipeline guarantees. Each variant names an EXACT property — never a heuristic
/// guess — so a hit is always a real defect: text no honest converter could emit,
/// or a page written before the guarantee existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextDefect {
    /// An inlined `data:` URI. Every rich-text converter degrades these to alt text
    /// because an embedded base64 payload is encoded bytes, not retrievable
    /// knowledge, and bloats both the page and every LLM task that reads it.
    InlineDataUri,
}

impl TextDefect {
    /// One-line human description for `lore doctor` output.
    pub fn description(self) -> &'static str {
        match self {
            TextDefect::InlineDataUri => {
                "inlined data: URI — encoded bytes, not knowledge; converters strip these"
            }
        }
    }
}

/// Scan a materialized page's text for cleanliness-contract violations, returning
/// each defect with its 1-based line number (empty result = clean). The SINGLE
/// SOURCE OF TRUTH for the contract: the rich-text converters uphold it
/// (`lk_source::markdown::html_to_markdown` degrades data: URIs to alt text),
/// property tests assert it on converter output, and `lore doctor` checks it on
/// pages at rest — all three call THIS, so they can never disagree about what
/// "clean vault text" means. New invariants are added as `TextDefect` variants,
/// extending every enforcement point at once.
pub fn scan_defects(text: &str) -> Vec<(usize, TextDefect)> {
    text.lines()
        .enumerate()
        .filter(|(_, line)| has_inline_data_uri(line))
        .map(|(i, _)| (i + 1, TextDefect::InlineDataUri))
        .collect()
}

/// Exact `data:`-URI signatures with zero false positives on prose: a markdown
/// image/link target (`](data:`) and an autolink (`<data:`) — the only shapes a
/// converter could emit. Matched case-insensitively to mirror the conversion-time
/// `lk_source::markdown` `is_data_uri` check (which strips `DATA:` too), so the
/// checker and the converter never disagree about a page's cleanliness.
fn has_inline_data_uri(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("](data:") || lower.contains("<data:")
}

/// A string in a namespace its issuer RESERVED for credentials.
///
/// A NAME for a shape, not a claim about validity. Whether a matched string was ever issued,
/// and whether it still works, is knowable only at the issuer — AWS publishes
/// `AKIAIOSFODNN7EXAMPLE` in its own documentation, and a vault holding that page carries the
/// shape without carrying a key. The report says which shape matched and leaves the judgment
/// where the evidence is.
///
/// Reading the shape is all that is decidable. A 40-character base64 run is a key or a hash
/// and nothing in the text distinguishes them, so only forms whose ISSUER publishes a grammar
/// are read: a reserved prefix no other string may take, followed by a run of that issuer's
/// alphabet at the width it mints. An unprefixed key is out of scope BY CONSTRUCTION, and a
/// clean scan is therefore not a statement that a page holds no secret. Saying so is the
/// point — an inferred check fires on every commit id in the vault, and findings nobody
/// believes are findings nobody reads.
///
/// The set grows as issuers publish; it does not aspire to completeness, and no rule here is
/// ever inferred from a string's appearance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialForm {
    GitHubToken,
    GitHubFineGrainedToken,
    AnthropicApiKey,
    OpenAiProjectKey,
    OpenAiAdminKey,
    PyPiToken,
    SlackToken,
    SlackAppToken,
    AwsAccessKeyId,
    GoogleApiKey,
    StripeSecretKey,
    StripeRestrictedKey,
    StripeTestKey,
    StripeWebhookSecret,
    PrivateKeyBlock,
}

impl CredentialForm {
    /// One-line human description for `lore doctor` output.
    pub fn description(self) -> &'static str {
        match self {
            CredentialForm::GitHubToken => "GitHub access token",
            CredentialForm::GitHubFineGrainedToken => "GitHub fine-grained personal access token",
            CredentialForm::AnthropicApiKey => "Anthropic API key",
            CredentialForm::OpenAiProjectKey => "OpenAI project API key",
            CredentialForm::OpenAiAdminKey => "OpenAI organization admin key",
            CredentialForm::PyPiToken => "PyPI API token",
            CredentialForm::SlackToken => "Slack API token",
            CredentialForm::SlackAppToken => "Slack app-level token",
            CredentialForm::AwsAccessKeyId => "AWS access key id",
            CredentialForm::GoogleApiKey => "Google API key",
            CredentialForm::StripeSecretKey => "Stripe live secret key",
            CredentialForm::StripeRestrictedKey => "Stripe live restricted key",
            CredentialForm::StripeTestKey => "Stripe test-mode key",
            CredentialForm::StripeWebhookSecret => "Stripe webhook signing secret",
            CredentialForm::PrivateKeyBlock => "private key block",
        }
    }
}

/// One issuer-published credential grammar: a reserved prefix, the alphabet the issuer mints
/// the rest from, and how long that run is. The run is taken MAXIMALLY and its length must
/// land in `body_len`, which is what separates a credential from a mention of its prefix in
/// prose — `sk-ant-` in a sentence is followed by a space, not by twenty more base62
/// characters.
struct CredentialGrammar {
    form: CredentialForm,
    prefixes: &'static [&'static str],
    body: fn(char) -> bool,
    body_len: std::ops::RangeInclusive<usize>,
    /// The issuer mints this form with a NUMERIC first field, so the body must open with a
    /// digit. Slack's `xoxb-<team id>-…` is the shape; `xoxb-please-rotate-…` in a message
    /// about rotating tokens is not, and no length rule separates the two because English
    /// hyphenates.
    body_starts_digit: bool,
}

fn base62(c: char) -> bool {
    c.is_ascii_alphanumeric()
}

fn base62_extended(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn base62_underscore(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn upper_alnum(c: char) -> bool {
    c.is_ascii_uppercase() || c.is_ascii_digit()
}

/// The grammars, each as its issuer publishes it. Exact lengths where the issuer mints a
/// fixed width; a floor elsewhere, because a prefix a vendor reserved is a finding whatever
/// length follows it and pinning a width the vendor later changes would miss real keys.
fn credential_grammars() -> [CredentialGrammar; 13] {
    [
        CredentialGrammar {
            form: CredentialForm::GitHubToken,
            prefixes: &["ghp_", "gho_", "ghu_", "ghs_", "ghr_"],
            body: base62,
            body_len: 36..=255,
            body_starts_digit: false,
        },
        CredentialGrammar {
            form: CredentialForm::GitHubFineGrainedToken,
            prefixes: &["github_pat_"],
            body: base62_underscore,
            body_len: 40..=255,
            body_starts_digit: false,
        },
        // The key-type tag is part of the reserved marker, not part of the body: `sk-ant-`
        // alone is what a runbook writes when it names the prefix, and no length rule tells
        // that from a key because both continue in hyphenated text.
        CredentialGrammar {
            form: CredentialForm::AnthropicApiKey,
            prefixes: &["sk-ant-api03-", "sk-ant-admin01-", "sk-ant-sid01-"],
            body: base62_extended,
            body_len: 24..=255,
            body_starts_digit: false,
        },
        CredentialGrammar {
            form: CredentialForm::OpenAiProjectKey,
            prefixes: &["sk-proj-"],
            body: base62_extended,
            body_len: 80..=255,
            body_starts_digit: false,
        },
        CredentialGrammar {
            form: CredentialForm::OpenAiAdminKey,
            prefixes: &["sk-admin-"],
            body: base62_extended,
            body_len: 80..=255,
            body_starts_digit: false,
        },
        CredentialGrammar {
            form: CredentialForm::PyPiToken,
            prefixes: &["pypi-"],
            body: base62_extended,
            body_len: 100..=255,
            body_starts_digit: false,
        },
        // The rotating forms nest the bot/user prefix behind `xoxe.`, so they are listed in
        // their own right: the scan takes the LONGEST prefix at a position, which is what
        // keeps one token from being read as the shorter form it contains.
        CredentialGrammar {
            form: CredentialForm::SlackToken,
            prefixes: &[
                "xoxe.xoxb-",
                "xoxe.xoxp-",
                "xoxb-",
                "xoxp-",
                "xoxa-",
                "xoxc-",
                "xoxr-",
                "xoxs-",
                "xoxe-",
            ],
            body: base62_extended,
            body_len: 20..=255,
            body_starts_digit: true,
        },
        CredentialGrammar {
            form: CredentialForm::SlackAppToken,
            prefixes: &["xapp-"],
            body: base62_extended,
            body_len: 20..=255,
            body_starts_digit: true,
        },
        // Two deliberate narrowings, both trading recall for a finding a reader believes.
        // `ASIA` (temporary keys) is dropped because it is an English word and the rest of the
        // grammar is sixteen more uppercase characters, which all-caps product codes,
        // conference names and document ids reach. And the width is pinned at the twenty
        // characters AWS MINTS rather than the sixteen-to-128 its docs permit: widening it
        // makes every long all-caps run beginning `AKIA` a finding, and an IAM key of another
        // length is a miss this accepts. A form whose findings a reader learns to scroll past
        // protects nothing.
        CredentialGrammar {
            form: CredentialForm::AwsAccessKeyId,
            prefixes: &["AKIA"],
            body: upper_alnum,
            body_len: 16..=16,
            body_starts_digit: false,
        },
        CredentialGrammar {
            form: CredentialForm::GoogleApiKey,
            prefixes: &["AIza"],
            body: base62_extended,
            body_len: 35..=35,
            body_starts_digit: false,
        },
        CredentialGrammar {
            form: CredentialForm::StripeSecretKey,
            prefixes: &["sk_live_"],
            body: base62,
            body_len: 24..=255,
            body_starts_digit: false,
        },
        CredentialGrammar {
            form: CredentialForm::StripeRestrictedKey,
            prefixes: &["rk_live_"],
            body: base62,
            body_len: 24..=255,
            body_starts_digit: false,
        },
        CredentialGrammar {
            form: CredentialForm::StripeTestKey,
            prefixes: &["sk_test_", "rk_test_"],
            body: base62,
            body_len: 24..=255,
            body_starts_digit: false,
        },
    ]
}

/// A grammar flattened to one prefix, so the scan can order every prefix by length.
struct PrefixRule {
    prefix: &'static str,
    form: CredentialForm,
    body: fn(char) -> bool,
    body_len: std::ops::RangeInclusive<usize>,
    body_starts_digit: bool,
}

/// Every prefix any grammar declares, LONGEST first. Length order is what makes a nested
/// form read as itself: `xoxe.xoxb-…` contains `xoxb-`, and taking the shorter prefix would
/// both mislabel it and leave the outer form unmatched.
fn prefix_rules() -> Vec<PrefixRule> {
    let mut rules: Vec<PrefixRule> = credential_grammars()
        .into_iter()
        .flat_map(|grammar| {
            grammar.prefixes.iter().map(move |prefix| PrefixRule {
                prefix,
                form: grammar.form,
                body: grammar.body,
                body_len: grammar.body_len.clone(),
                body_starts_digit: grammar.body_starts_digit,
            })
        })
        .collect();
    rules.sort_by_key(|rule| std::cmp::Reverse(rule.prefix.len()));
    rules
}

/// The `-----BEGIN …-----` labels that name private key material, as their specifications
/// publish them (RFC 7468, OpenSSH, OpenPGP).
///
/// A label SET, not a suffix test. `PGP PRIVATE KEY BLOCK` does not end in `PRIVATE KEY`, so
/// a suffix would miss it, while `THIS IS NOT A PRIVATE KEY` does end that way and a suffix
/// would name it one. Neither is possible against a closed list.
const PRIVATE_KEY_LABELS: &[&str] = &[
    "PRIVATE KEY",
    "RSA PRIVATE KEY",
    "DSA PRIVATE KEY",
    "EC PRIVATE KEY",
    "ENCRYPTED PRIVATE KEY",
    "OPENSSH PRIVATE KEY",
    "PGP PRIVATE KEY BLOCK",
];

/// Every credential form named in `text`, each with its 1-based line number. A line carrying
/// two of them yields two entries: an operator acts on what the report lists, so collapsing
/// them would hide the second.
///
/// A page is REPORTED, never rewritten. The vault is the record of what a source said, and
/// editing a message to remove a key would leave a page asserting something nobody wrote
/// while the key stayed live at its issuer. Rotating it is the repair; what to do with the
/// text is a decision about the record.
pub fn scan_credentials(text: &str) -> Vec<(usize, CredentialForm)> {
    let rules = prefix_rules();
    let mut found = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line_no = i + 1;
        if is_private_key_header(line) {
            found.push((line_no, CredentialForm::PrivateKeyBlock));
        }
        found.extend(scan_line(line, &rules).map(|form| (line_no, form)));
    }
    found
}

/// Walk one line left to right, taking the longest prefix that matches at each position and
/// skipping past the credential it names, so a token is counted once and a nested form is not
/// also read as the shorter one inside it.
fn scan_line<'a>(
    line: &'a str,
    rules: &'a [PrefixRule],
) -> impl Iterator<Item = CredentialForm> + 'a {
    let mut cursor = 0;
    std::iter::from_fn(move || {
        while cursor < line.len() {
            // A prefix reached over alphanumerics is the tail of a longer word, not a prefix.
            // The test is alphanumeric rather than the grammar's own alphabet: several
            // alphabets admit `_` and `-`, and treating those as a left boundary would hide
            // a whole credential written after one (`leak_sk-proj-…`).
            let after_word = line[..cursor]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric());
            let rest = &line[cursor..];
            let matched = (!after_word).then(|| {
                rules.iter().find_map(|rule| {
                    let tail = rest.strip_prefix(rule.prefix)?;
                    // A form the issuer mints with a numeric first field opens with a digit.
                    // English hyphenates, so for a grammar whose alphabet admits `-` this is
                    // what tells a token from a sentence naming one — length cannot.
                    if rule.body_starts_digit && !tail.starts_with(|c: char| c.is_ascii_digit()) {
                        return None;
                    }
                    let body: usize = tail
                        .chars()
                        .take_while(|c| (rule.body)(*c))
                        .map(char::len_utf8)
                        .sum();
                    let run = tail[..body].chars().count();
                    rule.body_len
                        .contains(&run)
                        .then_some((rule.form, rule.prefix.len() + body))
                })
            });
            match matched.flatten() {
                Some((form, span)) => {
                    cursor += span;
                    return Some(form);
                }
                None => {
                    cursor += rest.chars().next().map_or(1, char::len_utf8);
                }
            }
        }
        None
    })
}

/// A PEM header naming private key material, tolerant of the markdown a quoted message
/// arrives in. A key pasted into an email or a Slack thread reaches the vault behind `> `,
/// and a header only recognised at column zero would miss exactly the pages this ingests.
fn is_private_key_header(line: &str) -> bool {
    let Some(label) = strip_markdown_quoting(line)
        .trim()
        .strip_prefix("-----BEGIN ")
        .and_then(|rest| rest.strip_suffix("-----"))
    else {
        return false;
    };
    PRIVATE_KEY_LABELS.contains(&label)
}

/// Drop the blockquote and list markers a quoted line carries, so the content is read as the
/// author wrote it. A marker is only a marker where markdown says one is: `>` anywhere in the
/// leading run, and a list bullet only when a space follows it — which is what keeps the
/// `-----` of a PEM header from being eaten as bullets.
fn strip_markdown_quoting(line: &str) -> &str {
    let mut rest = line.trim_start();
    loop {
        rest = match rest.strip_prefix('>') {
            Some(quoted) => quoted.trim_start(),
            None => match rest.split_at_checked(2) {
                Some(("- " | "* " | "+ ", tail)) => tail.trim_start(),
                _ => return rest,
            },
        };
    }
}

/// Tracks whether a line walker is currently inside a fenced code block. Per
/// CommonMark, an opening fence is 3+ consecutive `` ` `` or `~` characters (with
/// optional info string); a closing fence must use the same character, be at least
/// as long as the opener, and carry no info string. Headings (and other structure)
/// inside an open fence are quoted content, not document structure.
#[derive(Debug, Clone, Copy)]
pub enum FenceState {
    Closed,
    Open { marker: char, len: usize },
}

impl FenceState {
    pub fn new() -> Self {
        FenceState::Closed
    }

    pub fn is_closed(self) -> bool {
        matches!(self, FenceState::Closed)
    }

    /// Apply one line to the fence state. Returns true if the line was a fence
    /// marker (and thus must not be treated as document structure).
    pub fn apply(&mut self, line: &str) -> bool {
        let Some((marker, len, info)) = parse_fence(line) else {
            return false;
        };
        match *self {
            FenceState::Closed => {
                *self = FenceState::Open { marker, len };
                true
            }
            FenceState::Open {
                marker: open_marker,
                len: open_len,
            } => {
                // A closing fence must match the opener's character, be at least as
                // long, and carry no info string. Anything else is a marker-shaped
                // line inside the open block and the fence stays open.
                if marker == open_marker && len >= open_len && info.is_empty() {
                    *self = FenceState::Closed;
                }
                true
            }
        }
    }
}

impl Default for FenceState {
    fn default() -> Self {
        Self::new()
    }
}

/// Recognize a fence marker line. Returns `(marker char, marker length, info
/// string)`. CommonMark allows up to three spaces of leading indent before the
/// marker; the info string is everything after the marker run. A backtick fence's
/// info string MUST NOT contain a backtick (ambiguous with an inline code span),
/// so such a line is not a fence. Tilde fences have no such restriction.
pub fn parse_fence(line: &str) -> Option<(char, usize, &str)> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let marker = trimmed.chars().next()?;
    if marker != '`' && marker != '~' {
        return None;
    }
    let marker_len = trimmed.chars().take_while(|c| *c == marker).count();
    if marker_len < 3 {
        return None;
    }
    let info = trimmed[marker_len..].trim();
    if marker == '`' && info.contains('`') {
        return None;
    }
    Some((marker, marker_len, info))
}

/// The ATX heading level of a line (1–6), or `None` if it isn't a heading.
/// Per CommonMark: up to 3 leading spaces, 1–6 `#`, then a space or end of line.
/// `####### ` (7 hashes) is not a heading. Returns the byte offset where the `#`
/// run starts so callers can rewrite in place.
fn atx_heading(line: &str) -> Option<(usize, usize)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let level = rest.chars().take_while(|c| *c == '#').count();
    if level == 0 || level > 6 {
        return None;
    }
    // The hash run must be followed by a space or the end of the line.
    match rest[level..].chars().next() {
        None | Some(' ') => Some((indent, level)),
        _ => None,
    }
}

/// Demote every ATX heading in `text` so the shallowest sits at `floor` (clamped to
/// H6), preserving relative structure. Headings inside fenced code blocks are left
/// untouched. Used to sanitise embedded source content (Jira ADF, manual `.md`,
/// RSS→Markdown) before it is rendered under a page's `##`-structured sections, so a
/// source body's `## Heading` never collides with the page/section/event heading
/// hierarchy that `lk-vault::section` relies on. A no-op when there are no headings
/// or the shallowest is already at/below `floor`.
pub fn demote_headings(text: &str, floor: usize) -> String {
    // Pass 1: find the shallowest non-fenced heading level.
    let mut fence = FenceState::new();
    let mut min_level: Option<usize> = None;
    for line in text.split_inclusive('\n') {
        let stripped = line.strip_suffix('\n').unwrap_or(line);
        // Skip a line that is a fence marker (`apply` true) OR sits inside an open
        // fence (state still Open after applying it) — only un-fenced lines are
        // document structure.
        let is_marker = fence.apply(stripped);
        if is_marker || !fence.is_closed() {
            continue;
        }
        if let Some((_, level)) = atx_heading(stripped) {
            min_level = Some(min_level.map_or(level, |m| m.min(level)));
        }
    }

    let Some(min_level) = min_level else {
        return text.to_string();
    };
    if min_level >= floor {
        return text.to_string();
    }
    let shift = floor - min_level;

    // Pass 2: rewrite each non-fenced heading line, raising its level by `shift`
    // (clamped to 6) by inserting the extra `#`s at the start of the hash run.
    let mut fence = FenceState::new();
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let nl = line.ends_with('\n');
        let stripped = line.strip_suffix('\n').unwrap_or(line);
        let is_marker = fence.apply(stripped);
        if is_marker || !fence.is_closed() {
            out.push_str(line);
            continue;
        }
        match atx_heading(stripped) {
            Some((indent, level)) => {
                let new_level = (level + shift).min(6);
                out.push_str(&stripped[..indent]);
                for _ in 0..new_level {
                    out.push('#');
                }
                out.push_str(&stripped[indent + level..]);
                if nl {
                    out.push('\n');
                }
            }
            None => out.push_str(line),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{CredentialForm, scan_credentials};

    fn forms(line: &str) -> Vec<CredentialForm> {
        scan_credentials(line).into_iter().map(|(_, f)| f).collect()
    }

    /// A credential-shaped string, assembled from parts.
    ///
    /// Every fixture here is built rather than written, so this file carries no literal a
    /// secret scanner reads as a live key — GitHub's push protection rejected the literal
    /// form, and it was right to: a repository holding one teaches every clone's scanner that
    /// this shape is noise. The bytes reaching `scan_credentials` are identical, so the
    /// grammar under test is unchanged.
    fn shaped(parts: &[&str]) -> String {
        parts.concat()
    }

    /// Bodies long enough to clear the floors the issuers' minted widths set.
    const LONG_BODY: &str =
        "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghij";
    const PYPI_BODY: &str = "AgEIcHlwaS5vcmcCJGFiY2RlZmdoaWprbG1ub3BxcnN0dXZ3eHl6MDEyMzQ1Njc4OWFiY2RlZmdoaWprbG1ub3BxcnN0dXZ3eHl6MDEy";

    /// The forms an issuer publishes a grammar for, each written as that issuer mints it.
    #[test]
    fn every_published_grammar_is_named() {
        let cases: Vec<(String, CredentialForm)> = vec![
            (
                shaped(&["ghp", "_0123456789abcdefghij", "klmnopqrstuvwxyz"]),
                CredentialForm::GitHubToken,
            ),
            (
                shaped(&[
                    "github",
                    "_pat_11ABCDEFG0abcdefghij_0123456789",
                    "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOP",
                ]),
                CredentialForm::GitHubFineGrainedToken,
            ),
            (
                shaped(&["sk-ant", "-api03-abcdefghij", "klmnopqrstuvwx"]),
                CredentialForm::AnthropicApiKey,
            ),
            (
                shaped(&["sk-ant", "-admin01-abcdefghij", "klmnopqrstuvwx"]),
                CredentialForm::AnthropicApiKey,
            ),
            (
                shaped(&["sk-proj", "-", LONG_BODY]),
                CredentialForm::OpenAiProjectKey,
            ),
            (
                shaped(&["sk-admin", "-", LONG_BODY]),
                CredentialForm::OpenAiAdminKey,
            ),
            (shaped(&["pypi", "-", PYPI_BODY]), CredentialForm::PyPiToken),
            (
                shaped(&[
                    "xoxb",
                    "-123456789012-1234567890123-",
                    "abcdefghijklmnopqrstuvwx",
                ]),
                CredentialForm::SlackToken,
            ),
            (
                shaped(&[
                    "xapp",
                    "-1-A01234567-1234567890123-",
                    "abcdefghijklmnopqrstuvwx",
                ]),
                CredentialForm::SlackAppToken,
            ),
            (
                shaped(&["AKIA", "IOSFODNN7", "EXAMPLE"]),
                CredentialForm::AwsAccessKeyId,
            ),
            (
                shaped(&["AIza", "SyA0123456789abcdefghij", "klmnopqrstuv"]),
                CredentialForm::GoogleApiKey,
            ),
            (
                shaped(&["sk", "_live_0123456789", "abcdefghijklmnop"]),
                CredentialForm::StripeSecretKey,
            ),
            (
                shaped(&["rk", "_live_0123456789", "abcdefghijklmnop"]),
                CredentialForm::StripeRestrictedKey,
            ),
            (
                shaped(&["sk", "_test_0123456789", "abcdefghijklmnop"]),
                CredentialForm::StripeTestKey,
            ),
            (
                "-----BEGIN OPENSSH PRIVATE KEY-----".to_owned(),
                CredentialForm::PrivateKeyBlock,
            ),
            (
                "-----BEGIN RSA PRIVATE KEY-----".to_owned(),
                CredentialForm::PrivateKeyBlock,
            ),
            (
                "-----BEGIN PGP PRIVATE KEY BLOCK-----".to_owned(),
                CredentialForm::PrivateKeyBlock,
            ),
        ];
        for (line, form) in &cases {
            assert_eq!(forms(line), vec![*form], "{line}");
        }
    }

    /// Two keys on one line are two findings. An operator acts on what the report lists, so
    /// collapsing them hides the second — and a rotation that misses one leaves it live.
    #[test]
    fn every_credential_on_a_line_is_reported() {
        let token = shaped(&["ghp", "_0123456789abcdefghij", "klmnopqrstuvwxyz"]);
        let other = shaped(&["ghp", "_zyxwvutsrqponmlkji", "hgfedcba9876543210"]);
        let line = format!("old={token} new={other}");
        assert_eq!(
            scan_credentials(&line),
            vec![
                (1, CredentialForm::GitHubToken),
                (1, CredentialForm::GitHubToken)
            ]
        );
    }

    /// A rotating Slack token nests the bot prefix behind `xoxe.`. Taking the longest prefix
    /// at a position is what reads it as one token rather than mislabelling it and leaving
    /// the outer form unmatched.
    #[test]
    fn a_nested_prefix_is_read_as_the_form_that_contains_it() {
        let line = shaped(&["xoxe.xoxb", "-1-abcdefghij", "klmnopqrstuvwxyz012345"]);
        assert_eq!(forms(&line), vec![CredentialForm::SlackToken]);
    }

    /// The left boundary is alphanumeric, not the grammar's own alphabet. Several alphabets
    /// admit `_` and `-`, so testing against them would hide a whole credential written after
    /// one — while a prefix reached over letters is still the tail of a word.
    #[test]
    fn a_separator_before_a_prefix_does_not_hide_the_credential() {
        assert_eq!(
            forms(
                "leak_sk-proj-abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghij"
            ),
            vec![CredentialForm::OpenAiProjectKey]
        );
        assert!(
            forms(&shaped(&[
                "xxghp",
                "_0123456789abcdefghij",
                "klmnopqrstuvwxyz"
            ]))
            .is_empty(),
            "the run continues to the left, so this is one string"
        );
    }

    /// A key pasted into an email or a Slack thread reaches the vault behind `> `. A header
    /// only recognised at column zero would miss exactly the pages this ingests.
    #[test]
    fn a_quoted_private_key_header_is_still_a_header() {
        for line in [
            "> -----BEGIN RSA PRIVATE KEY-----",
            ">> -----BEGIN OPENSSH PRIVATE KEY-----",
            "- -----BEGIN PRIVATE KEY-----",
            "  > - -----BEGIN EC PRIVATE KEY-----",
        ] {
            assert_eq!(forms(line), vec![CredentialForm::PrivateKeyBlock], "{line}");
        }
    }

    /// The labels are a closed set, so a line that merely ends in those words is not a key.
    /// A suffix test would name this one and miss `PGP PRIVATE KEY BLOCK`, which is the pair
    /// of errors the set removes at once.
    #[test]
    fn only_a_published_label_names_a_private_key() {
        for line in [
            "-----BEGIN THIS IS NOT A PRIVATE KEY-----",
            "-----BEGIN CERTIFICATE-----",
            "-----BEGIN PUBLIC KEY-----",
            "the file starts -----BEGIN RSA PRIVATE KEY----- as usual",
        ] {
            assert!(forms(line).is_empty(), "{line}");
        }
    }

    /// A page explaining which prefix to look for is the ordinary case in a vault about
    /// tooling, and a check that fired on it is one whose findings stop being read.
    #[test]
    fn a_prefix_written_in_prose_is_not_a_credential() {
        for line in [
            "Anthropic keys start with sk-ant-, GitHub's with ghp_.",
            "The AKIA prefix identifies a long-lived AWS key.",
            "Rotate anything matching xoxb- immediately.",
            "See github_pat_ for the fine-grained form.",
            "A row of ghp_ in a table header.",
        ] {
            assert!(forms(line).is_empty(), "{line}");
        }
    }

    /// English hyphenates, so for any grammar whose alphabet admits `-` the body run reaches
    /// whatever floor a length gate sets — a runbook sentence naming a token IS the alphabet.
    /// Length cannot separate these; the issuer's own shape can, and this is a pipeline that
    /// ingests Slack messages about rotating tokens.
    #[test]
    fn hyphenated_prose_naming_a_token_is_not_a_token() {
        for line in [
            "ref: sk-ant-this-is-a-doc-about-key-prefixes-not-a-key",
            "Slack thread mentions xoxb-please-rotate-any-token-you-see",
            "the xapp-level-token-rotation-runbook-lives-here",
            "sk-proj-migration-notes-for-the-key-rotation-window-next-quarter",
            "see pypi-publishing-and-token-scoping-guidelines-for-maintainers",
        ] {
            assert!(forms(line).is_empty(), "{line}");
        }
    }

    /// `ASIA` is an English word and the rest of the grammar is sixteen more uppercase
    /// characters, which all-caps product codes and document ids reach. AWS temporary keys
    /// are out of scope rather than have every such string reported.
    #[test]
    fn an_all_caps_word_is_not_an_aws_key() {
        assert!(forms("see chart in ASIAPACIFICSUM2026XY for details").is_empty());
        assert!(forms("ASIAIOSFODNN7EXAMPLE").is_empty());
    }

    /// A run the issuer would never mint is not that issuer's credential. AWS access key ids
    /// are exactly twenty characters, so a longer word beginning `AKIA` is a word.
    #[test]
    fn a_run_outside_the_published_width_is_not_a_credential() {
        assert!(forms("AKIAIOSFODNN7EXAMPLETOOLONG").is_empty());
        assert!(forms("AKIASHORT").is_empty());
        assert!(forms("AIzaTooShortForAGoogleKey").is_empty());
    }

    /// Ordinary vault text — prose, links, hashes, ids — names nothing. A hash is the shape
    /// an inferred check fires on, and the reason this one reads only reserved namespaces.
    #[test]
    fn ordinary_vault_text_names_nothing() {
        for line in [
            "The daily page links [RAG](../../wiki/concepts/rag.md).",
            "commit 0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c",
            "cache_hash: af51662a480e70911c443fbae7ca8df5",
            "Message-ID: <CAF8s0Xk9mVQ0abcdefghijklmnopqrstuvwxyz@mail.gmail.com>",
            "https://example.com/AKIAIOSFODNN7EXAMPLEISNOTHERE",
            "8비트 색상 정규화 — 한글 본문도 바이트 경계를 넘지 않는다",
            "",
        ] {
            assert!(forms(line).is_empty(), "{line}");
        }
    }

    #[test]
    fn a_credential_is_reported_with_its_line() {
        let token = shaped(&["ghp", "_0123456789abcdefghij", "klmnopqrstuvwxyz"]);
        let page = format!("# Notes\n\nsetup:\n\n    export GITHUB_TOKEN={token}\n");
        assert_eq!(
            scan_credentials(&page),
            vec![(5, CredentialForm::GitHubToken)]
        );
    }

    use super::*;

    #[test]
    fn demotes_h2_to_floor_preserving_relative_structure() {
        // Shallowest is H2 → shift +2 so it lands at H4; H3 follows to H5.
        let input = "## Plan\n\nbody\n\n### Detail\n\nmore\n";
        let out = demote_headings(input, 4);
        assert_eq!(out, "#### Plan\n\nbody\n\n##### Detail\n\nmore\n");
    }

    #[test]
    fn h1_demoted_to_floor() {
        assert_eq!(demote_headings("# Title\ntext\n", 4), "#### Title\ntext\n");
    }

    #[test]
    fn noop_when_already_at_or_below_floor() {
        let input = "#### Already deep\n\n##### Deeper\n";
        assert_eq!(demote_headings(input, 4), input);
    }

    #[test]
    fn noop_when_no_headings() {
        let input = "just a paragraph\n\nand another\n";
        assert_eq!(demote_headings(input, 4), input);
    }

    #[test]
    fn clamps_at_h6() {
        // H2 with shift +2 → H4; an H5 in the same body → H7 clamped to H6.
        let input = "## A\n\n##### Deep\n";
        let out = demote_headings(input, 4);
        assert_eq!(out, "#### A\n\n###### Deep\n");
    }

    #[test]
    fn leaves_headings_inside_fenced_code_untouched() {
        // The `## Inside` is fenced code, not a heading — it must NOT count toward
        // min-level NOR be rewritten. The real `## Real` heading drives the shift.
        let input = "## Real\n\n```\n## Inside\n```\n";
        let out = demote_headings(input, 4);
        assert_eq!(out, "#### Real\n\n```\n## Inside\n```\n");
    }

    #[test]
    fn ignores_non_heading_hash_lines() {
        // `#nospace` and a 7-hash run are not ATX headings.
        let input = "#nospace\n\n####### sevenhashes\n";
        assert_eq!(demote_headings(input, 4), input);
    }

    #[test]
    fn preserves_trailing_text_after_hashes() {
        assert_eq!(
            demote_headings("## Heading text here\n", 4),
            "#### Heading text here\n"
        );
    }

    #[test]
    fn scan_flags_inline_data_uri_image_with_line_number() {
        let page = "# Page\n\nintro\n\n![logo](data:image/png;base64,AAAA)\n\nmore\n";
        assert_eq!(scan_defects(page), vec![(5, TextDefect::InlineDataUri)]);
    }

    #[test]
    fn scan_flags_data_uri_autolink() {
        assert_eq!(
            scan_defects("see <data:text/plain,hi>\n"),
            vec![(1, TextDefect::InlineDataUri)]
        );
    }

    #[test]
    fn scan_clean_page_has_no_defects() {
        // A fetchable http image and the bare word "data" are both fine — only an
        // inlined data: URI target is a defect.
        let page = "# Page\n\n![chart](https://x.io/a.png)\n\nthe data shows growth\n";
        assert!(scan_defects(page).is_empty());
    }

    #[test]
    fn scan_is_case_insensitive_on_the_scheme() {
        // The converter strips `DATA:` (case-insensitive), so the checker must catch it
        // too — otherwise a page the pipeline cleaned could still read as defective.
        assert_eq!(
            scan_defects("![x](DATA:image/png;base64,AA)\n"),
            vec![(1, TextDefect::InlineDataUri)]
        );
    }

    #[test]
    fn scan_reports_correct_line_across_crlf_and_bom() {
        // `str::lines` strips a trailing `\r`, and a leading BOM rides on line 1, so the
        // reported line number is stable regardless of encoding quirks.
        let page = "\u{feff}# Page\r\n\r\nintro\r\n![x](data:text/plain,a)\r\n";
        assert_eq!(scan_defects(page), vec![(4, TextDefect::InlineDataUri)]);
    }
}
