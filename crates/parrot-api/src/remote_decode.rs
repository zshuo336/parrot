//! Resource accounting around serde's bincode deserializer. Wire length alone
//! cannot bound zero-byte values, recursion, or an owned buffer's declared size.
use serde::de::{
    self, DeserializeOwned, DeserializeSeed, EnumAccess, MapAccess, SeqAccess, VariantAccess,
    Visitor,
};
use serde::Deserializer;
use std::{fmt, marker::PhantomData};

/// Limits for one bincode payload, including all nested fields.
///
/// Values count serde value/seed visits (containers and scalar values), not
/// bytes. Strings and byte buffers additionally obey the payload byte limit.
/// User-written Deserialize implementations remain trusted application code;
/// these limits do not sandbox arbitrary code inside a custom visitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteDecodeLimits {
    /// Maximum input length. Default: 16 MiB, matching the wire frame ceiling.
    pub max_payload_bytes: usize,
    /// Total serde values visited. Default: 1,048,576, including containers.
    pub max_values: usize,
    /// Maximum nested serde depth. Default: 64; values above 128 are rejected.
    pub max_depth: usize,
}
impl Default for RemoteDecodeLimits {
    fn default() -> Self {
        Self {
            max_payload_bytes: 16 * 1024 * 1024,
            max_values: 1 << 20,
            max_depth: 64,
        }
    }
}

pub(crate) fn deserialize<T: DeserializeOwned>(
    bytes: &[u8],
    limits: RemoteDecodeLimits,
) -> Result<T, String> {
    if bytes.len() > limits.max_payload_bytes {
        return Err("bincode decode: payload byte limit exceeded".into());
    }
    if limits.max_depth > 128 {
        return Err("bincode decode: max_depth must not exceed 128".into());
    }
    let mut budget = Budget {
        remaining: limits.max_values,
        max_depth: limits.max_depth,
    };
    let seed = LimitedSeed {
        inner: PhantomData::<T>,
        budget: &mut budget,
        depth: 0,
    };
    let (value, consumed) =
        bincode::serde::seed_decode_from_slice(seed, bytes, bincode::config::standard())
            .map_err(|error| format!("bincode decode: {error}"))?;
    if consumed != bytes.len() {
        return Err(format!(
            "bincode decode: {} trailing bytes",
            bytes.len() - consumed
        ));
    }
    Ok(value)
}

struct Budget {
    remaining: usize,
    max_depth: usize,
}
impl Budget {
    fn enter<E: de::Error>(&mut self, depth: usize) -> Result<(), E> {
        if depth >= self.max_depth {
            return Err(E::custom("nesting depth limit exceeded"));
        }
        self.remaining = self
            .remaining
            .checked_sub(1)
            .ok_or_else(|| E::custom("value limit exceeded"))?;
        Ok(())
    }
    fn container<E: de::Error>(
        &self,
        count: Option<usize>,
        values_per_entry: usize,
    ) -> Result<(), E> {
        if count.is_some_and(|count| count > self.remaining / values_per_entry) {
            return Err(E::custom("declared container exceeds value limit"));
        }
        Ok(())
    }
}

struct LimitedSeed<'a, S> {
    inner: S,
    budget: &'a mut Budget,
    depth: usize,
}
impl<'de, S: DeserializeSeed<'de>> DeserializeSeed<'de> for LimitedSeed<'_, S> {
    type Value = S::Value;
    fn deserialize<D: Deserializer<'de>>(self, inner: D) -> Result<Self::Value, D::Error> {
        // Charge before entering application code, even when a custom zero-byte
        // Deserialize implementation never calls a deserializer method.
        self.budget.enter(self.depth)?;
        self.inner.deserialize(LimitedDeserializer {
            inner,
            budget: self.budget,
            depth: self.depth,
        })
    }
}

// Every construction site charges the value before passing this to application code.
struct LimitedDeserializer<'a, D> {
    inner: D,
    budget: &'a mut Budget,
    depth: usize,
}
macro_rules! deserialize_method {
    ($method:ident $(, $arg:ident : $ty:ty)*) => {
        fn $method<V: Visitor<'de>>(self, $($arg: $ty,)* visitor: V) -> Result<V::Value, Self::Error> {
            self.inner.$method($($arg,)* LimitedVisitor { inner: visitor, budget: self.budget, depth: self.depth })
        }
    };
}
impl<'de, D: Deserializer<'de>> Deserializer<'de> for LimitedDeserializer<'_, D> {
    type Error = D::Error;
    deserialize_method!(deserialize_any);
    deserialize_method!(deserialize_bool);
    deserialize_method!(deserialize_i8);
    deserialize_method!(deserialize_i16);
    deserialize_method!(deserialize_i32);
    deserialize_method!(deserialize_i64);
    deserialize_method!(deserialize_i128);
    deserialize_method!(deserialize_u8);
    deserialize_method!(deserialize_u16);
    deserialize_method!(deserialize_u32);
    deserialize_method!(deserialize_u64);
    deserialize_method!(deserialize_u128);
    deserialize_method!(deserialize_f32);
    deserialize_method!(deserialize_f64);
    deserialize_method!(deserialize_char);
    deserialize_method!(deserialize_str);
    deserialize_method!(deserialize_bytes);
    deserialize_method!(deserialize_option);
    deserialize_method!(deserialize_unit);
    deserialize_method!(deserialize_unit_struct, name: &'static str);
    deserialize_method!(deserialize_newtype_struct, name: &'static str);
    deserialize_method!(deserialize_seq);
    deserialize_method!(deserialize_tuple, len: usize);
    deserialize_method!(deserialize_tuple_struct, name: &'static str, len: usize);
    deserialize_method!(deserialize_map);
    deserialize_method!(deserialize_struct, name: &'static str, fields: &'static [&'static str]);
    deserialize_method!(deserialize_enum, name: &'static str, variants: &'static [&'static str]);
    deserialize_method!(deserialize_identifier);
    deserialize_method!(deserialize_ignored_any);
    fn is_human_readable(&self) -> bool {
        self.inner.is_human_readable()
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        // Validate the length against the actual input slice before allocating.
        // Preserve visit_string for custom visitors that require owned data.
        self.inner.deserialize_str(OwnedString(visitor))
    }
    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.inner.deserialize_bytes(OwnedBytes(visitor))
    }
}

struct OwnedString<V>(V);
impl<'de, V: Visitor<'de>> Visitor<'de> for OwnedString<V> {
    type Value = V::Value;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.expecting(f)
    }
    fn visit_borrowed_str<E: de::Error>(self, value: &'de str) -> Result<Self::Value, E> {
        self.0.visit_string(value.to_owned())
    }
    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        self.0.visit_string(value.to_owned())
    }
    fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
        self.0.visit_string(value)
    }
}
struct OwnedBytes<V>(V);
impl<'de, V: Visitor<'de>> Visitor<'de> for OwnedBytes<V> {
    type Value = V::Value;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.expecting(f)
    }
    fn visit_borrowed_bytes<E: de::Error>(self, value: &'de [u8]) -> Result<Self::Value, E> {
        self.0.visit_byte_buf(value.to_owned())
    }
    fn visit_bytes<E: de::Error>(self, value: &[u8]) -> Result<Self::Value, E> {
        self.0.visit_byte_buf(value.to_owned())
    }
    fn visit_byte_buf<E: de::Error>(self, value: Vec<u8>) -> Result<Self::Value, E> {
        self.0.visit_byte_buf(value)
    }
}

struct LimitedVisitor<'a, V> {
    inner: V,
    budget: &'a mut Budget,
    depth: usize,
}
macro_rules! visit_value {
    ($method:ident, $ty:ty) => {
        fn $method<E: de::Error>(self, value: $ty) -> Result<Self::Value, E> {
            self.inner.$method(value)
        }
    };
}
impl<'de, V: Visitor<'de>> Visitor<'de> for LimitedVisitor<'_, V> {
    type Value = V::Value;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.expecting(f)
    }
    visit_value!(visit_bool, bool);
    visit_value!(visit_i8, i8);
    visit_value!(visit_i16, i16);
    visit_value!(visit_i32, i32);
    visit_value!(visit_i64, i64);
    visit_value!(visit_i128, i128);
    visit_value!(visit_u8, u8);
    visit_value!(visit_u16, u16);
    visit_value!(visit_u32, u32);
    visit_value!(visit_u64, u64);
    visit_value!(visit_u128, u128);
    visit_value!(visit_f32, f32);
    visit_value!(visit_f64, f64);
    visit_value!(visit_char, char);
    visit_value!(visit_str, &str);
    visit_value!(visit_borrowed_str, &'de str);
    visit_value!(visit_string, String);
    visit_value!(visit_bytes, &[u8]);
    visit_value!(visit_borrowed_bytes, &'de [u8]);
    visit_value!(visit_byte_buf, Vec<u8>);
    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        self.inner.visit_none()
    }
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        self.inner.visit_unit()
    }
    fn visit_some<D: Deserializer<'de>>(self, inner: D) -> Result<Self::Value, D::Error> {
        self.budget.enter(self.depth + 1)?;
        self.inner.visit_some(LimitedDeserializer {
            inner,
            budget: self.budget,
            depth: self.depth + 1,
        })
    }
    fn visit_newtype_struct<D: Deserializer<'de>>(self, inner: D) -> Result<Self::Value, D::Error> {
        self.budget.enter(self.depth + 1)?;
        self.inner.visit_newtype_struct(LimitedDeserializer {
            inner,
            budget: self.budget,
            depth: self.depth + 1,
        })
    }
    fn visit_seq<A: SeqAccess<'de>>(self, inner: A) -> Result<Self::Value, A::Error> {
        self.budget.container(inner.size_hint(), 1)?;
        self.inner.visit_seq(LimitedAccess {
            inner,
            budget: self.budget,
            depth: self.depth + 1,
        })
    }
    fn visit_map<A: MapAccess<'de>>(self, inner: A) -> Result<Self::Value, A::Error> {
        self.budget.container(inner.size_hint(), 2)?;
        self.inner.visit_map(LimitedAccess {
            inner,
            budget: self.budget,
            depth: self.depth + 1,
        })
    }
    fn visit_enum<A: EnumAccess<'de>>(self, inner: A) -> Result<Self::Value, A::Error> {
        self.inner.visit_enum(LimitedAccess {
            inner,
            budget: self.budget,
            depth: self.depth + 1,
        })
    }
}

struct LimitedAccess<'a, A> {
    inner: A,
    budget: &'a mut Budget,
    depth: usize,
}
impl<'de, A: SeqAccess<'de>> SeqAccess<'de> for LimitedAccess<'_, A> {
    type Error = A::Error;
    fn next_element_seed<S: DeserializeSeed<'de>>(
        &mut self,
        seed: S,
    ) -> Result<Option<S::Value>, Self::Error> {
        self.inner.next_element_seed(LimitedSeed {
            inner: seed,
            budget: self.budget,
            depth: self.depth,
        })
    }
    fn size_hint(&self) -> Option<usize> {
        self.inner.size_hint().map(|n| n.min(self.budget.remaining))
    }
}
impl<'de, A: MapAccess<'de>> MapAccess<'de> for LimitedAccess<'_, A> {
    type Error = A::Error;
    fn next_key_seed<S: DeserializeSeed<'de>>(
        &mut self,
        seed: S,
    ) -> Result<Option<S::Value>, Self::Error> {
        self.inner.next_key_seed(LimitedSeed {
            inner: seed,
            budget: self.budget,
            depth: self.depth,
        })
    }
    fn next_value_seed<S: DeserializeSeed<'de>>(
        &mut self,
        seed: S,
    ) -> Result<S::Value, Self::Error> {
        self.inner.next_value_seed(LimitedSeed {
            inner: seed,
            budget: self.budget,
            depth: self.depth,
        })
    }
    fn size_hint(&self) -> Option<usize> {
        self.inner
            .size_hint()
            .map(|n| n.min(self.budget.remaining / 2))
    }
}
impl<'de, 'a, A: EnumAccess<'de>> EnumAccess<'de> for LimitedAccess<'a, A> {
    type Error = A::Error;
    type Variant = LimitedAccess<'a, A::Variant>;
    fn variant_seed<S: DeserializeSeed<'de>>(
        self,
        seed: S,
    ) -> Result<(S::Value, Self::Variant), Self::Error> {
        let (value, inner) = self.inner.variant_seed(LimitedSeed {
            inner: seed,
            budget: self.budget,
            depth: self.depth,
        })?;
        Ok((
            value,
            LimitedAccess {
                inner,
                budget: self.budget,
                depth: self.depth,
            },
        ))
    }
}
impl<'de, A: VariantAccess<'de>> VariantAccess<'de> for LimitedAccess<'_, A> {
    type Error = A::Error;
    fn unit_variant(self) -> Result<(), Self::Error> {
        self.budget.enter(self.depth)?;
        self.inner.unit_variant()
    }
    fn newtype_variant_seed<S: DeserializeSeed<'de>>(
        self,
        seed: S,
    ) -> Result<S::Value, Self::Error> {
        self.inner.newtype_variant_seed(LimitedSeed {
            inner: seed,
            budget: self.budget,
            depth: self.depth,
        })
    }
    fn tuple_variant<V: Visitor<'de>>(
        self,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.budget.enter(self.depth)?;
        self.inner.tuple_variant(
            len,
            LimitedVisitor {
                inner: visitor,
                budget: self.budget,
                depth: self.depth,
            },
        )
    }
    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.budget.enter(self.depth)?;
        self.inner.struct_variant(
            fields,
            LimitedVisitor {
                inner: visitor,
                budget: self.budget,
                depth: self.depth,
            },
        )
    }
}
