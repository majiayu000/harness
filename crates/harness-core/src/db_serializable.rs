/// Trait for types that can be serialized to/from database string representation.
///
/// This trait provides a standard interface for converting enum types to their
/// database string representation and back, eliminating duplicate conversion
/// functions across the codebase.
pub trait DbSerializable: Sized {
    /// Convert the value to its database string representation.
    fn to_db_str(&self) -> &'static str;

    /// Parse a database string into the value.
    ///
    /// Returns an error if the string is not a valid representation.
    fn from_db_str(s: &str) -> anyhow::Result<Self>;
}
