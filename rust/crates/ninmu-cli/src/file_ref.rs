//! @-style file reference expansion.
//!
//! Users can reference files in their prompt using `@path/to/file`.
//! Before sending to the model, these references are expanded inline
//! with the file contents wrapped in a clear delimiter.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// A parsed `@` reference found in user input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRef {
    /// The `@` prefix position in the original input (byte offset).
    pub at_offset: usize,
    /// The file path portion (after `@`).
    pub path: String,
    /// The full `@path` span length in the original input.
    pub span_len: usize,
}

/// Result of expanding all `@` references in an input string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandedInput {
    /// The expanded input with file contents inlined.
    pub expanded: String,
    /// Paths that were successfully expanded.
    pub resolved: Vec<String>,
    /// Paths that could not be resolved (with error reason).
    pub failed: Vec<(String, String)>,
}

/// Parse all `@path` references from input text.
///
/// An `@` reference starts with `@` and continues until whitespace or end of string.
/// The path must be non-empty after `@`.
/// Escaped `\@` is not treated as a reference.
pub fn parse_file_refs(input: &str) -> Vec<FileRef> {
    let mut refs = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '@' {
            // Check it's not preceded by a backslash (escaped)
            if i > 0 && chars[i - 1] == '\\' {
                i += 1;
                continue;
            }

            // Collect path after @
            let start = i;
            let mut path_end = i + 1;
            while path_end < chars.len() && !chars[path_end].is_whitespace() {
                path_end += 1;
            }

            let path: String = chars[i + 1..path_end].iter().collect();
            if !path.is_empty() {
                let span: String = chars[start..path_end].iter().collect();
                refs.push(FileRef {
                    at_offset: start,
                    path: path.clone(),
                    span_len: span.len(),
                });
            }
            i = path_end;
        } else {
            i += 1;
        }
    }

    refs
}

/// Expand all `@path` references in input text, replacing them with
/// the file contents wrapped in a delimiter block.
///
/// Returns the expanded input and lists of resolved/failed paths.
pub fn expand_file_refs(input: &str) -> ExpandedInput {
    let refs = parse_file_refs(input);

    if refs.is_empty() {
        return ExpandedInput {
            expanded: input.to_string(),
            resolved: Vec::new(),
            failed: Vec::new(),
        };
    }

    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut resolved = Vec::new();
    let mut failed = Vec::new();
    let mut result = input.to_string();

    // Process in reverse order so byte offsets remain valid
    for file_ref in refs.into_iter().rev() {
        let file_path = cwd.join(&file_ref.path);

        match fs::read_to_string(&file_path) {
            Ok(contents) => {
                let display_path = file_ref.path.clone();
                let replacement = format!("<file path=\"{display_path}\">\n{contents}\n</file>");
                // Find the @path span in the current result string
                let at_marker = format!("@{}", file_ref.path);
                if let Some(pos) = result.rfind(&at_marker) {
                    result.replace_range(pos..pos + at_marker.len(), &replacement);
                }
                resolved.push(display_path);
            }
            Err(err) => {
                failed.push((file_ref.path.clone(), err.to_string()));
                // Leave the @reference as-is in the output so the model can see it
            }
        }
    }

    ExpandedInput {
        expanded: result,
        resolved,
        failed,
    }
}

/// Find file paths matching a partial prefix for tab completion.
///
/// `partial` is the text after `@` (e.g., "src/ma" for `@src/ma`).
/// Returns a list of matching file/directory paths relative to CWD.
///
/// Rules:
/// - Only matches regular files (not directories, but directories get a `/` suffix)
/// - Respects .gitignore if possible (for now, just skip hidden files/dirs starting with `.`)
/// - Limits results to 50 matches
/// - Matches by prefix on the filename (not full path) if no `/` in partial,
///   or by path prefix if `/` is present
pub fn complete_file_ref(partial: &str) -> Vec<String> {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let limit = 50;

    if partial.is_empty() {
        // List top-level files/dirs
        return list_dir_entries(&cwd, "", limit);
    }

    // Check if partial contains a directory separator
    if let Some(last_sep) = partial.rfind('/') {
        let dir_part = &partial[..last_sep];
        let file_prefix = &partial[last_sep + 1..];
        let search_dir = cwd.join(dir_part);

        if search_dir.is_dir() {
            list_dir_entries_matching(&search_dir, dir_part, file_prefix, limit)
        } else {
            Vec::new()
        }
    } else {
        // No directory separator — match files in CWD by filename prefix
        list_dir_entries_matching(&cwd, "", partial, limit)
    }
}

fn list_dir_entries(dir: &Path, prefix: &str, limit: usize) -> Vec<String> {
    let mut entries = Vec::new();
    let Ok(rd) = fs::read_dir(dir) else {
        return entries;
    };

    for entry in rd.flatten() {
        if entries.len() >= limit {
            break;
        }
        let file_name = entry.file_name().to_string_lossy().to_string();
        // Skip hidden files
        if file_name.starts_with('.') {
            continue;
        }
        let path = if prefix.is_empty() {
            file_name.clone()
        } else {
            format!("{}/{}", prefix, file_name)
        };

        // Add trailing / for directories
        if entry.file_type().map_or(false, |t| t.is_dir()) {
            entries.push(format!("{}/", path));
        } else {
            entries.push(path);
        }
    }

    entries.sort();
    entries
}

fn list_dir_entries_matching(
    dir: &Path,
    prefix: &str,
    file_prefix: &str,
    limit: usize,
) -> Vec<String> {
    let mut entries = Vec::new();
    let Ok(rd) = fs::read_dir(dir) else {
        return entries;
    };

    let lower_prefix = file_prefix.to_lowercase();

    for entry in rd.flatten() {
        if entries.len() >= limit {
            break;
        }
        let file_name = entry.file_name().to_string_lossy().to_string();
        // Skip hidden files
        if file_name.starts_with('.') {
            continue;
        }
        // Case-insensitive prefix match on filename
        if !file_name.to_lowercase().starts_with(&lower_prefix) {
            continue;
        }

        let path = if prefix.is_empty() {
            file_name.clone()
        } else {
            format!("{}/{}", prefix, file_name)
        };

        // Add trailing / for directories
        if entry.file_type().map_or(false, |t| t.is_dir()) {
            entries.push(format!("{}/", path));
        } else {
            entries.push(path);
        }
    }

    entries.sort();
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;

    /// Mutex to serialize CWD-dependent tests (parallel tests share process CWD).
    static CWD_LOCK: Mutex<()> = Mutex::new(());

    /// Helper to run a test closure with exclusive CWD access.
    fn with_cwd_lock<F>(f: F)
    where
        F: FnOnce(),
    {
        let _guard = CWD_LOCK.lock().unwrap();
        f();
    }

    #[test]
    fn parse_empty_input() {
        assert!(parse_file_refs("").is_empty());
    }

    #[test]
    fn parse_no_refs() {
        assert!(parse_file_refs("hello world").is_empty());
    }

    #[test]
    fn parse_single_ref() {
        let refs = parse_file_refs("read @src/main.rs please");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].path, "src/main.rs");
    }

    #[test]
    fn parse_multiple_refs() {
        let refs = parse_file_refs("@foo.rs and @bar.txt");
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].path, "foo.rs");
        assert_eq!(refs[1].path, "bar.txt");
    }

    #[test]
    fn parse_ref_at_end() {
        let refs = parse_file_refs("look at @README.md");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].path, "README.md");
    }

    #[test]
    fn parse_ref_at_start() {
        let refs = parse_file_refs("@Cargo.toml what is this");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].path, "Cargo.toml");
    }

    #[test]
    fn parse_only_at_symbol() {
        // Lone @ with no path should not produce a ref
        let refs = parse_file_refs("hello @ world");
        assert!(refs.is_empty());
    }

    #[test]
    fn parse_escaped_at() {
        let refs = parse_file_refs("email is \\@foo and @real.txt");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].path, "real.txt");
    }

    #[test]
    fn expand_no_refs() {
        let result = expand_file_refs("just a normal prompt");
        assert_eq!(result.expanded, "just a normal prompt");
        assert!(result.resolved.is_empty());
        assert!(result.failed.is_empty());
    }

    #[test]
    fn expand_existing_file() {
        with_cwd_lock(|| {
            let dir = std::env::temp_dir().join("ninmu-file-ref-test-expand");
            let _ = fs::create_dir_all(&dir);
            fs::write(dir.join("hello.txt"), "hello world").expect("write test file");

            let orig_cwd = env::current_dir();
            env::set_current_dir(&dir).expect("cd to temp dir");

            let input = "please read @hello.txt and summarize";
            let result = expand_file_refs(input);

            assert!(result.expanded.contains("<file path=\"hello.txt\">"));
            assert!(result.expanded.contains("hello world"));
            assert!(result.expanded.contains("</file>"));
            assert_eq!(result.resolved, vec!["hello.txt"]);
            assert!(result.failed.is_empty());

            if let Ok(cwd) = orig_cwd {
                let _ = env::set_current_dir(cwd);
            }
            let _ = fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn expand_missing_file() {
        let result = expand_file_refs("read @nonexistent_file_xyz.txt");
        assert!(result.failed.len() == 1);
        assert_eq!(result.failed[0].0, "nonexistent_file_xyz.txt");
        // The @reference should remain in the output
        assert!(result.expanded.contains("@nonexistent_file_xyz.txt"));
    }

    #[test]
    fn expand_multiple_refs() {
        with_cwd_lock(|| {
            let dir = std::env::temp_dir().join("ninmu-file-ref-test-multi");
            let _ = fs::create_dir_all(&dir);
            fs::write(dir.join("a.txt"), "content A").expect("write a");
            fs::write(dir.join("b.txt"), "content B").expect("write b");

            let orig_cwd = env::current_dir();
            env::set_current_dir(&dir).expect("cd to temp dir");

            let input = "compare @a.txt and @b.txt";
            let result = expand_file_refs(input);

            assert_eq!(result.resolved.len(), 2);
            assert!(result.expanded.contains("content A"));
            assert!(result.expanded.contains("content B"));

            if let Ok(cwd) = orig_cwd {
                let _ = env::set_current_dir(cwd);
            }
            let _ = fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn complete_empty_partial() {
        with_cwd_lock(|| {
            let dir = std::env::temp_dir().join("ninmu-file-ref-test-complete");
            let _ = fs::create_dir_all(&dir);
            fs::write(dir.join("alpha.rs"), "").expect("write");
            fs::write(dir.join("beta.txt"), "").expect("write");
            let _ = fs::create_dir_all(dir.join("src"));

            let orig_cwd = env::current_dir();
            env::set_current_dir(&dir).expect("cd to temp dir");

            let results = complete_file_ref("");
            assert!(results.contains(&"alpha.rs".to_string()));
            assert!(results.contains(&"beta.txt".to_string()));
            assert!(results.contains(&"src/".to_string()));
            // Should not contain hidden files
            assert!(!results.iter().any(|r| r.starts_with('.')));

            if let Ok(cwd) = orig_cwd {
                let _ = env::set_current_dir(cwd);
            }
            let _ = fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn complete_partial_filename() {
        with_cwd_lock(|| {
            let dir = std::env::temp_dir().join("ninmu-file-ref-test-partial");
            let _ = fs::create_dir_all(&dir);
            fs::write(dir.join("main.rs"), "").expect("write");
            fs::write(dir.join("model.rs"), "").expect("write");
            fs::write(dir.join("config.toml"), "").expect("write");

            let orig_cwd = env::current_dir();
            env::set_current_dir(&dir).expect("cd to temp dir");

            let results = complete_file_ref("ma");
            assert!(results.contains(&"main.rs".to_string()));
            assert!(!results.contains(&"config.toml".to_string()));

            if let Ok(cwd) = orig_cwd {
                let _ = env::set_current_dir(cwd);
            }
            let _ = fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn complete_with_directory_prefix() {
        with_cwd_lock(|| {
            let dir = std::env::temp_dir().join("ninmu-file-ref-test-dir");
            let _ = fs::create_dir_all(dir.join("src"));
            fs::write(dir.join("src/main.rs"), "").expect("write");
            fs::write(dir.join("src/model.rs"), "").expect("write");

            let orig_cwd = env::current_dir();
            env::set_current_dir(&dir).expect("cd to temp dir");

            let results = complete_file_ref("src/ma");
            assert!(results.contains(&"src/main.rs".to_string()));
            assert!(!results.contains(&"src/model.rs".to_string()));

            if let Ok(cwd) = orig_cwd {
                let _ = env::set_current_dir(cwd);
            }
            let _ = fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn complete_case_insensitive() {
        with_cwd_lock(|| {
            let dir = std::env::temp_dir().join("ninmu-file-ref-test-case");
            let _ = fs::create_dir_all(&dir);
            fs::write(dir.join("README.md"), "").expect("write");

            let orig_cwd = env::current_dir();
            env::set_current_dir(&dir).expect("cd to temp dir");

            let results = complete_file_ref("read");
            assert!(results.contains(&"README.md".to_string()));

            if let Ok(cwd) = orig_cwd {
                let _ = env::set_current_dir(cwd);
            }
            let _ = fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn complete_respects_limit() {
        with_cwd_lock(|| {
            let dir = std::env::temp_dir().join("ninmu-file-ref-test-limit");
            let _ = fs::create_dir_all(&dir);
            for i in 0..60 {
                fs::write(dir.join(format!("file_{:03}.txt", i)), "").expect("write");
            }

            let orig_cwd = env::current_dir();
            env::set_current_dir(&dir).expect("cd to temp dir");

            let results = complete_file_ref("");
            assert!(results.len() <= 50);

            if let Ok(cwd) = orig_cwd {
                let _ = env::set_current_dir(cwd);
            }
            let _ = fs::remove_dir_all(&dir);
        });
    }
}
