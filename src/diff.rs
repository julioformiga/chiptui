//! Unified diff generation (line-based, longest-common-subsequence).
//!
//! No dependency: the only inputs are two text buffers and the only output is
//! a list of unified-diff lines. Kept here rather than in `files` because a
//! content diff is its own well-defined concern, independent of the
//! local/device comparison model that lives there.
//!
//! The LCS dynamic program is `O(n*m)` in time and space. Embedded project
//! files are small, and the viewer already caps preview size
//! ([`crate::files::MAX_VIEW_BYTES`]), so this never runs on a buffer that
//! would make the table expensive.

/// Lines of context kept around each changed region, matching `diff -u`.
const DEFAULT_CONTEXT: usize = 3;

/// Produces a unified diff between `old` (the local side) and `new` (the
/// device side), with [`DEFAULT_CONTEXT`] lines of context around each hunk.
///
/// Returns an empty vector when the two texts are identical, so a caller can
/// treat "no output" as "no differences".
pub fn unified_diff(old: &str, new: &str) -> Vec<String> {
    unified_diff_with_context(old, new, DEFAULT_CONTEXT)
}

/// Same as [`unified_diff`], but with an explicit number of context lines ---
/// exposed for tests so the hunk-merging logic can be exercised precisely.
pub fn unified_diff_with_context(old: &str, new: &str, context: usize) -> Vec<String> {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();

    let ops = lcs_ops(&old_lines, &new_lines);
    if ops.iter().all(|op| matches!(op, Op::Equal(_))) {
        return Vec::new();
    }

    format_hunks(&ops, context)
}

/// One step of the edit script produced by walking an LCS table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op<'a> {
    Equal(&'a str),
    Delete(&'a str),
    Insert(&'a str),
}

/// Builds the edit script from an LCS table, walking forward so deletes
/// precede inserts within a change region (the conventional unified-diff
/// ordering, which reads as "remove the old, then add the new").
fn lcs_ops<'a>(old: &'a [&str], new: &'a [&str]) -> Vec<Op<'a>> {
    let dp = lcs_table(old, new);
    let mut ops = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < old.len() && j < new.len() {
        if old[i] == new[j] {
            ops.push(Op::Equal(old[i]));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            ops.push(Op::Delete(old[i]));
            i += 1;
        } else {
            ops.push(Op::Insert(new[j]));
            j += 1;
        }
    }
    while i < old.len() {
        ops.push(Op::Delete(old[i]));
        i += 1;
    }
    while j < new.len() {
        ops.push(Op::Insert(new[j]));
        j += 1;
    }
    ops
}

/// Length of the longest common subsequence from each suffix, indexed by
/// `old`/`new` start positions. The +1 dimensions let `lcs_ops` index
/// `dp[i+1][j+1]` without a bounds check at the edges.
fn lcs_table(old: &[&str], new: &[&str]) -> Vec<Vec<usize>> {
    let mut dp = vec![vec![0usize; new.len() + 1]; old.len() + 1];
    for i in (0..old.len()).rev() {
        for j in (0..new.len()).rev() {
            dp[i][j] = if old[i] == new[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    dp
}

/// Groups the edit script into unified-diff hunks with `context` unchanged
/// lines of surrounding context, merging hunks whose context windows overlap
/// (GNU `diff -u` does the same, so two nearby changes read as one block
/// rather than two with a shared context line repeated).
fn format_hunks(ops: &[Op<'_>], context: usize) -> Vec<String> {
    let mut out = Vec::new();

    // Index of `ops`, plus the position counters that turn it into line
    // numbers (old starts at 1, new at 1, in unified-diff convention).
    let mut k = 0;
    let n = ops.len();
    while k < n {
        // Skip leading equal lines, but remember where the next change is.
        let change_at = (k..n).find(|&idx| !matches!(ops[idx], Op::Equal(_)));
        let Some(change_at) = change_at else { break };

        // Start of this hunk's context window: at most `context` equal lines
        // before the first change.
        let hunk_start = change_at.saturating_sub(context);

        // Old/new lines consumed up to `hunk_start` --- raw counts; the
        // 1-based start line number depends on whether the hunk actually
        // contains any lines of that side (an empty side starts at 0).
        let (old_before, new_before) = count_consumed(ops, hunk_start);

        // Walk forward until `context` consecutive equal lines follow the
        // last change --- that trailing run is the hunk's context, and the
        // change after it starts a new hunk.
        let mut hunk_end = hunk_start;
        let mut trailing_equals = 0;
        let mut idx = hunk_start;
        while idx < n {
            match ops[idx] {
                Op::Equal(_) => {
                    trailing_equals += 1;
                    idx += 1;
                    // Stop once we have `context` trailing equals *and* have
                    // seen at least one change in this hunk.
                    if trailing_equals > context {
                        break;
                    }
                }
                Op::Delete(_) | Op::Insert(_) => {
                    trailing_equals = 0;
                    idx += 1;
                }
            }
            hunk_end = idx;
        }

        // Emit the hunk header, then each line. The header's line numbers
        // come from `count_consumed` (start) and `count_range` (counts), so
        // emission itself only needs the text and its sign.
        let (old_count, new_count) = count_range(ops, hunk_start, hunk_end);
        let old_start = start_line(old_before, old_count);
        let new_start = start_line(new_before, new_count);
        out.push(format_hunk_header(
            old_start, old_count, new_start, new_count,
        ));
        for op in &ops[hunk_start..hunk_end] {
            match op {
                Op::Equal(line) => out.push(format!(" {line}")),
                Op::Delete(line) => out.push(format!("-{line}")),
                Op::Insert(line) => out.push(format!("+{line}")),
            }
        }

        k = hunk_end;
    }

    out
}

/// `(old_lines, new_lines)` consumed before `end` --- raw counts, since the
/// 1-based start line of a hunk depends on whether that side has any lines
/// in it (see [`start_line`]).
fn count_consumed(ops: &[Op<'_>], end: usize) -> (usize, usize) {
    let (mut old, mut new) = (0, 0);
    for op in ops.iter().take(end) {
        match op {
            Op::Equal(_) => {
                old += 1;
                new += 1;
            }
            Op::Delete(_) => old += 1,
            Op::Insert(_) => new += 1,
        }
    }
    (old, new)
}

/// The 1-based start line number for a hunk header. GNU `diff` uses the line
/// *after* the previous content when the hunk adds to an empty side (count 0
/// starts at `before`, not `before + 1`), so an all-insert hunk at the top of
/// an empty file reads `-0,0 +1,n`.
fn start_line(before: usize, count: usize) -> usize {
    if count == 0 { before } else { before + 1 }
}

/// Number of old and new lines covered by `ops[start..end]`, for the hunk
/// header's `count` fields.
fn count_range(ops: &[Op<'_>], start: usize, end: usize) -> (usize, usize) {
    let (mut old, mut new) = (0, 0);
    for op in ops.iter().take(end).skip(start) {
        match op {
            Op::Equal(_) => {
                old += 1;
                new += 1;
            }
            Op::Delete(_) => old += 1,
            Op::Insert(_) => new += 1,
        }
    }
    (old, new)
}

/// A unified-diff hunk header. GNU `diff` omits the `,count` when the count is
/// 1, which this mirrors so the output reads as a familiar `diff -u`.
fn format_hunk_header(
    old_start: usize,
    old_count: usize,
    new_start: usize,
    new_count: usize,
) -> String {
    let old = format_range(old_start, old_count);
    let new = format_range(new_start, new_count);
    format!("@@ -{old} +{new} @@")
}

fn format_range(start: usize, count: usize) -> String {
    if count == 1 {
        start.to_string()
    } else {
        format!("{start},{count}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_texts_produce_no_output() {
        assert!(unified_diff("a\nb\n", "a\nb\n").is_empty());
        assert!(unified_diff("", "").is_empty());
    }

    #[test]
    fn a_pure_insert_emits_one_hunk() {
        // old "a\nc" is 2 lines, new "a\nb\nc" is 3.
        let diff = unified_diff("a\nc\n", "a\nb\nc\n");
        assert_eq!(diff, vec!["@@ -1,2 +1,3 @@", " a", "+b", " c"]);
    }

    #[test]
    fn a_pure_delete_emits_one_hunk() {
        let diff = unified_diff("a\nb\nc\n", "a\nc\n");
        assert_eq!(diff, vec!["@@ -1,3 +1,2 @@", " a", "-b", " c"]);
    }

    #[test]
    fn a_modification_is_delete_then_insert() {
        let diff = unified_diff("CONFIG=1\n", "CONFIG=2\n");
        assert_eq!(diff, vec!["@@ -1 +1 @@", "-CONFIG=1", "+CONFIG=2"]);
    }

    #[test]
    fn distant_changes_form_separate_hunks() {
        // With the default 3 lines of context, two changes far apart keep
        // separate hunks (their context windows do not overlap).
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
        let new = "1\n2\n3\nX\n5\n6\n7\n8\n9\nY\n";
        let diff = unified_diff(old, new);
        let headers: Vec<&String> = diff.iter().filter(|l| l.starts_with("@@")).collect();
        assert_eq!(headers.len(), 2, "two separate hunks: {diff:?}");
        // First hunk: 3 lines context before/after line 4 -> old lines 1-7.
        assert!(
            diff.contains(&"@@ -1,7 +1,7 @@".to_string()),
            "first header: {diff:?}"
        );
        assert!(diff.contains(&"-4".to_string()));
        assert!(diff.contains(&"+X".to_string()));
        assert!(diff.contains(&"-10".to_string()));
        assert!(diff.contains(&"+Y".to_string()));
    }

    #[test]
    fn nearby_changes_merge_into_one_hunk() {
        // Two changes 4 lines apart: the first's trailing context overlaps
        // the second's leading context, so a single hunk covers both.
        let old = "1\n2\n3\n4\n5\n6\n7\n";
        let new = "1\n2\nX\n4\n5\n6\nY\n";
        let diff = unified_diff(old, new);
        let headers: Vec<&String> = diff.iter().filter(|l| l.starts_with("@@")).collect();
        assert_eq!(headers.len(), 1, "one merged hunk: {diff:?}");
    }

    #[test]
    fn an_empty_old_side_is_all_inserts() {
        let diff = unified_diff("", "x\ny\n");
        assert_eq!(diff, vec!["@@ -0,0 +1,2 @@", "+x", "+y"]);
    }

    #[test]
    fn an_empty_new_side_is_all_deletes() {
        let diff = unified_diff("x\ny\n", "");
        assert_eq!(diff, vec!["@@ -1,2 +0,0 @@", "-x", "-y"]);
    }

    #[test]
    fn context_count_can_be_narrowed() {
        // With zero context, only the changed line itself appears.
        let diff = unified_diff_with_context("a\nb\nc\n", "a\nB\nc\n", 0);
        assert_eq!(diff, vec!["@@ -2 +2 @@", "-b", "+B"]);
    }

    #[test]
    fn trailing_newline_absence_is_tolerated() {
        // `str::lines` drops a trailing newline, so a file ending in "\n" and
        // one missing it only differ in that terminator and diff as equal.
        let diff = unified_diff("a\nb", "a\nb\n");
        assert!(diff.is_empty(), "only a trailing newline differs: {diff:?}");
    }
}
