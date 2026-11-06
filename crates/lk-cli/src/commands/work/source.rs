use lk_core::semantic::{SemanticAuthority, SemanticResource};
use serde_json::{Value, json};

use super::Work;

#[cfg(unix)]
pub(super) fn descriptor(work: &Work) -> miette::Result<Option<Value>> {
    let mut sources = work
        .plan
        .resources
        .iter()
        .enumerate()
        .filter(|(_, guard)| guard.authority == SemanticAuthority::RetainedInput);
    let Some((resource, _)) = sources.next() else {
        return Ok(None);
    };
    if sources.next().is_some() {
        return Err(miette::miette!(
            "this Work has ambiguous retained task sources"
        ));
    }
    Ok(Some(json!({"work_id":work.plan.id,"resource":resource,
        "command":["lore","work","source",work.plan.id],
        "offset_unit":"unicode-scalars","max_page_characters":lk_core::semantic::INSPECTION_CHARACTERS.get(),
        "max_page_bytes":work.max_input_bytes})))
}

/// A read page is a current source view, never an observation or completion receipt.
pub(super) fn read(
    work: &Work,
    resources: &[SemanticResource],
    offset: usize,
    limit: usize,
    find: Option<&str>,
) -> miette::Result<Value> {
    if limit == 0 || limit > lk_core::semantic::INSPECTION_CHARACTERS.get() {
        return Err(miette::miette!(
            "source limit must be between 1 and {} Unicode scalars",
            lk_core::semantic::INSPECTION_CHARACTERS
        ));
    }
    if find.is_some_and(|query| query.trim().is_empty()) {
        return Err(miette::miette!(
            "source find requires nonblank literal text"
        ));
    }
    let mut sources = resources
        .iter()
        .enumerate()
        .filter(|(_, resource)| resource.authority == SemanticAuthority::RetainedInput);
    let (index, source) = sources
        .next()
        .ok_or_else(|| miette::miette!("this Work has no retained task source"))?;
    if sources.next().is_some() {
        return Err(miette::miette!(
            "this Work has ambiguous retained task sources"
        ));
    }
    let guard = &work.plan.resources[index];
    if offset > guard.characters {
        return Err(miette::miette!(
            "source offset exceeds its Unicode scalar length"
        ));
    }
    let byte_offset = source
        .text
        .char_indices()
        .nth(offset)
        .map_or(source.text.len(), |(byte, _)| byte);
    let suffix = &source.text[byte_offset..];
    let (start, suffix) = match find {
        None => (offset, suffix),
        Some(query) => match suffix.find(query) {
            Some(byte) => (offset + suffix[..byte].chars().count(), &suffix[byte..]),
            None => {
                let value = json!({"schema":"work-source-v1","status":"not-found",
                    "work_id":work.plan.id,"context_revision":work.plan.subject_revision,
                    "resource":index,"guard":guard,"searched_from":offset,"find":query});
                ensure_fits(work, &value)?;
                return Ok(value);
            }
        },
    };
    let text = suffix.chars().take(limit).collect::<String>();
    let boundaries = text
        .char_indices()
        .map(|(byte, _)| byte)
        .chain(std::iter::once(text.len()))
        .collect::<Vec<_>>();
    let page = |characters: usize| {
        let end = start + characters;
        json!({"schema":"work-source-v1","status":"page",
            "work_id":work.plan.id,"context_revision":work.plan.subject_revision,
            "resource":index,"guard":guard,
            "range":{"resource":index,"start":start,"end":end},
            "text":&text[..boundaries[characters]],
            "next_offset":(end < guard.characters).then_some(end)})
    };
    let requested = boundaries.len() - 1;
    let value = page(requested);
    if bytes(&value) <= work.max_input_bytes {
        return Ok(value);
    }
    let mut low = 0;
    let mut high = requested;
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        if bytes(&page(middle)) <= work.max_input_bytes {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    if low == 0 {
        ensure_fits(work, &page(usize::from(requested > 0)))?;
    }
    Ok(page(low))
}

fn bytes(value: &Value) -> usize {
    serde_json::to_vec(value)
        .expect("source page serializes")
        .len()
}

fn ensure_fits(work: &Work, value: &Value) -> miette::Result<()> {
    let actual = bytes(value);
    if actual > work.max_input_bytes {
        return Err(super::state::InputCapacity {
            actual_bytes: actual,
            max_bytes: work.max_input_bytes,
        }
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::work::state::WorkLimits;

    fn fixture(text: &str, max_input_bytes: usize) -> (Work, Vec<SemanticResource>) {
        let resources = vec![SemanticResource {
            address: "retained-input://wiki/documents/source.md/summary".into(),
            revision: lk_core::concept::file_revision(text),
            authority: SemanticAuthority::RetainedInput,
            text: text.into(),
        }];
        let work = Work::build(
            &resources,
            "scope",
            json!({"purpose":{"operation":"queue","task_kind":"summarize"}}),
            WorkLimits {
                max_input_bytes,
                max_result_bytes: 1024,
            },
        )
        .unwrap();
        (work, resources)
    }

    #[test]
    fn scalar_pages_roundtrip_under_complete_json_byte_limits() {
        let text = "한🙂e\u{301}\n\"\\".repeat(1000);
        let (work, resources) = fixture(&text, 2048);
        let mut offset = 0;
        let mut reconstructed = String::new();
        loop {
            let page = read(&work, &resources, offset, 80000, None).unwrap();
            assert!(bytes(&page) <= work.max_input_bytes);
            assert_eq!(page["status"], "page");
            assert_eq!(page["range"]["start"], offset);
            let chunk = page["text"].as_str().unwrap();
            let end = offset + chunk.chars().count();
            assert_eq!(page["range"]["end"], end);
            assert!(end > offset);
            reconstructed.push_str(chunk);
            match page["next_offset"].as_u64() {
                None => break,
                Some(next) => {
                    assert_eq!(next as usize, end);
                    offset = next as usize;
                }
            }
        }
        assert_eq!(reconstructed, text);
        let eof = read(&work, &resources, text.chars().count(), 4, None).unwrap();
        assert_eq!(eof["text"], "");
        assert!(eof["next_offset"].is_null());
    }

    #[test]
    fn literal_seek_keeps_duplicate_matches_and_unicode_offsets_distinct() {
        let text = "한🙂Title\nfirst\nTitle\nsecond";
        let (work, resources) = fixture(text, 8192);
        let first = read(&work, &resources, 0, 12, Some("Title")).unwrap();
        assert_eq!(first["range"]["start"], 2);
        assert!(first["text"].as_str().unwrap().starts_with("Title\nfirst"));
        let second = read(&work, &resources, 3, 12, Some("Title")).unwrap();
        assert_eq!(second["range"]["start"], 14);
        assert_eq!(second["text"], "Title\nsecond");
        let missing = read(&work, &resources, 0, 12, Some("title")).unwrap();
        assert_eq!(missing["status"], "not-found");
        assert!(missing.get("text").is_none());
        for invalid in [0, 80001] {
            assert!(read(&work, &resources, 0, invalid, None).is_err());
        }
        assert!(read(&work, &resources, usize::MAX, 1, None).is_err());
        assert!(read(&work, &resources, 0, 1, Some(" \n")).is_err());
        let error = read(&work, &resources, 0, 1, Some(&"x".repeat(10000))).unwrap_err();
        assert!(
            error
                .downcast_ref::<super::super::state::InputCapacity>()
                .is_some()
        );
    }
}
