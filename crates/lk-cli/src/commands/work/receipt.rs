use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;

use serde::de::{DeserializeOwned, Error, MapAccess, Visitor, value::MapAccessDeserializer};
use serde::{Deserialize, Deserializer};

pub(super) fn decode<T: DeserializeOwned>(raw: &str) -> serde_json::Result<T> {
    struct Object<T>(T);

    impl<'de, T: Deserialize<'de>> Deserialize<'de> for Object<T> {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct ObjectVisitor<T>(PhantomData<T>);

            impl<'de, T: Deserialize<'de>> Visitor<'de> for ObjectVisitor<T> {
                type Value = Object<T>;

                fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                    formatter.write_str("a receipt object")
                }

                fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                    T::deserialize(MapAccessDeserializer::new(map)).map(Object)
                }
            }

            deserializer.deserialize_map(ObjectVisitor(PhantomData))
        }
    }

    serde_json::from_str::<Object<T>>(raw).map(|object| object.0)
}

pub(super) fn unique_entries<'de, D, T>(deserializer: D) -> Result<BTreeMap<String, T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Entries<T>(PhantomData<T>);

    impl<'de, T: Deserialize<'de>> Visitor<'de> for Entries<T> {
        type Value = BTreeMap<String, T>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("distinct receipt entry keys")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut entries = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, T>()? {
                if entries.insert(key, value).is_some() {
                    return Err(A::Error::custom("duplicate receipt entry key"));
                }
            }
            Ok(entries)
        }
    }

    deserializer.deserialize_map(Entries(PhantomData))
}
