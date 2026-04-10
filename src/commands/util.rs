//! Shared helpers for the command implementations.

/// Return the first 8 bytes of an ID as a display-friendly short form,
/// falling back to the full ID if it is shorter than 8 bytes.
///
/// Uses `str::get` so non-ASCII IDs do not panic on char boundaries.
/// Run IDs in this codebase are ASCII (UUID-derived), so the behavior
/// for them is equivalent to `&id[..8]`.
pub(crate) fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}
