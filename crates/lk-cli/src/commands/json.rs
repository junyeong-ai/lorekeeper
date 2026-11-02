use std::fmt;

use serde::de::{Error, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

pub(super) fn decode_object(bytes: &[u8]) -> serde_json::Result<Value> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = deserializer.deserialize_map(JsonVisitor)?;
    deserializer.end()?;
    Ok(value)
}

pub(super) fn deserialize_value<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Value, D::Error> {
    deserializer.deserialize_any(JsonVisitor)
}

struct JsonValue(Value);

impl<'de> Deserialize<'de> for JsonValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_value(deserializer).map(Self)
    }
}

struct JsonVisitor;

impl<'de> Visitor<'de> for JsonVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JSON with unique object fields")
    }

    fn visit_bool<E: Error>(self, value: bool) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_i64<E: Error>(self, value: i64) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_u64<E: Error>(self, value: u64) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_f64<E: Error>(self, value: f64) -> Result<Value, E> {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E: Error>(self, value: &str) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_string<E: Error>(self, value: String) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_unit<E: Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(JsonValue(value)) = sequence.next_element()? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut values = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(A::Error::custom("duplicate JSON object field"));
            }
            let JsonValue(value) = map.next_value()?;
            values.insert(key, value);
        }
        Ok(Value::Object(values))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_objects_preserve_json_values_and_compact_encoding() {
        for raw in [
            r#"{}"#,
            r#" {"rows":[{"value":null},{"value":true},{"value":false},[],{}]} "#,
            r#"{"numbers":[-9223372036854775808,18446744073709551615,9007199254740993,18446744073709551616,-0.0,1e-310,1.7976931348623157e308]}"#,
            r#"{"text":"한글 🦀\n\u0061","empty":"","keys":{"a":1,"A":2}}"#,
        ] {
            let expected: Value = serde_json::from_str(raw).unwrap();
            let actual = decode_object(raw.as_bytes()).unwrap();
            assert_eq!(actual, expected);
            assert_eq!(
                serde_json::to_vec(&actual).unwrap(),
                serde_json::to_vec(&expected).unwrap()
            );
        }
    }

    #[test]
    fn ambiguous_or_invalid_objects_are_never_normalized() {
        for raw in [
            r#"{"key":1,"key":1}"#,
            r#"{"key":1,"k\u0065y":2}"#,
            r#"{"rows":[{"key":1,"key":2}]}"#,
            r#"{"rows":[{"nested":{"key":1,"key":2}}]}"#,
            r#"{"unterminated":"#,
            r#"{"number":1e999}"#,
            r#"{} {}"#,
            r#"{"key":1,}"#,
            r#"[]"#,
            r#"null"#,
            r#""text""#,
        ] {
            assert!(decode_object(raw.as_bytes()).is_err(), "{raw}");
        }
        assert!(decode_object(b"{\"invalid\":\"\xff\"}").is_err());
        let nested = format!("{{\"nested\":{}0{}}}", "[".repeat(129), "]".repeat(129));
        assert!(decode_object(nested.as_bytes()).is_err());
    }
}
