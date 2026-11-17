use std::collections::{BTreeMap, BTreeSet};

use crate::commands::work::{SCREENING_INSTRUCTION, ScreeningDecision, ScreeningRejection};
use serde::Deserialize;

use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Body {
    decisions: Vec<Decision>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Decision {
    candidate_ref: String,
    decision: ScreeningDecision,
    reason: String,
}

/// Short request-local references in candidate order, each naming its exact candidate key.
fn candidates(request: &Value) -> miette::Result<BTreeMap<String, String>> {
    let rows = request["candidates"]
        .as_array()
        .ok_or_else(|| miette::miette!("screening request has no candidate array"))?;
    let mut keys = BTreeSet::new();
    rows.iter()
        .enumerate()
        .map(|(index, row)| {
            let key = row["candidate_key"]
                .as_str()
                .filter(|key| !key.is_empty())
                .ok_or_else(|| miette::miette!("screening candidate has no key"))?;
            if row.get("candidate_ref").is_some() || !keys.insert(key) {
                return Err(miette::miette!(
                    "screening candidate reference or key is ambiguous"
                ));
            }
            Ok((format!("c{index}"), key.to_owned()))
        })
        .collect()
}

/// The request as a native screener reads it: each row carries a short reference in
/// place of its opaque key and owner, which Rust keeps.
pub(in crate::commands) fn screening_request(request: &Value) -> miette::Result<Value> {
    candidates(request)?;
    let mut native = request.clone();
    for (index, row) in native["candidates"]
        .as_array_mut()
        .expect("validated candidates")
        .iter_mut()
        .enumerate()
    {
        let row = row.as_object_mut().expect("candidate with key");
        row.remove("candidate_key");
        row.remove("owner");
        row.insert("candidate_ref".into(), json!(format!("c{index}")));
    }
    Ok(native)
}

fn reply_limit(request: &Value, limit: usize) -> miette::Result<usize> {
    let expansion = candidates(request)?
        .iter()
        .map(|(reference, key)| {
            serde_json::to_vec(key)
                .expect("key serializes")
                .len()
                .saturating_sub(
                    serde_json::to_vec(reference)
                        .expect("reference serializes")
                        .len(),
                )
        })
        .sum::<usize>();
    response_limit(request, &SCREENING_IDENTITIES, 0, limit)?
        .checked_sub(expansion)
        .ok_or_else(|| miette::miette!("candidate keys exceed the stored result allowance"))
}

pub(super) fn build_invocation(
    profile: &ExecutionProfile,
    request: &Value,
    limit: usize,
) -> miette::Result<Invocation> {
    let limit = reply_limit(request, limit)?;
    build(
        profile,
        &screening_request(request)?,
        crate::commands::work::screening_reply_schema(),
        &format!(
            "{SCREENING_INSTRUCTION}\nNative reply allowance: {limit} compact UTF-8 JSON bytes."
        ),
    )
}

pub(super) fn decode(value: Value, request: &Value, limit: usize) -> miette::Result<Value> {
    let mut header = identities(request, &SCREENING_IDENTITIES)?;
    check_response_size(&value, reply_limit(request, limit)?)?;
    reject_returned_identities(&value, &SCREENING_IDENTITIES)?;
    let response_revision =
        lk_core::rss_original::digest(&serde_json::to_vec(&value).expect("reply serializes"));
    let body: Body = serde_json::from_value(value)
        .map_err(|_| miette::miette!("worker screening does not match the native contract"))?;
    let max_reason_json_bytes = request["max_reason_json_bytes"]
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .filter(|limit| *limit > 0)
        .ok_or_else(|| miette::miette!("screening request has no positive reason byte limit"))?;
    let mut assigned = candidates(request)?;
    let mut seen = BTreeSet::new();
    let mut duplicate = 0;
    let mut unknown = 0;
    let mut blank = 0;
    let mut oversized = 0;
    let mut largest_oversized_json_bytes = 0;
    let mut decisions = Vec::new();
    for decision in body.decisions {
        let reason_bytes = serde_json::to_vec(&decision.reason)
            .expect("reason serializes")
            .len();
        if decision.reason.trim().is_empty() {
            blank += 1;
        } else if reason_bytes > max_reason_json_bytes {
            oversized += 1;
            largest_oversized_json_bytes = largest_oversized_json_bytes.max(reason_bytes);
        }
        if !seen.insert(decision.candidate_ref.clone()) {
            duplicate += 1;
        } else if let Some(key) = assigned.remove(&decision.candidate_ref) {
            decisions.push(
                json!({"candidate_key":key,"decision":decision.decision,"reason":decision.reason}),
            );
        } else {
            unknown += 1;
        }
    }
    if !assigned.is_empty() || duplicate != 0 || unknown != 0 {
        return Err(ScreeningRejection::ReferenceCoverage {
            missing: assigned.len(),
            duplicate,
            unknown,
            response_revision,
        }
        .into());
    }
    if blank != 0 || oversized != 0 {
        return Err(ScreeningRejection::InvalidReasons {
            blank,
            oversized,
            max_reason_json_bytes,
            largest_oversized_json_bytes,
            response_revision,
        }
        .into());
    }
    header.insert("decisions".into(), json!(decisions));
    let output = Value::Object(header);
    if serde_json::to_vec(&output)
        .expect("screening serializes")
        .len()
        + 1
        > result_limit(request, limit)?
    {
        return Err(miette::miette!(
            "complete screening exceeds max_result_bytes; no decisions were recorded"
        ));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Value {
        json!({"task_id":"ext-1","request_revision":"r".repeat(64),
            "max_result_bytes":32768,"max_reason_json_bytes":256,
            "candidates":[
                {"candidate_key":"a".repeat(64),"owner":"wiki/documents/매니페스트는-좌표만-나른다.md","path":"wiki/documents/alias.md"},
                {"candidate_key":"b".repeat(64),"owner":"wiki/documents/second.md","path":"wiki/documents/second.md"}]})
    }

    fn reply() -> Value {
        json!({"decisions":[
            {"candidate_ref":"c1","decision":"not-selected","reason":"Second card has a different declared scope."},
            {"candidate_ref":"c0","decision":"inspect","reason":"First card needs full inspection."}]})
    }

    #[test]
    fn references_replace_keys_and_bind_decisions_independently_of_response_order() {
        let request = request();
        let native = screening_request(&request).unwrap();
        for (index, original) in request["candidates"].as_array().unwrap().iter().enumerate() {
            let row = &native["candidates"][index];
            assert_eq!(row["candidate_ref"], format!("c{index}"));
            assert_eq!(row["path"], original["path"]);
            assert!(row.get("candidate_key").is_none() && row.get("owner").is_none());
        }
        let output = decode(reply(), &request, 32768).unwrap();
        assert_eq!(
            output["decisions"][0]["candidate_key"],
            request["candidates"][1]["candidate_key"]
        );
        assert_eq!(output["decisions"][0]["decision"], "not-selected");
        assert_eq!(
            output["decisions"][1]["candidate_key"],
            request["candidates"][0]["candidate_key"]
        );
        assert_eq!(output["decisions"][1]["decision"], "inspect");
        assert_eq!(output["task_id"], request["task_id"]);
        assert_eq!(output["request_revision"], request["request_revision"]);
    }

    #[test]
    fn invalid_reference_coverage_is_typed_and_never_repaired_or_zipped() {
        for (references, counts) in [
            (vec!["c0"], (1, 0, 0)),
            (vec!["c0", "c0"], (1, 1, 0)),
            (vec!["c1", "c2"], (1, 0, 1)),
            (vec!["c0", "c01"], (1, 0, 1)),
            (vec!["wiki/documents/second.md", "c1"], (1, 0, 1)),
        ] {
            let reply = json!({"decisions":references.iter().map(|reference|
                json!({"candidate_ref":reference,"decision":"inspect","reason":"Read this card."}))
                .collect::<Vec<_>>()});
            let error = decode(reply, &request(), 32768).unwrap_err();
            let rejected = error
                .downcast_ref::<ScreeningRejection>()
                .expect("typed local refusal");
            match rejected {
                ScreeningRejection::ReferenceCoverage {
                    missing,
                    duplicate,
                    unknown,
                    ..
                } => assert_eq!((*missing, *duplicate, *unknown), counts),
                _ => panic!("expected reference coverage rejection"),
            }
        }
        for field in ["candidate_key", "task_id", "request_revision"] {
            let mut forged = reply();
            if field == "candidate_key" {
                forged["decisions"][0][field] = json!("b".repeat(64));
            } else {
                forged[field] = request()[field].clone();
            }
            assert!(
                decode(forged, &request(), 32768)
                    .unwrap_err()
                    .downcast_ref::<ScreeningRejection>()
                    .is_none(),
                "{field}"
            );
        }
        let mut invalid = request();
        invalid["candidates"][1]["candidate_key"] =
            invalid["candidates"][0]["candidate_key"].clone();
        assert!(screening_request(&invalid).is_err());
    }

    #[test]
    fn reasons_count_json_utf8_bytes_and_invalid_ones_are_typed_refusals() {
        let request = request();
        for reason in [
            "근".repeat(75) + &"x".repeat(29),
            "ab".to_owned() + &"\n".repeat(126),
            "ab".to_owned() + &"\"".repeat(126),
        ] {
            assert_eq!(serde_json::to_vec(&reason).unwrap().len(), 256);
            let mut response = reply();
            response["decisions"][0]["reason"] = json!(reason);
            let output = decode(response.clone(), &request, 32768).unwrap();
            assert_eq!(
                output["decisions"][0]["reason"],
                response["decisions"][0]["reason"]
            );
            response["decisions"][0]["reason"] = json!(reason + "x");
            assert!(matches!(
                decode(response, &request, 32768)
                    .unwrap_err()
                    .downcast_ref::<ScreeningRejection>(),
                Some(ScreeningRejection::InvalidReasons {
                    oversized: 1,
                    largest_oversized_json_bytes: 257,
                    ..
                })
            ));
        }
        let mut blank = reply();
        blank["decisions"][0]["reason"] = json!(" \t\n");
        assert!(matches!(
            decode(blank, &request, 32768)
                .unwrap_err()
                .downcast_ref::<ScreeningRejection>(),
            Some(ScreeningRejection::InvalidReasons { blank: 1, .. })
        ));
        let mut response = reply();
        response["decisions"][0]["candidate_ref"] = json!("unknown");
        response["decisions"][0]["reason"] = json!(" ");
        assert!(matches!(
            decode(response, &request, 32768)
                .unwrap_err()
                .downcast_ref::<ScreeningRejection>(),
            Some(ScreeningRejection::ReferenceCoverage { .. })
        ));
        for (field, value) in [("reason", json!(false)), ("decision", json!("unknown"))] {
            let mut response = reply();
            response["decisions"][0][field] = value;
            assert!(
                decode(response, &request, 32768)
                    .unwrap_err()
                    .downcast_ref::<ScreeningRejection>()
                    .is_none()
            );
        }
    }

    #[test]
    fn reply_limit_reserves_the_stored_candidate_keys() {
        let mut request = request();
        request["max_reason_json_bytes"] = json!(32768);
        let limit = reply_limit(&request, 32768).unwrap();
        let mut reply = reply();
        let size = serde_json::to_vec(&reply).unwrap().len();
        reply["decisions"][0]["reason"] = json!(
            reply["decisions"][0]["reason"].as_str().unwrap().to_owned()
                + &"a".repeat(limit - size)
        );
        let output = decode(reply.clone(), &request, 32768).unwrap();
        assert_eq!(serde_json::to_vec(&output).unwrap().len() + 1, 32768);
        reply["decisions"][0]["reason"] =
            json!(reply["decisions"][0]["reason"].as_str().unwrap().to_owned() + "a");
        assert!(decode(reply, &request, 32768).is_err());
    }
}
