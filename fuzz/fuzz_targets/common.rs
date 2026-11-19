//! Shared helpers for the fuzz targets (included with `#[path]`, not a crate).

/// Splits fuzz input into header lines: `name: value` per line. Lines without `:` are skipped.
pub fn headers(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim_start().to_owned()))
        .collect()
}
