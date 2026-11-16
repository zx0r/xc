//! Search pipeline: bitmask prune → nucleo fuzzy → score ranking.
//! Borrows Arena immutably. Returns owned `SearchResult`.

use std::borrow::Cow;
use std::collections::BinaryHeap;

use nucleo_matcher::{Matcher, Utf32Str};

use crate::arena::{Arena, Scope, Scored, char_mask};

// ── Constants ────────────────────────────────────────────────────

/// Maximum scored candidates retained for non-empty search queries.
pub const TOP_K: usize = 1000;

/// Chunk size for parallel search. Below this, single-thread.
#[cfg(not(miri))]
const PAR_THRESHOLD: usize = 500;

/// Rayon chunk size for parallel iteration.
/// 4096 entries * 8 bytes (mask) = 32KB (fits 100% within L1 Data Cache of a modern CPU core).
const CHUNK_SIZE: usize = 4096;

/// Stack scratch buffer capacity for zero-allocation query normalization.
pub const STACK_QUERY_CAPACITY: usize = 256;

// ── Query Parsing ────────────────────────────────────────────────

/// Parsed query tokens extracted once per keystroke.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum QueryMode<'a> {
    Empty,
    Prefix(&'a str),    // ^foo
    Substring(&'a str), // 'foo
    Suffix(&'a str),    // foo$
    Fuzzy(&'a str),     // default
}

/// Match algorithm kind dispatched to matcher.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MatchKind {
    Prefix,
    Substring,
    Suffix,
    Fuzzy,
}

impl QueryMode<'_> {
    #[must_use]
    pub const fn match_kind(self) -> Option<MatchKind> {
        match self {
            Self::Empty => None,
            Self::Prefix(_) => Some(MatchKind::Prefix),
            Self::Substring(_) => Some(MatchKind::Substring),
            Self::Suffix(_) => Some(MatchKind::Suffix),
            Self::Fuzzy(_) => Some(MatchKind::Fuzzy),
        }
    }
}

#[allow(
    clippy::option_if_let_else,
    reason = "Explicit if-let branching provides clearer parser control flow"
)]
fn parse_query(q: &str) -> QueryMode<'_> {
    let t = q.trim();
    if t.is_empty() {
        return QueryMode::Empty;
    }
    if let Some(r) = t.strip_prefix('^') {
        if r.is_empty() {
            QueryMode::Empty
        } else {
            QueryMode::Prefix(r)
        }
    } else if let Some(r) = t.strip_prefix('\'') {
        if r.is_empty() {
            QueryMode::Empty
        } else {
            QueryMode::Substring(r)
        }
    } else if let Some(r) = t.strip_suffix('$') {
        if r.is_empty() {
            QueryMode::Empty
        } else {
            QueryMode::Suffix(r)
        }
    } else {
        QueryMode::Fuzzy(t)
    }
}

// ── Preprocessed Query ───────────────────────────────────────────

/// Preprocessed query with guaranteed nucleo-matcher contract compliance and 0 heap allocations.
#[derive(Debug)]
pub struct ProcessedQuery<'a> {
    pub text: &'a str,
    pub ignore_case: bool,
    pub mask: u64,
}

impl<'a> ProcessedQuery<'a> {
    /// Normalizes query with zero heap allocations on the hot path (stack buffer `[u8; 256]`).
    ///
    /// Implements Smart-Case:
    /// - If uppercase ASCII character is detected: `ignore_case = false` (case-sensitive matching).
    ///   Query text is borrowed verbatim with 0 copies and 0 allocations.
    /// - If only lowercase ASCII characters: `ignore_case = true` (case-insensitive matching).
    ///   Query text is borrowed verbatim with 0 copies and 0 allocations.
    ///
    /// If `smart_case = false` (forced case-insensitive mode) and query contains uppercase:
    ///   Lowercases into stack scratch buffer `[u8; 256]` (0 heap allocations).
    #[inline]
    pub fn new(
        raw: &'a str,
        scratch: &'a mut [u8; STACK_QUERY_CAPACITY],
        overflow: &'a mut Option<String>,
        smart_case: bool,
    ) -> Self {
        let bytes = raw.as_bytes();
        let has_upper = bytes.iter().any(u8::is_ascii_uppercase);

        let (text, ignore_case) = if smart_case {
            if has_upper { (raw, false) } else { (raw, true) }
        } else if has_upper {
            if bytes.len() <= STACK_QUERY_CAPACITY {
                scratch[..bytes.len()].copy_from_slice(bytes);
                scratch[..bytes.len()].make_ascii_lowercase();
                let s = core::str::from_utf8(&scratch[..bytes.len()]).unwrap_or(raw);
                (s, true)
            } else {
                let s = overflow.insert(raw.to_ascii_lowercase());
                (s.as_str(), true)
            }
        } else {
            (raw, true)
        };

        let mask = char_mask(text.as_bytes());
        Self {
            text,
            ignore_case,
            mask,
        }
    }
}

// ── Search Result ────────────────────────────────────────────────

/// Immutable search result: indices into Arena, sorted by score descending.
#[derive(Debug, Clone)]
pub struct SearchResult<'a> {
    pub indices: Cow<'a, [u32]>,
    pub total: u32,
}

impl SearchResult<'_> {
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            indices: Cow::Borrowed(&[]),
            total: 0,
        }
    }
}

// ── Search Chunk Engine ──────────────────────────────────────────

fn search_chunk(
    arena: &Arena,
    indices: &[u32],
    kind: MatchKind,
    needle: Utf32Str<'_>,
    q_bytes: &[u8],
    ignore_case: bool,
    query_mask: u64,
) -> (BinaryHeap<Scored>, u32) {
    let fast_prefix = matches!(kind, MatchKind::Prefix | MatchKind::Fuzzy);

    let mut config = nucleo_matcher::Config::DEFAULT;
    config.ignore_case = ignore_case;
    let mut matcher = Matcher::new(config);

    let mut haystack_buf = Vec::new();
    let mut heap = BinaryHeap::with_capacity(TOP_K.min(indices.len()).min(128));
    let mut total_matches: u32 = 0;

    for &idx in indices {
        if query_mask == 0 || (arena.masks[idx as usize] & query_mask) == query_mask {
            let cmd = arena.cmd_str(idx);
            let h_bytes = cmd.as_bytes();

            let score = if fast_prefix
                && h_bytes.len() >= q_bytes.len()
                && (if ignore_case {
                    h_bytes[..q_bytes.len()].eq_ignore_ascii_case(q_bytes)
                } else {
                    &h_bytes[..q_bytes.len()] == q_bytes
                }) {
                Some(20_000_u32.saturating_sub(cmd.len().min(10_000) as u32))
            } else {
                let haystack = Utf32Str::new(cmd, &mut haystack_buf);
                let s = match kind {
                    MatchKind::Prefix => matcher.prefix_match(haystack, needle),
                    MatchKind::Substring => matcher.substring_match(haystack, needle),
                    MatchKind::Suffix => matcher.postfix_match(haystack, needle),
                    MatchKind::Fuzzy => matcher.fuzzy_match(haystack, needle),
                };
                s.map(u32::from)
            };

            if let Some(score) = score {
                total_matches += 1;
                let cand = Scored { score, idx };
                if heap.len() < TOP_K {
                    heap.push(cand);
                } else if let Some(mut top) = heap.peek_mut() {
                    // Peek returns the candidate with the lowest score in the heap.
                    // If cand has higher score (or equal score with smaller idx):
                    if cand.score > top.score || (cand.score == top.score && cand.idx < top.idx) {
                        *top = cand;
                    }
                }
            }
        }
    }
    (heap, total_matches)
}

// ── Search Execution (Sequential / Parallel) ─────────────────────

fn search_sequential<'a>(
    arena: &'a Arena,
    indices: &'a [u32],
    kind: MatchKind,
    needle: Utf32Str<'_>,
    q_bytes: &[u8],
    ignore_case: bool,
    query_mask: u64,
) -> SearchResult<'a> {
    let (heap, total) = search_chunk(
        arena,
        indices,
        kind,
        needle,
        q_bytes,
        ignore_case,
        query_mask,
    );
    let mut items = heap.into_vec();
    items.sort_unstable_by(|a, b| b.score.cmp(&a.score).then_with(|| a.idx.cmp(&b.idx)));
    let result_indices = items.into_iter().map(|s| s.idx).collect();
    SearchResult {
        indices: Cow::Owned(result_indices),
        total,
    }
}

fn search_parallel<'a>(
    arena: &'a Arena,
    indices: &'a [u32],
    kind: MatchKind,
    needle: Utf32Str<'_>,
    q_bytes: &[u8],
    ignore_case: bool,
    query_mask: u64,
) -> SearchResult<'a> {
    use rayon::prelude::{ParallelIterator, ParallelSlice};

    let local_results: Vec<(BinaryHeap<Scored>, u32)> = indices
        .par_chunks(CHUNK_SIZE)
        .map(|chunk| search_chunk(arena, chunk, kind, needle, q_bytes, ignore_case, query_mask))
        .collect();

    let total_matches: u32 = local_results.iter().map(|(_, cnt)| *cnt).sum();
    let total_heap_items: usize = local_results.iter().map(|(h, _)| h.len()).sum();
    let mut all_items = Vec::with_capacity(total_heap_items);
    for (heap, _) in local_results {
        all_items.extend(heap.into_vec());
    }

    if all_items.len() > TOP_K {
        all_items.select_nth_unstable_by(TOP_K, |a, b| {
            b.score.cmp(&a.score).then_with(|| a.idx.cmp(&b.idx))
        });
        all_items.truncate(TOP_K);
    }
    all_items.sort_unstable_by(|a, b| b.score.cmp(&a.score).then_with(|| a.idx.cmp(&b.idx)));
    let result_indices = all_items.into_iter().map(|s| s.idx).collect();
    SearchResult {
        indices: Cow::Owned(result_indices),
        total: total_matches,
    }
}

// ── Public API ───────────────────────────────────────────────────

/// Execute search pipeline: bitmask prune → nucleo fuzzy → sorted matches.
///
/// Zero-copy: borrows `indices` slice from `Arena` for empty queries.
/// Returns owned/borrowed `SearchResult` (`Cow<'a, [u32]>` of indices).
#[must_use]
pub fn search<'a>(arena: &'a Arena, query: &str, scope: Scope) -> SearchResult<'a> {
    let indices = arena.scope_indices(scope);
    if indices.is_empty() {
        return SearchResult::empty();
    }
    let mode = parse_query(query);

    let Some(kind) = mode.match_kind() else {
        return SearchResult {
            indices: Cow::Borrowed(indices),
            total: indices.len() as u32,
        };
    };

    let raw_query_str = match mode {
        QueryMode::Prefix(s)
        | QueryMode::Substring(s)
        | QueryMode::Suffix(s)
        | QueryMode::Fuzzy(s) => s,
        QueryMode::Empty => unreachable!(),
    };

    let mut scratch = [0_u8; STACK_QUERY_CAPACITY];
    let mut overflow = None;
    let processed = ProcessedQuery::new(raw_query_str, &mut scratch, &mut overflow, true);

    let mut needle_buf = Vec::new();
    let needle = Utf32Str::new(processed.text, &mut needle_buf);
    let q_bytes = processed.text.as_bytes();

    #[cfg(not(miri))]
    if indices.len() < PAR_THRESHOLD {
        search_sequential(
            arena,
            indices,
            kind,
            needle,
            q_bytes,
            processed.ignore_case,
            processed.mask,
        )
    } else {
        search_parallel(
            arena,
            indices,
            kind,
            needle,
            q_bytes,
            processed.ignore_case,
            processed.mask,
        )
    }
    #[cfg(miri)]
    search_sequential(
        arena,
        indices,
        kind,
        needle,
        q_bytes,
        processed.ignore_case,
        processed.mask,
    )
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    fn write_tsv(lines: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.tsv");
        let mut f = std::fs::File::create(&path).expect("create");
        for line in lines {
            writeln!(f, "{line}").expect("write");
        }
        (dir, path)
    }

    fn make_arena(lines: &[&str]) -> (tempfile::TempDir, Arena) {
        let (dir, path) = write_tsv(lines);
        let arena = Arena::load(&path, Some("/home")).expect("load");
        (dir, arena)
    }

    #[test]
    fn empty_query_reverse_chrono() {
        let (_dir, arena) = make_arena(&[
            "1000\t50\t0\t/home\tls",
            "2000\t50\t0\t/home\tpwd",
            "3000\t50\t0\t/home\tgit status",
        ]);
        let result = search(&arena, "", Scope::Global);
        assert_eq!(result.indices.len(), 3);
        // Newest first
        assert_eq!(arena.cmd_str(result.indices[0]), "git status");
        assert_eq!(arena.cmd_str(result.indices[1]), "pwd");
        assert_eq!(arena.cmd_str(result.indices[2]), "ls");
    }

    #[test]
    fn prefix_match() {
        let (_dir, arena) = make_arena(&[
            "1000\t50\t0\t/home\tgit status",
            "2000\t50\t0\t/home\tgit pull",
            "3000\t50\t0\t/home\tls -la",
            "4000\t50\t0\t/home\tgrep foo",
        ]);
        let result = search(&arena, "^git", Scope::Global);
        assert_eq!(result.indices.len(), 2);
        for &idx in result.indices.iter() {
            assert!(
                arena.cmd_str(idx).starts_with("git"),
                "expected prefix match: {}",
                arena.cmd_str(idx)
            );
        }
    }

    #[test]
    fn substring_match() {
        let (_dir, arena) = make_arena(&[
            "1000\t50\t0\t/home\techo hello world",
            "2000\t50\t0\t/home\tls -la",
            "3000\t50\t0\t/home\tgrep hello file",
        ]);
        let result = search(&arena, "'hello", Scope::Global);
        assert_eq!(result.indices.len(), 2);
        for &idx in result.indices.iter() {
            assert!(
                arena.cmd_str(idx).contains("hello"),
                "expected substring match: {}",
                arena.cmd_str(idx)
            );
        }
    }

    #[test]
    fn suffix_match() {
        let (_dir, arena) = make_arena(&[
            "1000\t50\t0\t/home\tls -la",
            "2000\t50\t0\t/home\tgit status",
            "3000\t50\t0\t/home\techo -la",
        ]);
        let result = search(&arena, "-la$", Scope::Global);
        assert_eq!(result.indices.len(), 2);
        for &idx in result.indices.iter() {
            assert!(
                arena.cmd_str(idx).ends_with("-la"),
                "expected suffix match: {}",
                arena.cmd_str(idx)
            );
        }
    }

    #[test]
    fn fuzzy_ranking_exact_higher() {
        let (_dir, arena) = make_arena(&[
            "1000\t50\t0\t/home\tgit",
            "2000\t50\t0\t/home\tgit status --porcelain",
            "3000\t50\t0\t/home\tgit commit -m fix",
        ]);
        let result = search(&arena, "git", Scope::Global);
        assert!(!result.indices.is_empty());
        // Exact match "git" should score highest
        let top_cmd = arena.cmd_str(result.indices[0]);
        assert_eq!(
            top_cmd, "git",
            "exact match should rank first, got: {top_cmd}"
        );
    }

    #[test]
    fn bitmask_no_false_negatives() {
        let (_dir, arena) = make_arena(&[
            "1000\t50\t0\t/home\tabc",
            "2000\t50\t0\t/home\tdef",
            "3000\t50\t0\t/home\tabcdef",
        ]);
        // "abc" search: should find "abc" and "abcdef" but not "def"
        let result = search(&arena, "abc", Scope::Global);
        assert!(result.indices.len() >= 2);
        let cmds: Vec<&str> = result.indices.iter().map(|&i| arena.cmd_str(i)).collect();
        assert!(cmds.contains(&"abc"));
        assert!(cmds.contains(&"abcdef"));
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn search_unbounded_real_matches() {
        let mut lines = Vec::new();
        let owned: Vec<String> = (0..2500)
            .map(|i| format!("{}\t50\t0\t/home\tcmd_{i:04}", 1000 + i))
            .collect();
        for line in &owned {
            lines.push(line.as_str());
        }
        let (_dir, arena) = make_arena(&lines);
        let result = search(&arena, "cmd", Scope::Global);
        assert_eq!(result.indices.len(), 1000);
        assert_eq!(result.total, 2500);

        let empty_res = search(&arena, "", Scope::Global);
        assert_eq!(empty_res.indices.len(), 2500);
        assert_eq!(empty_res.total, 2500);
    }

    #[test]
    fn scope_filtering() {
        let (_dir, arena) = make_arena(&[
            "1000\t50\t0\t/home\tls",
            "2000\t50\t0\t/tmp\tpwd",
            "3000\t50\t0\t/home\tgit status",
        ]);
        let global = search(&arena, "", Scope::Global);
        let local = search(&arena, "", Scope::Local);
        assert_eq!(global.indices.len(), 3);
        assert_eq!(local.indices.len(), 2);
    }

    #[test]
    fn empty_arena() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("empty.tsv");
        std::fs::File::create(&path).expect("create");
        let arena = Arena::load(&path, None).expect("load");
        let result = search(&arena, "foo", Scope::Global);
        assert!(result.indices.is_empty());
        assert_eq!(result.total, 0);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn parallel_vs_sequential_same_results() {
        // Build enough entries to trigger parallel path
        let mut lines = Vec::new();
        let owned: Vec<String> = (0..600)
            .map(|i| format!("{}\t50\t0\t/home\tcommand_{i:04}_test", 1000 + i))
            .collect();
        for line in &owned {
            lines.push(line.as_str());
        }
        let (_dir, arena) = make_arena(&lines);

        let mut scratch = [0_u8; STACK_QUERY_CAPACITY];
        let mut overflow = None;
        let processed = ProcessedQuery::new("test", &mut scratch, &mut overflow, true);
        let mut needle_buf = Vec::new();
        let needle = Utf32Str::new(processed.text, &mut needle_buf);
        let q_bytes = processed.text.as_bytes();

        let seq = search_sequential(
            &arena,
            arena.scope_indices(Scope::Global),
            MatchKind::Fuzzy,
            needle,
            q_bytes,
            processed.ignore_case,
            processed.mask,
        );
        let par = search_parallel(
            &arena,
            arena.scope_indices(Scope::Global),
            MatchKind::Fuzzy,
            needle,
            q_bytes,
            processed.ignore_case,
            processed.mask,
        );

        assert_eq!(seq.indices.len(), par.indices.len());
        // Same set of indices (order may differ slightly for equal scores)
        let mut seq_sorted = seq.indices.into_owned();
        let mut par_sorted = par.indices.into_owned();
        seq_sorted.sort_unstable();
        par_sorted.sort_unstable();
        assert_eq!(seq_sorted, par_sorted);
    }

    #[test]
    fn uppercase_query_no_panic_regression() {
        // Regression test for nucleo-matcher fuzzy_optimal.rs:37 panic: "should have been caught by prefilter"
        let (_dir, arena) = make_arena(&[
            "1000\t50\t0\t/home\tA_B_C",
            "2000\t50\t0\t/home\tGIT_COMMIT_HOOK",
            "3000\t50\t0\t/home\tcargo build --release",
            "4000\t50\t0\t/home\tCARGO_HOME=/tmp",
        ]);

        // Exact pattern that triggered the upstream panic
        let res1 = search(&arena, "AB", Scope::Global);
        assert!(!res1.indices.is_empty());
        assert_eq!(arena.cmd_str(res1.indices[0]), "A_B_C");

        let res2 = search(&arena, "GIT", Scope::Global);
        assert!(!res2.indices.is_empty());
        assert_eq!(arena.cmd_str(res2.indices[0]), "GIT_COMMIT_HOOK");

        let res3 = search(&arena, "CARGO", Scope::Global);
        assert!(!res3.indices.is_empty());
        assert_eq!(arena.cmd_str(res3.indices[0]), "CARGO_HOME=/tmp");
    }

    #[test]
    fn case_insensitive_char_mask_and_search() {
        let (_dir, arena) = make_arena(&[
            "1000\t50\t0\t/home\tGit Status",
            "2000\t50\t0\t/home\tls -la",
        ]);

        // Lowercase query matches uppercase candidate via case-folded char_mask and nucleo
        let res = search(&arena, "git", Scope::Global);
        assert_eq!(res.indices.len(), 1);
        assert_eq!(arena.cmd_str(res.indices[0]), "Git Status");
    }

    #[test]
    fn query_parse_modes() {
        assert!(matches!(parse_query(""), QueryMode::Empty));
        assert!(matches!(parse_query("  "), QueryMode::Empty));
        assert!(matches!(parse_query("^foo"), QueryMode::Prefix("foo")));
        assert!(matches!(parse_query("'bar"), QueryMode::Substring("bar")));
        assert!(matches!(parse_query("baz$"), QueryMode::Suffix("baz")));
        assert!(matches!(parse_query("qux"), QueryMode::Fuzzy("qux")));
        assert!(matches!(parse_query("^"), QueryMode::Empty));
        assert!(matches!(parse_query("'"), QueryMode::Empty));
        assert!(matches!(parse_query("$"), QueryMode::Empty));
    }
}
