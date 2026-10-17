//! One YAML input policy for configuration, page frontmatter and extraction manifests.
//!
//! Keep YAML 1.2 booleans and ordinary `<<` keys rather than silently enabling
//! YAML 1.1 coercion or merge expansion during a parser upgrade. Reject ambiguous
//! duplicate keys. Parser budgets remain enabled and includes are not compiled in.

use serde::Deserialize;

pub fn from_str<'de, T: Deserialize<'de>>(input: &'de str) -> Result<T, serde_saphyr::Error> {
    let options = serde_saphyr::options! {
        strict_booleans: true,
        merge_keys: serde_saphyr::MergeKeyPolicy::AsOrdinary,
        duplicate_keys: serde_saphyr::DuplicateKeyPolicy::Error,
        // Diagnostics must not echo an entire config/frontmatter containing secrets.
        with_snippet: false,
    };
    serde_saphyr::from_str_with_options(input, options)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn preserves_yaml12_scalars_aliases_and_literal_merge_keys() {
        let parsed: Value = from_str(
            "name: on\nflag: true\ncode: '0052'\nbase: &base {x: 1}\ncopy: *base\nmap: {<<: *base, y: 2}\ntext: |\n  first\n  second\n",
        ).unwrap();
        assert_eq!(
            parsed,
            json!({"name":"on","flag":true,"code":"0052","base":{"x":1},"copy":{"x":1},"map":{"<<":{"x":1},"y":2},"text":"first\nsecond\n"})
        );
    }

    #[test]
    fn refuses_ambiguous_or_invalid_documents() {
        for input in ["x: 1\nx: 2\n", "x: [unfinished", "x: 1\n---\nx: 2\n"] {
            assert!(from_str::<Value>(input).is_err(), "accepted {input:?}");
        }
    }

    #[test]
    fn boolean_fields_do_not_accept_yaml11_shorthand() {
        #[derive(Deserialize)]
        struct Flag {
            #[allow(dead_code)]
            enabled: bool,
        }
        assert!(from_str::<Flag>("enabled: on").is_err());
        assert!(from_str::<Flag>("enabled: true").is_ok());
    }
}
