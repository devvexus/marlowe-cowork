//! A short line diff for reminders and stale-file refusals: common prefix and suffix trimmed,
//! then an LCS over the middle (bounded), printed as `@@ line N` hunks of `-`/`+` lines.

/// Middle sizes above this many lines (each side) skip the LCS and show the region replaced.
const LCS_MAX_LINES: usize = 600;

/// The diff from `old` to `new`, at most `max_lines` body lines, then "… N more changed lines".
pub fn short_diff(old: &str, new: &str, max_lines: usize) -> String {
    short_diff_cut(old, new, max_lines).0
}

/// [`short_diff`], and whether it was cut (some changed lines are not in it).
pub fn short_diff_cut(old: &str, new: &str, max_lines: usize) -> (String, bool) {
    if old == new {
        return (String::new(), false);
    }
    let a: Vec<&str> = old.split_inclusive('\n').collect();
    let b: Vec<&str> = new.split_inclusive('\n').collect();
    let pre = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suf = a[pre..].iter().rev().zip(b[pre..].iter().rev()).take_while(|(x, y)| x == y).count();
    let (am, bm) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);
    let ops = if am.len() <= LCS_MAX_LINES && bm.len() <= LCS_MAX_LINES {
        lcs_ops(am, bm)
    } else {
        am.iter().map(|l| Op::Del(l)).chain(bm.iter().map(|l| Op::Add(l))).collect()
    };
    let mut out = Vec::new();
    let mut total = 0usize;
    let mut ib = pre;
    let mut in_hunk = false;
    for op in ops {
        match op {
            Op::Keep => {
                ib += 1;
                in_hunk = false;
            }
            Op::Del(l) | Op::Add(l) => {
                if !in_hunk {
                    out.push(format!("@@ line {} @@", ib + 1));
                    in_hunk = true;
                }
                let (sign, l) = match op {
                    Op::Del(_) => ('-', l),
                    _ => {
                        ib += 1;
                        ('+', l)
                    }
                };
                total += 1;
                if total <= max_lines {
                    let l = l.trim_end_matches(['\n', '\r']);
                    let l: String = if l.len() > 200 { format!("{}…", &l[..floor(l, 200)]) } else { l.to_string() };
                    out.push(format!("{sign}{l}"));
                }
            }
        }
    }
    let cut = total > max_lines;
    if cut {
        out.push(format!("… {} more changed lines", total - max_lines));
    }
    (out.join("\n"), cut)
}

fn floor(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

enum Op<'a> {
    Keep,
    Del(&'a str),
    Add(&'a str),
}

fn lcs_ops<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<Op<'a>> {
    let (n, m) = (a.len(), b.len());
    // dp[i][j]: LCS length of a[i..] and b[j..].
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] { dp[i + 1][j + 1] + 1 } else { dp[i + 1][j].max(dp[i][j + 1]) };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut ops = Vec::with_capacity(n + m);
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            ops.push(Op::Keep);
            i += 1;
            j += 1;
        } else if j < m && (i == n || dp[i][j + 1] >= dp[i + 1][j]) {
            // Deletions first within a hunk reads better: prefer Del when tied.
            if i < n && dp[i + 1][j] == dp[i][j + 1] {
                ops.push(Op::Del(a[i]));
                i += 1;
            } else {
                ops.push(Op::Add(b[j]));
                j += 1;
            }
        } else {
            ops.push(Op::Del(a[i]));
            i += 1;
        }
    }
    ops
}

#[cfg(test)]
mod tests {
    use super::short_diff;

    #[test]
    fn diffs_show_the_changed_lines_with_their_place() {
        let d = short_diff("a\nb\nc\n", "a\nB\nc\n", 10);
        assert_eq!(d, "@@ line 2 @@\n-b\n+B");
        let d = short_diff("a\n", "a\nb\nc\n", 10);
        assert_eq!(d, "@@ line 2 @@\n+b\n+c");
        let d = short_diff("x\n".repeat(50).as_str(), "", 5);
        assert!(d.ends_with("… 45 more changed lines"), "{d}");
        assert_eq!(short_diff("same", "same", 5), "");
    }
}
