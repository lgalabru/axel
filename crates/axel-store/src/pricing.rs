//! Rough per-million-token prices for cost estimates. Shown as estimates only.

/// (input, output, cache_read, cache_write) USD per 1M tokens.
pub fn per_million(model: Option<&str>) -> (f64, f64, f64, f64) {
    let m = model.unwrap_or("").to_ascii_lowercase();
    if m.contains("haiku") {
        (1.0, 5.0, 0.10, 1.25)
    } else if m.contains("sonnet") {
        (3.0, 15.0, 0.30, 3.75)
    } else if m.contains("opus") || m.contains("fable") || m.contains("mythos") {
        (15.0, 75.0, 1.50, 18.75)
    } else if m.contains("gpt-5") || m.contains("codex") {
        (1.25, 10.0, 0.125, 0.0)
    } else {
        (3.0, 15.0, 0.30, 3.75)
    }
}

pub fn estimate(model: Option<&str>, input: i64, output: i64, cache_read: i64, cache_write: i64) -> f64 {
    let (i, o, cr, cw) = per_million(model);
    (input as f64 * i + output as f64 * o + cache_read as f64 * cr + cache_write as f64 * cw) / 1_000_000.0
}
