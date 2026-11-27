use lk_core::semantic::{SemanticAuthority, SemanticResource};
use serde_json::{Value, json};

use super::Work;

/// How a finalizer rereads this Work's planned resources: any of them through `--resource` and
/// its index in the inspected roster, by default the retained task source when the Work has one.
#[cfg(unix)]
pub(super) fn descriptor(work: &Work) -> miette::Result<Value> {
    let default = retained_source(&work.plan.resources, |guard| guard.authority)?;
    Ok(json!({"work_id":work.plan.id,"default_resource":default,
        "command":["lore","work","source",work.plan.id],
        "offset_unit":"unicode-scalars","max_page_characters":lk_core::semantic::INSPECTION_CHARACTERS.get(),
        "max_page_bytes":work.max_input_bytes}))
}

/// The index of the Work's single retained task source, if it has one.
fn retained_source<T>(
    resources: &[T],
    authority: impl Fn(&T) -> SemanticAuthority,
) -> miette::Result<Option<usize>> {
    let mut sources = resources
        .iter()
        .enumerate()
        .filter(|(_, resource)| authority(resource) == SemanticAuthority::RetainedInput);
    let source = sources.next().map(|(index, _)| index);
    if sources.next().is_some() {
        return Err(miette::miette!(
            "this Work has ambiguous retained task sources"
        ));
    }
    Ok(source)
}

/// The planned resource a read names: `resource` by its index in the Work's plan, by default
/// the Work's retained task source.
pub(super) fn resource_index(work: &Work, resource: Option<usize>) -> miette::Result<usize> {
    let planned = &work.plan.resources;
    match resource {
        Some(index) if index < planned.len() => Ok(index),
        Some(index) => Err(miette::miette!(
            "resource {index} is not one of this Work's {} planned resources",
            planned.len()
        )),
        None => retained_source(planned, |guard| guard.authority)?.ok_or_else(|| {
            miette::miette!("this Work has no retained task source; choose a planned resource")
        }),
    }
}

/// A read page of the planned resource at `index`, whose text is `source`: a current view of
/// it, never an observation or completion receipt.
pub(super) fn read(
    work: &Work,
    index: usize,
    source: &SemanticResource,
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

    fn read(
        work: &Work,
        resources: &[SemanticResource],
        resource: Option<usize>,
        offset: usize,
        limit: usize,
        find: Option<&str>,
    ) -> miette::Result<Value> {
        let index = resource_index(work, resource)?;
        super::read(work, index, &resources[index], offset, limit, find)
    }

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
            let page = read(&work, &resources, None, offset, 80000, None).unwrap();
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
        let eof = read(&work, &resources, None, text.chars().count(), 4, None).unwrap();
        assert_eq!(eof["text"], "");
        assert!(eof["next_offset"].is_null());
    }

    #[test]
    fn any_planned_resource_reads_by_index_and_only_the_retained_source_by_default() {
        let resources = vec![
            SemanticResource {
                address: "wiki/documents/cited.md".into(),
                revision: lk_core::concept::file_revision("Cited evidence."),
                authority: SemanticAuthority::Page,
                text: "Cited evidence.".into(),
            },
            SemanticResource {
                address: "wiki/concepts/owner.md".into(),
                revision: lk_core::concept::file_revision("Owner body."),
                authority: SemanticAuthority::Context,
                text: "Owner body.".into(),
            },
        ];
        let work = Work::build(
            &resources,
            "scope",
            json!({"purpose":{"operation":"queue","task_kind":"synthesize-concept"}}),
            WorkLimits {
                max_input_bytes: 8192,
                max_result_bytes: 1024,
            },
        )
        .unwrap();
        let page = read(&work, &resources, Some(1), 0, 100, None).unwrap();
        assert_eq!(page["resource"], 1);
        assert_eq!(page["text"], "Owner body.");
        assert!(read(&work, &resources, Some(2), 0, 100, None).is_err());
        assert!(read(&work, &resources, None, 0, 100, None).is_err());
    }

    #[test]
    fn literal_seek_keeps_duplicate_matches_and_unicode_offsets_distinct() {
        let text = "한🙂Title\nfirst\nTitle\nsecond";
        let (work, resources) = fixture(text, 8192);
        let first = read(&work, &resources, None, 0, 12, Some("Title")).unwrap();
        assert_eq!(first["range"]["start"], 2);
        assert!(first["text"].as_str().unwrap().starts_with("Title\nfirst"));
        let second = read(&work, &resources, None, 3, 12, Some("Title")).unwrap();
        assert_eq!(second["range"]["start"], 14);
        assert_eq!(second["text"], "Title\nsecond");
        let missing = read(&work, &resources, None, 0, 12, Some("title")).unwrap();
        assert_eq!(missing["status"], "not-found");
        assert!(missing.get("text").is_none());
        for invalid in [0, 80001] {
            assert!(read(&work, &resources, None, 0, invalid, None).is_err());
        }
        assert!(read(&work, &resources, None, usize::MAX, 1, None).is_err());
        assert!(read(&work, &resources, None, 0, 1, Some(" \n")).is_err());
        let error = read(&work, &resources, None, 0, 1, Some(&"x".repeat(10000))).unwrap_err();
        assert!(
            error
                .downcast_ref::<super::super::state::InputCapacity>()
                .is_some()
        );
    }
}
