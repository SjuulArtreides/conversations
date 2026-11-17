use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct LiveOrder {
    pub revision: i64,
    pub session_ids: Vec<String>,
}

/// Replace only selected slots, independently within cmux's pin tiers.
pub(super) fn project(actual: &[String], desired: &[String], pinned: &[bool]) -> Vec<String> {
    let mut result = actual.to_vec();
    for tier in [true, false] {
        let slots: Vec<_> = actual
            .iter()
            .enumerate()
            .filter(|(index, id)| pinned[*index] == tier && desired.contains(id))
            .map(|(index, _)| index)
            .collect();
        let ordered: Vec<_> = desired
            .iter()
            .filter(|id| slots.iter().any(|index| &actual[*index] == *id))
            .collect();
        for (index, id) in slots.into_iter().zip(ordered) {
            result[index] = id.clone();
        }
    }
    result
}

/// Membership churn does not establish a manual move.
pub(super) fn changed(previous: &[String], current: &[String]) -> bool {
    let before: Vec<_> = previous.iter().filter(|id| current.contains(id)).collect();
    let after: Vec<_> = current.iter().filter(|id| previous.contains(id)).collect();
    before != after
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ids(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }
    #[test]
    fn web_move_preserves_unmanaged_slots_and_pin_tiers() {
        let actual = ids(&["host", "a", "other", "b", "c"]);
        assert_eq!(
            project(&actual, &ids(&["c", "b", "a"]), &[false; 5]),
            ids(&["host", "c", "other", "b", "a"])
        );
        assert_eq!(
            project(
                &actual,
                &ids(&["c", "b", "a"]),
                &[true, true, false, false, false]
            ),
            ids(&["host", "a", "other", "c", "b"])
        );
    }
    #[test]
    fn native_move_is_detected_but_reconnect_and_new_items_are_not_moves() {
        assert!(changed(&ids(&["a", "b", "c"]), &ids(&["c", "a", "b"])));
        assert!(!changed(&ids(&["a", "b"]), &ids(&["new", "a", "b"])));
        assert!(!changed(&ids(&["a", "b", "c"]), &ids(&["a", "c"])));
        assert!(!changed(&ids(&["a", "c"]), &ids(&["a", "b", "c"])));
    }
}
