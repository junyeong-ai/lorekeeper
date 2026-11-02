use std::collections::BTreeSet;
use std::fmt;

use lk_queue::{TaskKind, TaskTarget};
use miette::IntoDiagnostic;
use serde::de::{
    DeserializeOwned, Error, MapAccess, SeqAccess, Visitor, value::MapAccessDeserializer,
};
use serde::{Deserialize, Deserializer};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RetainedTask {
    task_id: String,
    #[serde(deserialize_with = "string_enum")]
    kind: TaskKind,
    #[serde(rename = "created_at")]
    _created_at: jiff::Timestamp,
    cache_hash: String,
    #[serde(rename = "input", deserialize_with = "historical_input")]
    _input: (),
    #[serde(deserialize_with = "retained_target")]
    target: TaskTarget,
}

fn retained_target<'de, D: Deserializer<'de>>(deserializer: D) -> Result<TaskTarget, D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Target {
        vault_path: String,
        #[serde(deserialize_with = "string_enum")]
        kind: lk_queue::TargetKind,
        anchor: String,
    }
    struct TargetObject;
    impl<'de> Visitor<'de> for TargetObject {
        type Value = TaskTarget;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a historical queue target object")
        }
        fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
            let target = Target::deserialize(MapAccessDeserializer::new(map))?;
            Ok(TaskTarget {
                vault_path: target.vault_path,
                kind: target.kind,
                anchor: target.anchor,
            })
        }
    }
    deserializer.deserialize_map(TargetObject)
}

fn string_enum<'de, D: Deserializer<'de>, T: DeserializeOwned>(
    deserializer: D,
) -> Result<T, D::Error> {
    let value = String::deserialize(deserializer)?;
    serde_json::from_value(serde_json::Value::String(value)).map_err(D::Error::custom)
}

struct HistoricalValue;

struct HistoricalJson;

impl<'de> Visitor<'de> for HistoricalJson {
    type Value = HistoricalValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JSON with unique object fields")
    }

    fn visit_bool<E: Error>(self, _: bool) -> Result<Self::Value, E> {
        Ok(HistoricalValue)
    }
    fn visit_i64<E: Error>(self, _: i64) -> Result<Self::Value, E> {
        Ok(HistoricalValue)
    }
    fn visit_u64<E: Error>(self, _: u64) -> Result<Self::Value, E> {
        Ok(HistoricalValue)
    }
    fn visit_f64<E: Error>(self, _: f64) -> Result<Self::Value, E> {
        Ok(HistoricalValue)
    }
    fn visit_str<E: Error>(self, _: &str) -> Result<Self::Value, E> {
        Ok(HistoricalValue)
    }
    fn visit_unit<E: Error>(self) -> Result<Self::Value, E> {
        Ok(HistoricalValue)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
        while sequence.next_element::<HistoricalValue>()?.is_some() {}
        Ok(HistoricalValue)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut keys = BTreeSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key) {
                return Err(A::Error::custom("duplicate historical queue object field"));
            }
            map.next_value::<HistoricalValue>()?;
        }
        Ok(HistoricalValue)
    }
}

impl<'de> Deserialize<'de> for HistoricalValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(HistoricalJson)
    }
}

fn historical_input<'de, D: Deserializer<'de>>(deserializer: D) -> Result<(), D::Error> {
    deserializer.deserialize_map(HistoricalJson).map(|_| ())
}

pub(super) fn reference(raw: &str) -> miette::Result<(String, String)> {
    if !raw.trim_start().starts_with('{') {
        return Err(miette::miette!(
            "retained queue reference requires an object"
        ));
    }
    let task: RetainedTask = serde_json::from_str(raw).into_diagnostic()?;
    if task.task_id.trim().is_empty()
        || task.cache_hash.len() != 32
        || !task
            .cache_hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || !task.kind.supports_target(task.target.kind)
        || super::super::resolve_target_path(&task.target.vault_path)
            .is_none_or(|path| path.as_os_str().is_empty())
        || !task.target.anchor.starts_with("## ")
        || task.target.anchor.trim() == "##"
        || task.target.anchor.contains(['\n', '\r'])
    {
        return Err(miette::miette!("invalid retained queue reference identity"));
    }
    Ok((task.task_id, task.cache_hash))
}
