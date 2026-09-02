//! Token estimation (DESIGN.md §6.4): bytes / 3.6 for code, bytes / 4.2 for paths.

/// Code text: o200k_base measures 2.9–4.3 bytes per token on greeg output
/// (median 3.6); the shaper's per-hit cost adds a fixed overhead for the
/// kind column, line number and chain, calibrated so that the sum of hit
/// costs tracks the rendered total (bench/tokens.py).
pub fn code(bytes: usize) -> usize {
    (bytes as f64 / 3.4).ceil() as usize
}
pub fn path(bytes: usize) -> usize {
    (bytes as f64 / 3.8).ceil() as usize
}
/// Estimate for a fully rendered text output.
pub fn rendered(bytes: usize) -> usize {
    (bytes as f64 / 3.5).ceil() as usize
}
