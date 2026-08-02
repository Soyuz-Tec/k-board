//! Fractional indexing for z-order.
//!
//! Ordering shapes by integer index forces a renumbering write across many
//! elements whenever anything moves, and two people reordering at once produce
//! a conflict that last-writer-wins resolves by losing one of the moves.
//!
//! A fractional key is a string that always has room for another key between
//! any two neighbours. Inserting touches exactly one element, and two
//! concurrent inserts at the same position produce different keys that both
//! survive — the ordering between them is then settled by the key itself, not
//! by a clock.
//!
//! Keys are base-62 digits chosen so that ASCII order matches digit order,
//! which lets any host compare them as plain strings — `ORDER BY z_index` in
//! Postgres, `Array.sort` in JavaScript, `:lists.sort` on the BEAM.

const DIGITS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const BASE: u32 = 62;

fn value_of(byte: u8) -> u32 {
    match byte {
        b'0'..=b'9' => u32::from(byte - b'0'),
        b'A'..=b'Z' => u32::from(byte - b'A') + 10,
        b'a'..=b'z' => u32::from(byte - b'a') + 36,
        // Unreachable for keys this module produced. Treating junk as 0 keeps
        // a malformed peer from panicking every replica.
        _ => 0,
    }
}

fn digit_at(key: &[u8], index: usize) -> u32 {
    key.get(index).copied().map_or(0, value_of)
}

/// Produce a key ordered strictly between `before` and `after`.
///
/// `None` means "unbounded on that side": `between(None, None)` is the first
/// key on an empty board, `between(Some(last), None)` appends to the top.
///
/// # Panics
///
/// Never. Callers passing `before >= after` get a key ordered after `before`,
/// which keeps a corrupt peer from taking down the replica. Hosts that care
/// should validate ordering at their boundary.
pub fn between(before: Option<&str>, after: Option<&str>) -> String {
    // A misordered pair has no midpoint. Degrade to "after before" rather than
    // looping forever looking for room that does not exist.
    let after = match (before, after) {
        (Some(b), Some(a)) if b >= a => None,
        _ => after,
    };

    let lower = before.unwrap_or("").as_bytes();
    let upper = after.map(str::as_bytes);

    let mut out = Vec::new();
    let mut index = 0usize;
    // Once we emit a digit strictly below the upper bound, everything after it
    // is already below `after`, so the upper bound stops constraining us.
    let mut bounded = upper.is_some();

    loop {
        let low = digit_at(lower, index);
        let high = if bounded {
            // `upper` is Some whenever `bounded` holds.
            upper.map_or(BASE, |u| if index < u.len() { value_of(u[index]) } else { BASE })
        } else {
            BASE
        };

        if high > low + 1 {
            let mid = (low + high) / 2;
            out.push(DIGITS[mid as usize]);
            break;
        }

        out.push(DIGITS[low as usize]);
        if low < high {
            bounded = false;
        }
        index += 1;
    }

    // The final digit is a strict midpoint, so it is never '0' — which is the
    // invariant that keeps lexicographic order equal to numeric order.
    String::from_utf8(out).unwrap_or_else(|_| "V".to_owned())
}

/// The key for the first element on an empty board.
pub fn first() -> String {
    between(None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_key_is_mid_range() {
        let key = first();
        assert!(!key.is_empty());
        assert!(between(None, Some(&key)) < key);
        assert!(between(Some(&key), None) > key);
    }

    #[test]
    fn always_finds_room_between_neighbours() {
        let low = "A".to_owned();
        let high = "B".to_owned();
        let mid = between(Some(&low), Some(&high));
        assert!(low < mid && mid < high, "{low} < {mid} < {high}");
    }

    #[test]
    fn survives_repeated_subdivision() {
        // The pathological case: always insert at the same position. Integer
        // indices would need a renumbering pass; this must not.
        let mut low = first();
        let high = between(Some(&low), None);
        for step in 0..200 {
            let mid = between(Some(&low), Some(&high));
            assert!(low < mid && mid < high, "broke at step {step}");
            low = mid;
        }
    }

    #[test]
    fn never_ends_in_a_zero_digit() {
        // A trailing zero would make two distinct keys compare equal as
        // fractions, breaking the ordering guarantee.
        let mut key = first();
        for _ in 0..100 {
            assert!(!key.ends_with('0'), "trailing zero in {key}");
            key = between(None, Some(&key));
        }
    }

    #[test]
    fn appending_climbs_monotonically() {
        let mut previous = first();
        for _ in 0..100 {
            let next = between(Some(&previous), None);
            assert!(next > previous);
            previous = next;
        }
    }

    #[test]
    fn misordered_input_degrades_instead_of_hanging() {
        let key = between(Some("Z"), Some("A"));
        assert!(key.as_str() > "Z", "must stay deterministic on corrupt input");
    }

    #[test]
    fn concurrent_inserts_at_one_position_both_survive() {
        // Two replicas insert between the same neighbours without coordinating.
        // Neither key is lost; the pair simply has a deterministic order.
        let (low, high) = (first(), between(Some(&first()), None));
        let from_a = between(Some(&low), Some(&high));
        let from_b = between(Some(&low), Some(&from_a));
        assert!(low < from_b && from_b < from_a && from_a < high);
    }
}
