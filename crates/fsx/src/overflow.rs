//! Checked arithmetic helpers for filesystem aggregates.

/// Add `rhs` to `value`, saturating at `u64::MAX` and reporting whether the
/// addition overflowed. Callers can keep rendering safely while surfacing the
/// loss of precision to users.
pub fn checked_add_u64(value: &mut u64, rhs: u64) -> bool {
    if let Some(sum) = value.checked_add(rhs) {
        *value = sum;
        false
    } else {
        *value = u64::MAX;
        true
    }
}
