//! The fixed 12-color category palette — identical to metalcraft-notes'
//! `src/palette.rs` and notes-r2's `palette.rs`. A user has at most 12
//! categories, each a distinct color.

pub const COLORS: [&str; 12] = [
    "#EF4444", // red
    "#F97316", // orange
    "#F59E0B", // amber
    "#EAB308", // yellow
    "#22C55E", // green
    "#10B981", // emerald
    "#14B8A6", // teal
    "#06B6D4", // cyan
    "#3B82F6", // blue
    "#6366F1", // indigo
    "#8B5CF6", // violet
    "#EC4899", // pink
];

/// Max categories per user == palette size.
pub const MAX_CATEGORIES: usize = 12;

/// True if `color` is one of the palette colors (case-insensitive).
pub fn is_valid(color: &str) -> bool {
    COLORS.iter().any(|c| c.eq_ignore_ascii_case(color))
}

/// The first palette color not present in `used`, or `None` if all 12 are taken.
///
/// notes-r2 picks a *random* unused color to spread them out; we pick the first
/// unused one deterministically — simpler, no RNG dependency, and distinctness
/// (the point) is still guaranteed. Colors are assigned in palette order.
pub fn pick_unused(used: &[String]) -> Option<&'static str> {
    COLORS
        .iter()
        .copied()
        .find(|c| !used.iter().any(|u| u.eq_ignore_ascii_case(c)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_distinct_then_exhausts() {
        let mut used: Vec<String> = Vec::new();
        for _ in 0..MAX_CATEGORIES {
            let c = pick_unused(&used).unwrap();
            assert!(!used.iter().any(|u| u == c));
            used.push(c.to_string());
        }
        assert!(pick_unused(&used).is_none());
    }

    #[test]
    fn validity() {
        assert!(is_valid("#ef4444"));
        assert!(!is_valid("#123456"));
    }
}
