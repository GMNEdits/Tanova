//! Dynamic knob values and their deterministic hash feed.
//!
//! Knobs are the user-facing parameters of a cartridge. A [`KnobTable`] is an
//! insertion-ordered map so that dynamic "User Knob" parameters keep the order
//! the artist created them in.

use indexmap::IndexMap;

/// A fundamental Nuke-style parameter value.
#[derive(Debug, Clone, PartialEq)]
pub enum KnobValue {
    /// Floating point slider or field.
    Float(f64),
    /// Integer field.
    Int(i64),
    /// Checkbox.
    Bool(bool),
    /// Free text (expressions, file paths, menu labels).
    Text(String),
    /// RGBA color, kept as linear 32-bit floats.
    Color([f32; 4]),
}

impl KnobValue {
    /// Feeds this value into a hasher with a type tag and fixed-width encoding.
    ///
    /// Values are encoded little-endian, and negative zero is folded onto
    /// positive zero so two bit-identical-looking graphs always hash the same.
    pub fn feed_hash(&self, hasher: &mut blake3::Hasher) {
        match self {
            Self::Float(value) => {
                hasher.update(&[1]);
                hasher.update(&canonical_f64(*value).to_le_bytes());
            }
            Self::Int(value) => {
                hasher.update(&[2]);
                hasher.update(&value.to_le_bytes());
            }
            Self::Bool(value) => {
                hasher.update(&[3]);
                hasher.update(&[u8::from(*value)]);
            }
            Self::Text(value) => {
                hasher.update(&[4]);
                hasher.update(&(value.len() as u64).to_le_bytes());
                hasher.update(value.as_bytes());
            }
            Self::Color(channels) => {
                hasher.update(&[5]);
                for channel in channels {
                    hasher.update(&canonical_f32(*channel).to_le_bytes());
                }
            }
        }
    }
}

/// Insertion-ordered collection of named knobs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KnobTable {
    values: IndexMap<String, KnobValue>,
}

impl KnobTable {
    /// Creates an empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an empty table with room for `capacity` knobs.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            values: IndexMap::with_capacity(capacity),
        }
    }

    /// Inserts a knob, returning the previous value of the same name.
    pub fn insert(&mut self, name: impl Into<String>, value: KnobValue) -> Option<KnobValue> {
        self.values.insert(name.into(), value)
    }

    /// Borrows a knob by name.
    pub fn get(&self, name: &str) -> Option<&KnobValue> {
        self.values.get(name)
    }

    /// Mutably borrows a knob by name.
    pub fn get_mut(&mut self, name: &str) -> Option<&mut KnobValue> {
        self.values.get_mut(name)
    }

    /// Removes a knob by name.
    pub fn remove(&mut self, name: &str) -> Option<KnobValue> {
        self.values.shift_remove(name)
    }

    /// Returns `true` if a knob with this name exists.
    pub fn contains_key(&self, name: &str) -> bool {
        self.values.contains_key(name)
    }

    /// Number of knobs.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Returns `true` if there are no knobs.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Drops every knob.
    pub fn clear(&mut self) {
        self.values.clear();
    }

    /// Iterates knobs in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &KnobValue)> + '_ {
        self.values
            .iter()
            .map(|(name, value)| (name.as_str(), value))
    }

    /// Feeds the table into a hasher in insertion order.
    pub fn feed_hash(&self, hasher: &mut blake3::Hasher) {
        hasher.update(&(self.values.len() as u64).to_le_bytes());
        for (name, value) in &self.values {
            hasher.update(&(name.len() as u64).to_le_bytes());
            hasher.update(name.as_bytes());
            value.feed_hash(hasher);
        }
    }
}

/// Folds `-0.0` onto `0.0`; all other values pass through untouched.
pub(crate) fn canonical_f64(value: f64) -> f64 {
    if value == 0.0 { 0.0 } else { value }
}

/// Folds `-0.0` onto `0.0`; all other values pass through untouched.
pub(crate) fn canonical_f32(value: f32) -> f32 {
    if value == 0.0 { 0.0 } else { value }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash_value(value: &KnobValue) -> blake3::Hash {
        let mut hasher = blake3::Hasher::new();
        value.feed_hash(&mut hasher);
        hasher.finalize()
    }

    #[test]
    fn negative_zero_hashes_like_positive_zero() {
        assert_eq!(
            hash_value(&KnobValue::Float(-0.0)),
            hash_value(&KnobValue::Float(0.0))
        );
        assert_eq!(
            hash_value(&KnobValue::Color([-0.0, 0.0, -0.0, 0.0])),
            hash_value(&KnobValue::Color([0.0; 4]))
        );
        assert_ne!(
            hash_value(&KnobValue::Float(0.0)),
            hash_value(&KnobValue::Float(1.0))
        );
    }

    #[test]
    fn knob_values_of_different_types_hash_differently() {
        // Same numeric payload, different tags: these must not collide.
        assert_ne!(
            hash_value(&KnobValue::Float(1.0)),
            hash_value(&KnobValue::Int(1))
        );
    }

    #[test]
    fn knob_table_preserves_insertion_order() {
        let mut table = KnobTable::new();
        table.insert("gain", KnobValue::Float(1.0));
        table.insert("mix", KnobValue::Float(0.5));
        table.insert("enabled", KnobValue::Bool(true));

        let names: Vec<&str> = table.iter().map(|(name, _)| name).collect();
        assert_eq!(names, ["gain", "mix", "enabled"]);
    }
}
