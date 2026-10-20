//! Head-and-tail truncation of tool output.

/// The most bytes of text one tool result hands the model (plus the marker).
pub const MAX_TOOL_OUTPUT: usize = 30_000;

/// `s` when it fits in `max` bytes; otherwise its first 2/3 of `max` bytes, the marker
/// `\n... [truncated N bytes] ...\n` and its last 1/3, both cut on char boundaries
/// (each side rounds inward, so it may be a few bytes short). `N` is the number of
/// bytes left out.
pub fn truncate_middle(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let head_max = max * 2 / 3;
    let tail_max = max - head_max;
    let head_end = s.floor_char_boundary(head_max);
    let tail_start = s.ceil_char_boundary(s.len() - tail_max);
    let cut = tail_start - head_end;
    format!(
        "{}\n... [truncated {cut} bytes] ...\n{}",
        &s[..head_end],
        &s[tail_start..]
    )
}
