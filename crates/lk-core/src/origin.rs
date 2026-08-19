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

/// The width every identity is minted at.
const WIDTH: usize = 16;

/// Whether `value` is an identity this could have minted.
///
/// Read back out of a board stamp, a `src:` is only what the stamp says it is — the field's
/// grammar accepts any run of `[0-9A-Za-z-]`, so a value truncated by a crash mid-write or cut
/// by a sync client's conflict resolution is still a legal FIELD while naming no observation at
/// all. Asked here rather than at each reader, for the same reason `TaskId` parses rather than
/// being length-checked wherever it is read.
pub fn is_identity(value: &str) -> bool {
    value.len() == WIDTH && value.bytes().all(|byte| byte.is_ascii_hexdigit())
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

    /// A `src:` read back out of a stamp is only what the field's grammar allows, which is any
    /// run of `[0-9A-Za-z-]` — so a value a crash or a conflict cut short is a legal field
    /// naming no observation, and a set of "every origin the page answers to" holding one says
    /// something it does not hold.
    #[test]
    fn only_what_this_could_have_minted_is_an_identity() {
        assert!(is_identity(&identity(
            "https://acme.example.com/browse/PLAT-411"
        )));
        assert!(!is_identity("7fa5"), "truncated");
        assert!(!is_identity("7fa514724c2f102e0"), "too long");
        assert!(!is_identity("7fa514724c2f102z"), "not hex");
        assert!(!is_identity(""), "empty");
    }

    #[test]
    fn two_urls_have_two() {
        assert_ne!(
            identity("https://acme.atlassian.net/browse/PLAT-411"),
            identity("https://acme.atlassian.net/browse/PLAT-412")
        );
    }
}
