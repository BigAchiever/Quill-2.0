// Small shared text helpers used across capture (dwell-merge) and trigger (edit-delta, echo guard).

use std::collections::HashSet;

/// Jaccard word overlap in [0,1]: |A∩B| / |A∪B| over lowercased whitespace tokens. 1.0 = same
/// word set, 0.0 = disjoint. Cheap similarity for "is this a near-duplicate / a light edit".
pub fn word_overlap(a: &str, b: &str) -> f32 {
    let sa: HashSet<String> = a.split_whitespace().map(str::to_lowercase).collect();
    let sb: HashSet<String> = b.split_whitespace().map(str::to_lowercase).collect();
    if sa.is_empty() || sb.is_empty() {
        return 0.0;
    }
    let inter = sa.intersection(&sb).count() as f32;
    let union = sa.union(&sb).count() as f32;
    inter / union
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlap_separates_edits_from_new_text() {
        // A light edit of the same draft → high overlap (the improve() / dwell signal).
        let draft = "Thanks for flagging this — we moved the Comet launch to Friday for extra QA.";
        let edited = "Thanks for flagging! We moved the Comet launch to Friday to get extra QA in.";
        assert!(word_overlap(draft, edited) >= 0.35, "edit must register");
        // A completely different new message → low overlap (no false merge/improve).
        let fresh = "Can you send me the Q3 report checklist before tomorrow's standup?";
        assert!(word_overlap(draft, fresh) < 0.2, "unrelated text must not");
        // Identical text → 1.0.
        assert_eq!(word_overlap(draft, draft), 1.0);
    }
}
