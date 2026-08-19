//! Where an observation came from, as an IDENTITY rather than as text.
//!
//! A task written down from a Jira issue, a Slack thread or a mail carries that origin's URL in
//! its visible title, which is what makes the link survive onto the archive page. But a title is
//! expected to change — refining what a thing actually is, is most of what keeping a list is —
//! so a join that read the title would stop matching the moment someone rewrote it.
//!
//! This is the other half, the same split `TaskId` makes between the address a task is written
//! at and the name it answers to. It is what stops one open issue being proposed every morning
//! after it was accepted, declined or finished, and it is a HASH because a board stamp's values
//! are `[0-9A-Za-z-]+` and a URL is not.

/// The identity of `url`, as `blake3(url)[..16]`.
///
/// Byte-exact on the URL: two spellings of one page — a trailing slash, a tracking parameter —
/// are two origins here. Folding them would be a judgment about what a URL means, which differs
/// per provider and would take a rule per source; a duplicate proposal costs one `lore task
/// drop`, while a wrong fold silently suppresses a real one.
pub fn identity(url: &str) -> String {
    blake3::hash(url.as_bytes()).to_hex()[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_url_has_one_identity() {
        let url = "https://acme.atlassian.net/browse/PLAT-411";
        assert_eq!(identity(url), identity(url));
        assert_eq!(identity(url).len(), 16);
        assert!(identity(url).bytes().all(|b| b.is_ascii_alphanumeric()));
    }

    #[test]
    fn two_urls_have_two() {
        assert_ne!(
            identity("https://acme.atlassian.net/browse/PLAT-411"),
            identity("https://acme.atlassian.net/browse/PLAT-412")
        );
    }
}
