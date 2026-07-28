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
        (visit_bytes, &[u8]),
        (visit_borrowed_bytes, &'de [u8]),
        (visit_byte_buf, Vec<u8>),
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
        let mut captured = CapturedKey::default();
        let value = self
            .delegate
            .next_key_seed(CaptureKey::direct(seed, &mut captured))?;
        if let Some(field) = captured.first_reserved {
            let allowed = captured
                .direct_key
                .as_deref()
                .is_some_and(|key| self.path.allows_completion_evidence_field(key));
            if !allowed {
                return Err(A::Error::custom(format!("unknown field `{field}`")));
            }
        }
        self.key = captured.direct_key;
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

#[derive(Default)]
struct CapturedKey {
    direct_key: Option<String>,
    first_reserved: Option<&'static str>,
}

#[derive(Clone, Copy)]
enum KeyContent {
    Direct,
    Compound,
}

struct CaptureKey<'a, T> {
    delegate: T,
    captured: &'a mut CapturedKey,
    content: KeyContent,
}

impl<'a, T> CaptureKey<'a, T> {
    fn direct(delegate: T, captured: &'a mut CapturedKey) -> Self {
        Self::new(delegate, captured, KeyContent::Direct)
    }

    fn compound(delegate: T, captured: &'a mut CapturedKey) -> Self {
        Self::new(delegate, captured, KeyContent::Compound)
    }

    fn new(delegate: T, captured: &'a mut CapturedKey, content: KeyContent) -> Self {
        Self {
            delegate,
            captured,
            content,
        }
    }

    fn record(&mut self, value: &str) {
        if matches!(self.content, KeyContent::Direct) && self.captured.direct_key.is_none() {
            self.captured.direct_key = Some(value.to_owned());
        }
        if self.captured.first_reserved.is_none() {
            self.captured.first_reserved = completion_evidence_config_field(value);
        }
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
            .deserialize(CaptureKey::new(deserializer, self.captured, self.content))
    }
}

macro_rules! forward_capture_deserializer {
    ($($method:ident),+ $(,)?) => {
        $(
            fn $method<V>(self, visitor: V) -> Result<V::Value, D::Error>
            where
                V: Visitor<'de>,
            {
                self.delegate
                    .$method(CaptureKey::new(visitor, self.captured, self.content))
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
                    .$method(
                        $($arg,)*
                        CaptureKey::new(visitor, self.captured, self.content),
                    )
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
            .deserialize_any(CaptureKey::new(visitor, self.captured, self.content))
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
        (visit_bytes, &[u8]),
        (visit_borrowed_bytes, &'de [u8]),
        (visit_byte_buf, Vec<u8>),
    );

    fn visit_str<E>(mut self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.record(value);
        self.delegate.visit_str(value)
    }

    fn visit_borrowed_str<E>(mut self, value: &'de str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.record(value);
        self.delegate.visit_borrowed_str(value)
    }

    fn visit_string<E>(mut self, value: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.record(&value);
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
            .visit_some(CaptureKey::compound(deserializer, self.captured))
    }

    fn visit_newtype_struct<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        self.delegate
            .visit_newtype_struct(CaptureKey::compound(deserializer, self.captured))
    }

    fn visit_seq<A>(self, sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        self.delegate
            .visit_seq(CaptureKey::compound(sequence, self.captured))
    }

    fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        self.delegate
            .visit_map(CaptureKey::compound(map, self.captured))
    }

    fn visit_enum<A>(self, data: A) -> Result<Self::Value, A::Error>
    where
        A: EnumAccess<'de>,
    {
        self.delegate
            .visit_enum(CaptureKey::compound(data, self.captured))
    }
}

impl<'a, 'de, A> SeqAccess<'de> for CaptureKey<'a, A>
where
    A: SeqAccess<'de>,
{
    type Error = A::Error;

    fn next_element_seed<S>(&mut self, seed: S) -> Result<Option<S::Value>, A::Error>
    where
        S: DeserializeSeed<'de>,
    {
        self.delegate
            .next_element_seed(CaptureKey::compound(seed, &mut *self.captured))
    }

    fn size_hint(&self) -> Option<usize> {
        self.delegate.size_hint()
    }
}

impl<'a, 'de, A> MapAccess<'de> for CaptureKey<'a, A>
where
    A: MapAccess<'de>,
{
    type Error = A::Error;

    fn next_key_seed<S>(&mut self, seed: S) -> Result<Option<S::Value>, A::Error>
    where
        S: DeserializeSeed<'de>,
    {
        self.delegate
            .next_key_seed(CaptureKey::compound(seed, &mut *self.captured))
    }

    fn next_value_seed<S>(&mut self, seed: S) -> Result<S::Value, A::Error>
    where
        S: DeserializeSeed<'de>,
    {
        self.delegate
            .next_value_seed(CaptureKey::compound(seed, &mut *self.captured))
    }

    fn size_hint(&self) -> Option<usize> {
        self.delegate.size_hint()
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
        let (value, variant) =
            self.delegate
                .variant_seed(CaptureKey::new(seed, &mut *self.captured, self.content))?;
        Ok((value, CaptureKey::new(variant, self.captured, self.content)))
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
            .newtype_variant_seed(CaptureKey::new(seed, self.captured, self.content))
    }

    fn tuple_variant<V>(self, len: usize, visitor: V) -> Result<V::Value, A::Error>
    where
        V: Visitor<'de>,
    {
        self.delegate
            .tuple_variant(len, CaptureKey::new(visitor, self.captured, self.content))
    }

    fn struct_variant<V>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, A::Error>
    where
        V: Visitor<'de>,
    {
        self.delegate.struct_variant(
            fields,
            CaptureKey::new(visitor, self.captured, self.content),
        )
    }
}
