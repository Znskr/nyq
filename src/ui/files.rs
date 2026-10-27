//! The `@` file picker: workspace files (ignore files honoured) ranked by a
//! fuzzy match on the text typed after `@`.

use std::path::Path;

/// Stop indexing after this many files, so a huge tree stays responsive.
const MAX_FILES: usize = 20_000;

/// Files under `root`, relative and `/`-separated, sorted.
pub fn index(root: &Path) -> Vec<String> {
    let mut files: Vec<String> = ignore::WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .filter_map(|e| {
            let rel = e.path().strip_prefix(root).ok()?;
            Some(rel.to_string_lossy().replace('\\', "/"))
        })
        .take(MAX_FILES)
        .collect();
    files.sort();
    files
}

/// How well `query` matches `path` as an in-order subsequence (ignoring
/// case): runs of adjacent characters, matches at word starts and matches
/// in the file name count more. `None` when it doesn't match.
fn score(path: &str, query: &str) -> Option<i64> {
    let name_start = path.rfind('/').map_or(0, |i| i + 1);
    let mut want = query.chars().flat_map(char::to_lowercase).peekable();
    let (mut score, mut prev_hit, mut prev) = (0i64, false, '/');
    for (i, c) in path.char_indices() {
        let Some(&q) = want.peek() else { break };
        let hit = c.to_lowercase().eq(std::iter::once(q));
        if hit {
            want.next();
            score += 1
                + if prev_hit { 5 } else { 0 }
                + if matches!(prev, '/' | '_' | '-' | '.' | ' ') {
                    3
                } else {
                    0
                }
                + if i >= name_start { 2 } else { 0 };
        }
        prev_hit = hit;
        prev = c;
    }
    want.peek().is_none().then_some(score)
}

/// Up to `limit` files matching `query`, best first (shorter paths win ties).
pub fn matches(files: &[String], query: &str, limit: usize) -> Vec<String> {
    let mut hits: Vec<(i64, &String)> = files
        .iter()
        .filter_map(|f| score(f, query).map(|s| (s, f)))
        .collect();
    hits.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(a.1.len().cmp(&b.1.len()))
            .then(a.1.cmp(b.1))
    });
    hits.into_iter()
        .take(limit)
        .map(|(_, f)| f.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexes_and_ranks_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for f in [
            "src/main.rs",
            "src/ui/editor.rs",
            "README.md",
            ".git/config",
            "target/x.rs",
            ".gitignore",
        ] {
            std::fs::create_dir_all(root.join(f).parent().unwrap()).unwrap();
            std::fs::write(root.join(f), "").unwrap();
        }
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        // Ignore files apply inside a git repository.
        std::fs::create_dir_all(root.join(".git/info")).unwrap();
        let files = index(root);
        assert_eq!(
            files,
            [".gitignore", "README.md", "src/main.rs", "src/ui/editor.rs"]
        );
        assert_eq!(matches(&files, "edit", 8), ["src/ui/editor.rs"]);
        assert_eq!(matches(&files, "MAIN", 8), ["src/main.rs"]);
        assert_eq!(matches(&files, "sr", 8)[0], "src/main.rs");
        assert_eq!(matches(&files, "", 2).len(), 2);
        assert!(matches(&files, "zzz", 8).is_empty());
        // Adjacent letters in the file name beat letters scattered over the path.
        let files = vec!["src/models/rs.txt".to_string(), "src/mod.rs".to_string()];
        assert_eq!(matches(&files, "mod.rs", 8)[0], "src/mod.rs");
    }
}
