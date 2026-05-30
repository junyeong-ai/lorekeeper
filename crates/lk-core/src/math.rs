//! Small numeric helpers shared across crates.

/// `count` as a whole-percent share of `total`, rounded to the nearest percent.
/// Returns 0 when `total` is 0 so callers never divide by zero. Single-sourced so
/// every category-distribution table (the `performance` CLI summary and the synthesis
/// reviews) rounds identically rather than each re-deriving the formula.
pub fn round_percent(count: usize, total: usize) -> u32 {
    if total == 0 {
        return 0;
    }
    (count as f64 / total as f64 * 100.0).round() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_to_nearest_whole_percent() {
        assert_eq!(round_percent(1, 3), 33); // 33.33 → 33
        assert_eq!(round_percent(2, 3), 67); // 66.66 → 67
        assert_eq!(round_percent(1, 8), 13); // 12.5 → 13 (half rounds away from zero)
        assert_eq!(round_percent(0, 5), 0);
        assert_eq!(round_percent(5, 5), 100);
    }

    #[test]
    fn zero_total_is_zero_not_nan() {
        assert_eq!(round_percent(3, 0), 0);
    }
}
