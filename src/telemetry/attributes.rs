//! Typed span/metric attributes.
//!
//! Mirrors upstream `packages/telemetry/src/index.ts` `AttributeValue` /
//! `SpanAttributes`: a small, allocation-light key/value bag. `Attributes::EMPTY`
//! is a `const`, so a disabled telemetry path can forward an empty bag without
//! touching the allocator.

use std::borrow::Cow;

/// One attribute value, matching upstream's scalar and homogeneous-array
/// variants.
#[derive(Debug, Clone, PartialEq)]
pub enum AttributeValue {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    StringList(Vec<String>),
    IntList(Vec<i64>),
    FloatList(Vec<f64>),
    BoolList(Vec<bool>),
}

impl AttributeValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Int(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Float(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }
}

impl From<String> for AttributeValue {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for AttributeValue {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}

impl From<i64> for AttributeValue {
    fn from(value: i64) -> Self {
        Self::Int(value)
    }
}

impl From<i32> for AttributeValue {
    fn from(value: i32) -> Self {
        Self::Int(i64::from(value))
    }
}

impl From<u32> for AttributeValue {
    fn from(value: u32) -> Self {
        Self::Int(i64::from(value))
    }
}

impl From<f64> for AttributeValue {
    fn from(value: f64) -> Self {
        Self::Float(value)
    }
}

impl From<f32> for AttributeValue {
    fn from(value: f32) -> Self {
        Self::Float(f64::from(value))
    }
}

impl From<bool> for AttributeValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<Vec<String>> for AttributeValue {
    fn from(value: Vec<String>) -> Self {
        Self::StringList(value)
    }
}

impl From<Vec<i64>> for AttributeValue {
    fn from(value: Vec<i64>) -> Self {
        Self::IntList(value)
    }
}

impl From<Vec<f64>> for AttributeValue {
    fn from(value: Vec<f64>) -> Self {
        Self::FloatList(value)
    }
}

impl From<Vec<bool>> for AttributeValue {
    fn from(value: Vec<bool>) -> Self {
        Self::BoolList(value)
    }
}

/// A single named attribute. Keys are `Cow<'static, str>` so static names do
/// not allocate.
#[derive(Debug, Clone, PartialEq)]
pub struct Attribute {
    pub key: Cow<'static, str>,
    pub value: AttributeValue,
}

/// Ordered attribute bag. Lookup is linear; telemetry bags are small and the
/// empty case never allocates.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Attributes(Vec<Attribute>);

impl Attributes {
    /// Shared empty bag; `&Attributes::EMPTY` is the canonical no-allocation
    /// argument for no-op instruments and disabled spans.
    pub const EMPTY: Self = Self(Vec::new());

    pub const fn new() -> Self {
        Self(Vec::new())
    }

    /// Builder-style insert.
    #[must_use]
    pub fn with(
        mut self,
        key: impl Into<Cow<'static, str>>,
        value: impl Into<AttributeValue>,
    ) -> Self {
        self.insert(key, value);
        self
    }

    /// Insert or replace one attribute.
    pub fn insert(&mut self, key: impl Into<Cow<'static, str>>, value: impl Into<AttributeValue>) {
        let key = key.into();
        let value = value.into();
        if let Some(existing) = self.0.iter_mut().find(|attribute| attribute.key == key) {
            existing.value = value;
        } else {
            self.0.push(Attribute { key, value });
        }
    }

    pub fn get(&self, key: &str) -> Option<&AttributeValue> {
        self.0
            .iter()
            .find(|attribute| attribute.key == key)
            .map(|attribute| &attribute.value)
    }

    /// Merge `other` into `self`, replacing duplicate keys (upstream
    /// `setAttributes` semantics).
    pub fn merge(&mut self, other: &Self) {
        for attribute in &other.0 {
            self.insert(attribute.key.clone(), attribute.value.clone());
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &AttributeValue)> + '_ {
        self.0
            .iter()
            .map(|attribute| (attribute.key.as_ref(), &attribute.value))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<'a> IntoIterator for &'a Attributes {
    type Item = &'a Attribute;
    type IntoIter = std::slice::Iter<'a, Attribute>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<K, V> FromIterator<(K, V)> for Attributes
where
    K: Into<Cow<'static, str>>,
    V: Into<AttributeValue>,
{
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        let mut attributes = Self::new();
        for (key, value) in iter {
            attributes.insert(key, value);
        }
        attributes
    }
}
