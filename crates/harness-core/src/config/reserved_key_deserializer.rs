use super::{completion_evidence_config_field, COMPLETION_EVIDENCE_CONFIG_FIELDS};
use serde::{
    de::{
        self, DeserializeSeed, EnumAccess, Error as _, MapAccess, SeqAccess, VariantAccess, Visitor,
    },
    Deserialize, Deserializer,
};
use std::fmt;

/// Deserialize without changing the source format's native type semantics,
/// while rejecting reserved keys anywhere outside their canonical location.
pub(super) fn deserialize<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(Tracked::root(deserializer))
}

#[derive(Clone, Copy)]
struct ConfigPath {
    is_root: bool,
    is_root_workflow: bool,
}

impl ConfigPath {
    const ROOT: Self = Self {
        is_root: true,
        is_root_workflow: false,
    };
    const NON_CANONICAL: Self = Self {
        is_root: false,
        is_root_workflow: false,
    };

    fn field(self, key: Option<&str>) -> Self {
        Self {
            is_root: false,
            is_root_workflow: self.is_root && key == Some("workflow"),
        }
    }

    fn nested(self) -> Self {
        Self::NON_CANONICAL
    }

    fn allows_completion_evidence_field(self, key: &str) -> bool {
        self.is_root_workflow && key == COMPLETION_EVIDENCE_CONFIG_FIELDS[0]
    }
}

struct Tracked<T> {
    delegate: T,
    path: ConfigPath,
}

impl<T> Tracked<T> {
    fn root(delegate: T) -> Self {
        Self {
            delegate,
            path: ConfigPath::ROOT,
        }
    }

    fn new(delegate: T, path: ConfigPath) -> Self {
        Self { delegate, path }
    }
}

macro_rules! forward_deserializer {
    ($($method:ident),+ $(,)?) => {
        $(
            fn $method<V>(self, visitor: V) -> Result<V::Value, D::Error>
            where
                V: Visitor<'de>,
            {
                self.delegate.$method(Tracked::new(visitor, self.path))
            }
        )+
    };
}

macro_rules! forward_deserializer_with_args {
    ($(fn $method:ident($($arg:ident: $type:ty),*)),+ $(,)?) => {
        $(
            fn $method<V>(self, $($arg: $type,)* visitor: V) -> Result<V::Value, D::Error>
            where
                V: Visitor<'de>,
            {
                self.delegate
                    .$method($($arg,)* Tracked::new(visitor, self.path))
            }
        )+
    };
}

impl<'de, D> Deserializer<'de> for Tracked<D>
where
    D: Deserializer<'de>,
{
    type Error = D::Error;

    forward_deserializer!(
        deserialize_any,
        deserialize_bool,
        deserialize_u8,
        deserialize_u16,
        deserialize_u32,
        deserialize_u64,
        deserialize_u128,
        deserialize_i8,
        deserialize_i16,
        deserialize_i32,
        deserialize_i64,
        deserialize_i128,
        deserialize_f32,
        deserialize_f64,
        deserialize_char,
        deserialize_str,
        deserialize_string,
        deserialize_bytes,
        deserialize_byte_buf,
        deserialize_option,
        deserialize_unit,
        deserialize_seq,
        deserialize_map,
        deserialize_identifier,
    );

    forward_deserializer_with_args!(
        fn deserialize_unit_struct(name: &'static str),
        fn deserialize_newtype_struct(name: &'static str),
        fn deserialize_tuple(len: usize),
        fn deserialize_tuple_struct(name: &'static str, len: usize),
        fn deserialize_struct(name: &'static str, fields: &'static [&'static str]),
        fn deserialize_enum(name: &'static str, variants: &'static [&'static str]),
    );

    fn deserialize_ignored_any<V>(self, visitor: V) -> Result<V::Value, D::Error>
    where
        V: Visitor<'de>,
    {
        // Some formats skip ignored values without visiting their contents.
        // Force a recursive walk so reserved keys cannot hide in extensions.
        self.delegate
            .deserialize_any(Tracked::new(visitor, self.path))
    }

    fn is_human_readable(&self) -> bool {
        self.delegate.is_human_readable()
    }
}

macro_rules! forward_visit {
    ($(($method:ident, $type:ty)),+ $(,)?) => {
        $(
            fn $method<E>(self, value: $type) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                self.delegate.$method(value)
            }
        )+
    };
}

impl<'de, V> Visitor<'de> for Tracked<V>
where
    V: Visitor<'de>,
{
    type Value = V::Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.delegate.expecting(formatter)
    }

    forward_visit!(
        (visit_bool, bool),
        (visit_i8, i8),
        (visit_i16, i16),
        (visit_i32, i32),
        (visit_i64, i64),
        (visit_i128, i128),
        (visit_u8, u8),
        (visit_u16, u16),
        (visit_u32, u32),
        (visit_u64, u64),
        (visit_u128, u128),
        (visit_f32, f32),
        (visit_f64, f64),
        (visit_char, char),
    );

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.delegate.visit_str(value)
    }

    fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.delegate.visit_borrowed_str(value)
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.delegate.visit_string(value)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.delegate.visit_unit()
    }

    fn visit_none<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.delegate.visit_none()
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        self.delegate
            .visit_some(Tracked::new(deserializer, self.path.nested()))
    }

    fn visit_newtype_struct<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        self.delegate
            .visit_newtype_struct(Tracked::new(deserializer, self.path.nested()))
    }

    fn visit_seq<A>(self, sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        self.delegate
            .visit_seq(Tracked::new(sequence, self.path.nested()))
    }

    fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        self.delegate.visit_map(TrackedMap::new(map, self.path))
    }

    fn visit_enum<A>(self, data: A) -> Result<Self::Value, A::Error>
    where
        A: EnumAccess<'de>,
    {
        self.delegate
            .visit_enum(Tracked::new(data, self.path.nested()))
    }

    fn visit_bytes<E>(self, value: &[u8]) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.delegate.visit_bytes(value)
    }

    fn visit_borrowed_bytes<E>(self, value: &'de [u8]) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.delegate.visit_borrowed_bytes(value)
    }

    fn visit_byte_buf<E>(self, value: Vec<u8>) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.delegate.visit_byte_buf(value)
    }
}

impl<'de, S> DeserializeSeed<'de> for Tracked<S>
where
    S: DeserializeSeed<'de>,
{
    type Value = S::Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        self.delegate
            .deserialize(Tracked::new(deserializer, self.path))
    }
}

impl<'de, A> SeqAccess<'de> for Tracked<A>
where
    A: SeqAccess<'de>,
{
    type Error = A::Error;

    fn next_element_seed<S>(&mut self, seed: S) -> Result<Option<S::Value>, A::Error>
    where
        S: DeserializeSeed<'de>,
    {
        self.delegate
            .next_element_seed(Tracked::new(seed, self.path))
    }

    fn size_hint(&self) -> Option<usize> {
        self.delegate.size_hint()
    }
}

struct TrackedMap<A> {
    delegate: A,
    path: ConfigPath,
    key: Option<String>,
}

impl<A> TrackedMap<A> {
    fn new(delegate: A, path: ConfigPath) -> Self {
        Self {
            delegate,
            path,
            key: None,
        }
    }
}

impl<'de, A> MapAccess<'de> for TrackedMap<A>
where
    A: MapAccess<'de>,
{
    type Error = A::Error;

    fn next_key_seed<S>(&mut self, seed: S) -> Result<Option<S::Value>, A::Error>
    where
        S: DeserializeSeed<'de>,
    {
        self.key = None;
        let value = self
            .delegate
            .next_key_seed(CaptureKey::new(seed, &mut self.key))?;
        if let Some(key) = self.key.as_deref() {
            if let Some(field) = completion_evidence_config_field(key) {
                if !self.path.allows_completion_evidence_field(key) {
                    return Err(A::Error::custom(format!("unknown field `{field}`")));
                }
            }
        }
        Ok(value)
    }

    fn next_value_seed<S>(&mut self, seed: S) -> Result<S::Value, A::Error>
    where
        S: DeserializeSeed<'de>,
    {
        let path = self.path.field(self.key.take().as_deref());
        self.delegate.next_value_seed(Tracked::new(seed, path))
    }

    fn size_hint(&self) -> Option<usize> {
        self.delegate.size_hint()
    }
}

impl<'de, A> EnumAccess<'de> for Tracked<A>
where
    A: EnumAccess<'de>,
{
    type Error = A::Error;
    type Variant = Tracked<A::Variant>;

    fn variant_seed<S>(self, seed: S) -> Result<(S::Value, Self::Variant), A::Error>
    where
        S: DeserializeSeed<'de>,
    {
        self.delegate
            .variant_seed(seed)
            .map(|(value, delegate)| (value, Tracked::new(delegate, self.path)))
    }
}

impl<'de, A> VariantAccess<'de> for Tracked<A>
where
    A: VariantAccess<'de>,
{
    type Error = A::Error;

    fn unit_variant(self) -> Result<(), A::Error> {
        self.delegate.unit_variant()
    }

    fn newtype_variant_seed<S>(self, seed: S) -> Result<S::Value, A::Error>
    where
        S: DeserializeSeed<'de>,
    {
        self.delegate
            .newtype_variant_seed(Tracked::new(seed, self.path))
    }

    fn tuple_variant<V>(self, len: usize, visitor: V) -> Result<V::Value, A::Error>
    where
        V: Visitor<'de>,
    {
        self.delegate
            .tuple_variant(len, Tracked::new(visitor, self.path))
    }

    fn struct_variant<V>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, A::Error>
    where
        V: Visitor<'de>,
    {
        self.delegate
            .struct_variant(fields, Tracked::new(visitor, self.path))
    }
}

struct CaptureKey<'a, T> {
    delegate: T,
    key: &'a mut Option<String>,
}

impl<'a, T> CaptureKey<'a, T> {
    fn new(delegate: T, key: &'a mut Option<String>) -> Self {
        Self { delegate, key }
    }
}

impl<'a, 'de, S> DeserializeSeed<'de> for CaptureKey<'a, S>
where
    S: DeserializeSeed<'de>,
{
    type Value = S::Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        self.delegate
            .deserialize(CaptureKey::new(deserializer, self.key))
    }
}

macro_rules! forward_capture_deserializer {
    ($($method:ident),+ $(,)?) => {
        $(
            fn $method<V>(self, visitor: V) -> Result<V::Value, D::Error>
            where
                V: Visitor<'de>,
            {
                self.delegate.$method(CaptureKey::new(visitor, self.key))
            }
        )+
    };
}

macro_rules! forward_capture_deserializer_with_args {
    ($(fn $method:ident($($arg:ident: $type:ty),*)),+ $(,)?) => {
        $(
            fn $method<V>(self, $($arg: $type,)* visitor: V) -> Result<V::Value, D::Error>
            where
                V: Visitor<'de>,
            {
                self.delegate
                    .$method($($arg,)* CaptureKey::new(visitor, self.key))
            }
        )+
    };
}

impl<'a, 'de, D> Deserializer<'de> for CaptureKey<'a, D>
where
    D: Deserializer<'de>,
{
    type Error = D::Error;

    forward_capture_deserializer!(
        deserialize_any,
        deserialize_bool,
        deserialize_u8,
        deserialize_u16,
        deserialize_u32,
        deserialize_u64,
        deserialize_u128,
        deserialize_i8,
        deserialize_i16,
        deserialize_i32,
        deserialize_i64,
        deserialize_i128,
        deserialize_f32,
        deserialize_f64,
        deserialize_char,
        deserialize_str,
        deserialize_string,
        deserialize_bytes,
        deserialize_byte_buf,
        deserialize_option,
        deserialize_unit,
        deserialize_seq,
        deserialize_map,
        deserialize_identifier,
    );

    forward_capture_deserializer_with_args!(
        fn deserialize_unit_struct(name: &'static str),
        fn deserialize_newtype_struct(name: &'static str),
        fn deserialize_tuple(len: usize),
        fn deserialize_tuple_struct(name: &'static str, len: usize),
        fn deserialize_struct(name: &'static str, fields: &'static [&'static str]),
        fn deserialize_enum(name: &'static str, variants: &'static [&'static str]),
    );

    fn is_human_readable(&self) -> bool {
        self.delegate.is_human_readable()
    }

    fn deserialize_ignored_any<V>(self, visitor: V) -> Result<V::Value, D::Error>
    where
        V: Visitor<'de>,
    {
        // Ignored map keys need the same recursive treatment as ignored
        // values; otherwise compound keys can hide reserved nested fields.
        self.delegate
            .deserialize_any(CaptureKey::new(visitor, self.key))
    }
}

impl<'a, 'de, V> Visitor<'de> for CaptureKey<'a, V>
where
    V: Visitor<'de>,
{
    type Value = V::Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.delegate.expecting(formatter)
    }

    forward_visit!(
        (visit_bool, bool),
        (visit_i8, i8),
        (visit_i16, i16),
        (visit_i32, i32),
        (visit_i64, i64),
        (visit_i128, i128),
        (visit_u8, u8),
        (visit_u16, u16),
        (visit_u32, u32),
        (visit_u64, u64),
        (visit_u128, u128),
        (visit_f32, f32),
        (visit_f64, f64),
        (visit_char, char),
    );

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        *self.key = Some(value.to_owned());
        self.delegate.visit_str(value)
    }

    fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        *self.key = Some(value.to_owned());
        self.delegate.visit_borrowed_str(value)
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        *self.key = Some(value.clone());
        self.delegate.visit_string(value)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.delegate.visit_unit()
    }

    fn visit_none<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.delegate.visit_none()
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        self.delegate
            .visit_some(CaptureKey::new(deserializer, self.key))
    }

    fn visit_newtype_struct<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        self.delegate
            .visit_newtype_struct(CaptureKey::new(deserializer, self.key))
    }

    fn visit_seq<A>(self, sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        self.delegate
            .visit_seq(Tracked::new(sequence, ConfigPath::NON_CANONICAL))
    }

    fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        self.delegate
            .visit_map(TrackedMap::new(map, ConfigPath::NON_CANONICAL))
    }

    fn visit_enum<A>(self, data: A) -> Result<Self::Value, A::Error>
    where
        A: EnumAccess<'de>,
    {
        self.delegate.visit_enum(CaptureKey::new(data, self.key))
    }

    fn visit_bytes<E>(self, value: &[u8]) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.delegate.visit_bytes(value)
    }

    fn visit_borrowed_bytes<E>(self, value: &'de [u8]) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.delegate.visit_borrowed_bytes(value)
    }

    fn visit_byte_buf<E>(self, value: Vec<u8>) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.delegate.visit_byte_buf(value)
    }
}

impl<'a, 'de, A> EnumAccess<'de> for CaptureKey<'a, A>
where
    A: EnumAccess<'de>,
{
    type Error = A::Error;
    type Variant = CaptureKey<'a, A::Variant>;

    fn variant_seed<S>(self, seed: S) -> Result<(S::Value, Self::Variant), A::Error>
    where
        S: DeserializeSeed<'de>,
    {
        let (value, variant) = self
            .delegate
            .variant_seed(CaptureKey::new(seed, &mut *self.key))?;
        Ok((value, CaptureKey::new(variant, self.key)))
    }
}

impl<'a, 'de, A> VariantAccess<'de> for CaptureKey<'a, A>
where
    A: VariantAccess<'de>,
{
    type Error = A::Error;

    fn unit_variant(self) -> Result<(), A::Error> {
        self.delegate.unit_variant()
    }

    fn newtype_variant_seed<S>(self, seed: S) -> Result<S::Value, A::Error>
    where
        S: DeserializeSeed<'de>,
    {
        self.delegate
            .newtype_variant_seed(CaptureKey::new(seed, self.key))
    }

    fn tuple_variant<V>(self, len: usize, visitor: V) -> Result<V::Value, A::Error>
    where
        V: Visitor<'de>,
    {
        self.delegate
            .tuple_variant(len, CaptureKey::new(visitor, self.key))
    }

    fn struct_variant<V>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, A::Error>
    where
        V: Visitor<'de>,
    {
        self.delegate
            .struct_variant(fields, CaptureKey::new(visitor, self.key))
    }
}
