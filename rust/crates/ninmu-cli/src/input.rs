use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::io::{self, IsTerminal, Write};

use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::Validator;
use rustyline::{
    Cmd, CompletionType, Config, Context, EditMode, Editor, Helper, KeyCode, KeyEvent, Modifiers,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOutcome {
    Submit(String),
    Cancel,
    Exit,
}

/// External data provider for argument completion.
/// Set on the `LineEditor` at construction time.
#[derive(Default)]
pub struct CompletionProvider {
    /// Function that returns available model names for completion.
    pub model_names: Vec<String>,
    /// Function that returns available session IDs for completion.
    pub session_ids: Vec<String>,
}

struct SlashCommandHelper {
    completions: Vec<String>,
    current_line: RefCell<String>,
    provider: CompletionProvider,
}

impl SlashCommandHelper {
    fn new(completions: Vec<String>, provider: CompletionProvider) -> Self {
        Self {
            completions: normalize_completions(completions),
            current_line: RefCell::new(String::new()),
            provider,
        }
    }

    fn reset_current_line(&self) {
        self.current_line.borrow_mut().clear();
    }

    fn current_line(&self) -> String {
        self.current_line.borrow().clone()
    }

    fn set_current_line(&self, line: &str) {
        let mut current = self.current_line.borrow_mut();
        current.clear();
        current.push_str(line);
    }

    fn set_completions(&mut self, completions: Vec<String>) {
        self.completions = normalize_completions(completions);
    }

    fn set_provider(&mut self, provider: CompletionProvider) {
        self.provider = provider;
    }

    /// Complete file paths for commands that accept file arguments.
    fn complete_file_arg(&self, prefix: &str) -> Vec<Pair> {
        let file_matches = crate::file_ref::complete_file_ref(prefix);
        file_matches
            .into_iter()
            .map(|path| Pair {
                display: path.clone(),
                replacement: path,
            })
            .collect()
    }

    /// Complete model names.
    fn complete_model_arg(&self, prefix: &str) -> Vec<Pair> {
        // Built-in aliases
        let mut candidates: Vec<String> = vec![
            "opus".to_string(),
            "sonnet".to_string(),
            "haiku".to_string(),
            "claude-opus-4-6".to_string(),
            "claude-sonnet-4-6".to_string(),
            "claude-haiku-4-5-20251213".to_string(),
        ];
        // From provider
        candidates.extend(self.provider.model_names.clone());

        candidates
            .into_iter()
            .filter(|c| c.starts_with(prefix))
            .map(|candidate| Pair {
                display: candidate.clone(),
                replacement: candidate,
            })
            .collect()
    }

    /// Complete session IDs.
    fn complete_session_arg(&self, prefix: &str) -> Vec<Pair> {
        self.provider
            .session_ids
            .iter()
            .filter(|id| id.starts_with(prefix))
            .map(|id| Pair {
                display: id.clone(),
                replacement: id.clone(),
            })
            .collect()
    }

    /// Detect argument command from the line and return a (`completer_fn`, `arg_prefix`) or None.
    fn try_argument_completion(&self, line: &str, pos: usize) -> Option<Vec<Pair>> {
        if pos != line.len() || !line.starts_with('/') {
            return None;
        }

        let parts: Vec<&str> = line.splitn(3, ' ').collect();
        match parts.len() {
            2 => {
                // "command " with trailing space or "command partial"
                let (cmd, arg) = (parts[0], parts[1]);
                match cmd {
                    "/export" | "/memory" => {
                        let prefix = if line.ends_with(' ') { "" } else { arg };
                        Some(self.complete_file_arg(prefix))
                    }
                    "/model" | "/permissions" => {
                        // permissions only supports read-only, workspace-write, danger-full-access
                        if cmd == "/model" {
                            let prefix = if line.ends_with(' ') { "" } else { arg };
                            Some(self.complete_model_arg(prefix))
                        } else {
                            // /permissions has fixed values, already in completions list
                            None
                        }
                    }
                    "/session" | "/resume" => {
                        // /resume <session-id>
                        // /session switch <session-id>
                        let prefix = if line.ends_with(' ') { "" } else { arg };
                        Some(self.complete_session_arg(prefix))
                    }
                    _ => None,
                }
            }
            3 => {
                // "command subcommand argument"
                let (cmd, sub, arg) = (parts[0], parts[1], parts[2]);
                match (cmd, sub) {
                    ("/session", "switch") => {
                        let prefix = if line.ends_with(' ') { "" } else { arg };
                        Some(self.complete_session_arg(prefix))
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

impl Completer for SlashCommandHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Self::Candidate>)> {
        // Try @-file completion first
        if let Some(file_prefix) = at_file_prefix(line, pos) {
            let file_matches = crate::file_ref::complete_file_ref(&file_prefix);
            if !file_matches.is_empty() {
                // start = byte offset right after the @ character.
                // rustyline replaces line[start..pos] with Pair.replacement.
                // For "@src" (pos=4, prefix="src"): start=1, replace "src" with "src/main.rs"
                // For "hello @s" (pos=8, prefix="s"): start=7, replace "s" with "rc/main.rs"
                let start = pos.saturating_sub(file_prefix.len());
                let matches: Vec<Pair> = file_matches
                    .into_iter()
                    .map(|path| Pair {
                        display: path.clone(),
                        replacement: path,
                    })
                    .collect();
                return Ok((start, matches));
            }
        }

        // Try argument-aware completion for known slash commands
        if let Some(arg_matches) = self.try_argument_completion(line, pos) {
            if !arg_matches.is_empty() {
                // Determine the start position for replacement
                let last_space = line[0..pos].rfind(' ').map_or(0, |i| i + 1);
                return Ok((last_space, arg_matches));
            }
        }

        // Fall back to slash command completion
        let Some(prefix) = slash_command_prefix(line, pos) else {
            return Ok((0, Vec::new()));
        };

        let matches = self
            .completions
            .iter()
            .filter(|candidate| candidate.starts_with(prefix))
            .map(|candidate| Pair {
                display: candidate.clone(),
                replacement: candidate.clone(),
            })
            .collect();

        Ok((0, matches))
    }
}

impl Hinter for SlashCommandHelper {
    type Hint = String;

    fn hint(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Option<String> {
        // Show inline hint when typing after @
        if let Some(file_prefix) = at_file_prefix(line, pos) {
            let matches = crate::file_ref::complete_file_ref(&file_prefix);
            if let Some(first) = matches.first() {
                // Show remaining characters of first match as dim hint
                let suffix = &first[file_prefix.len()..];
                if !suffix.is_empty() {
                    return Some(format!("\x1b[2m{suffix}\x1b[0m"));
                }
            }
        }
        None
    }
}

impl Highlighter for SlashCommandHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        self.set_current_line(line);
        Cow::Borrowed(line)
    }

    fn highlight_char(&self, line: &str, _pos: usize, _kind: CmdKind) -> bool {
        self.set_current_line(line);
        false
    }
}

impl Validator for SlashCommandHelper {}
impl Helper for SlashCommandHelper {}

pub struct LineEditor {
    prompt: String,
    editor: Editor<SlashCommandHelper, DefaultHistory>,
}

impl LineEditor {
    #[must_use]
    pub fn new(
        prompt: impl Into<String>,
        completions: Vec<String>,
        provider: CompletionProvider,
    ) -> Self {
        let config = Config::builder()
            .completion_type(CompletionType::Circular)
            .edit_mode(EditMode::Emacs)
            .build();
        let mut editor = Editor::<SlashCommandHelper, DefaultHistory>::with_config(config)
            .expect("rustyline editor should initialize");
        editor.set_helper(Some(SlashCommandHelper::new(completions, provider)));
        editor.bind_sequence(KeyEvent(KeyCode::Char('J'), Modifiers::CTRL), Cmd::Newline);
        editor.bind_sequence(KeyEvent(KeyCode::Enter, Modifiers::SHIFT), Cmd::Newline);

        Self {
            prompt: prompt.into(),
            editor,
        }
    }

    pub fn push_history(&mut self, entry: impl Into<String>) {
        let entry = entry.into();
        if entry.trim().is_empty() {
            return;
        }

        let _ = self.editor.add_history_entry(entry);
    }

    pub fn set_completions(&mut self, completions: Vec<String>) {
        if let Some(helper) = self.editor.helper_mut() {
            helper.set_completions(completions);
        }
    }

    pub fn set_provider(&mut self, provider: CompletionProvider) {
        if let Some(helper) = self.editor.helper_mut() {
            helper.set_provider(provider);
        }
    }

    pub fn read_line(&mut self) -> io::Result<ReadOutcome> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return self.read_line_fallback();
        }

        if let Some(helper) = self.editor.helper_mut() {
            helper.reset_current_line();
        }

        match self.editor.readline(&self.prompt) {
            Ok(line) => Ok(ReadOutcome::Submit(line)),
            Err(ReadlineError::Interrupted) => {
                let has_input = !self.current_line().is_empty();
                self.finish_interrupted_read()?;
                if has_input {
                    Ok(ReadOutcome::Cancel)
                } else {
                    Ok(ReadOutcome::Exit)
                }
            }
            Err(ReadlineError::Eof) => {
                self.finish_interrupted_read()?;
                Ok(ReadOutcome::Exit)
            }
            Err(error) => Err(io::Error::other(error)),
        }
    }

    fn current_line(&self) -> String {
        self.editor
            .helper()
            .map_or_else(String::new, SlashCommandHelper::current_line)
    }

    fn finish_interrupted_read(&mut self) -> io::Result<()> {
        if let Some(helper) = self.editor.helper_mut() {
            helper.reset_current_line();
        }
        let mut stdout = io::stdout();
        writeln!(stdout)
    }

    fn read_line_fallback(&self) -> io::Result<ReadOutcome> {
        let mut stdout = io::stdout();
        write!(stdout, "{}", self.prompt)?;
        stdout.flush()?;

        let mut buffer = String::new();
        let bytes_read = io::stdin().read_line(&mut buffer)?;
        if bytes_read == 0 {
            return Ok(ReadOutcome::Exit);
        }

        while matches!(buffer.chars().last(), Some('\n' | '\r')) {
            buffer.pop();
        }
        Ok(ReadOutcome::Submit(buffer))
    }
}

fn slash_command_prefix(line: &str, pos: usize) -> Option<&str> {
    if pos != line.len() {
        return None;
    }

    let prefix = &line[..pos];
    if !prefix.starts_with('/') {
        return None;
    }

    Some(prefix)
}

/// Extract the file path portion after `@` at the end of the line for completion.
/// Returns `Some(path_portion)` if the cursor is at the end of an `@path` fragment.
fn at_file_prefix(line: &str, pos: usize) -> Option<String> {
    if pos != line.len() {
        return None;
    }

    let before_cursor = &line[..pos];

    // Find the last `@` in the line
    let at_pos = before_cursor.rfind('@')?;

    // The @ must be preceded by whitespace or be at start of line
    if at_pos > 0 {
        let before_at = &before_cursor[..at_pos];
        if !before_at.is_empty() {
            let last_char = before_at.chars().last()?;
            if !last_char.is_whitespace() {
                return None;
            }
        }
    }

    let path_portion = &before_cursor[at_pos + 1..];
    Some(path_portion.to_string())
}

fn normalize_completions(completions: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    completions
        .into_iter()
        .filter(|candidate| candidate.starts_with('/'))
        .filter(|candidate| seen.insert(candidate.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        at_file_prefix, slash_command_prefix, CompletionProvider, LineEditor, SlashCommandHelper,
    };
    use rustyline::completion::Completer;
    use rustyline::highlight::Highlighter;
    use rustyline::hint::Hinter;
    use rustyline::history::{DefaultHistory, History};
    use rustyline::Context;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn cwd_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_cwd_lock()
    }

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("ninmu-input-{label}-{nanos}"))
    }

    fn with_current_dir<T>(cwd: &Path, f: impl FnOnce() -> T) -> T {
        let previous =
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        std::env::set_current_dir(cwd).expect("cwd should change");
        let result = f();
        if std::env::set_current_dir(&previous).is_err() {
            std::env::set_current_dir(env!("CARGO_MANIFEST_DIR")).expect("cwd should restore");
        }
        result
    }

    fn file_completion_workspace(label: &str) -> PathBuf {
        let workspace = temp_dir(label);
        std::fs::create_dir_all(workspace.join("src")).expect("src should exist");
        std::fs::write(workspace.join("src").join("main.rs"), "fn main() {}\n")
            .expect("main fixture should write");
        std::fs::write(
            workspace.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\n",
        )
        .expect("cargo fixture should write");
        workspace
    }

    #[test]
    fn extracts_terminal_slash_command_prefixes_with_arguments() {
        assert_eq!(slash_command_prefix("/he", 3), Some("/he"));
        assert_eq!(slash_command_prefix("/help me", 8), Some("/help me"));
        assert_eq!(
            slash_command_prefix("/session switch ses", 19),
            Some("/session switch ses")
        );
        assert_eq!(slash_command_prefix("hello", 5), None);
        assert_eq!(slash_command_prefix("/help", 2), None);
    }

    #[test]
    fn completes_matching_slash_commands() {
        let helper = SlashCommandHelper::new(
            vec![
                "/help".to_string(),
                "/hello".to_string(),
                "/status".to_string(),
            ],
            CompletionProvider::default(),
        );
        let history = DefaultHistory::new();
        let ctx = Context::new(&history);
        let (start, matches) = helper
            .complete("/he", 3, &ctx)
            .expect("completion should work");

        assert_eq!(start, 0);
        assert_eq!(
            matches
                .into_iter()
                .map(|candidate| candidate.replacement)
                .collect::<Vec<_>>(),
            vec!["/help".to_string(), "/hello".to_string()]
        );
    }

    #[test]
    fn completes_matching_slash_command_arguments() {
        let helper = SlashCommandHelper::new(
            vec![
                "/model".to_string(),
                "/model opus".to_string(),
                "/model sonnet".to_string(),
                "/session switch alpha".to_string(),
            ],
            CompletionProvider::default(),
        );
        let history = DefaultHistory::new();
        let ctx = Context::new(&history);
        let (start, matches) = helper
            .complete("/model o", 8, &ctx)
            .expect("completion should work");

        // Argument-aware completion returns start = position after space (7)
        assert_eq!(start, 7);
        assert!(!matches.is_empty());
        for candidate in &matches {
            assert!(
                candidate.replacement.starts_with("opus")
                    || candidate.replacement.starts_with("sonnet")
            );
        }
    }

    #[test]
    fn ignores_non_slash_command_completion_requests() {
        let helper =
            SlashCommandHelper::new(vec!["/help".to_string()], CompletionProvider::default());
        let history = DefaultHistory::new();
        let ctx = Context::new(&history);
        let (_, matches) = helper
            .complete("hello", 5, &ctx)
            .expect("completion should work");

        assert!(matches.is_empty());
    }

    #[test]
    fn tracks_current_buffer_through_highlighter() {
        let helper = SlashCommandHelper::new(Vec::new(), CompletionProvider::default());
        let _ = helper.highlight("draft", 5);

        assert_eq!(helper.current_line(), "draft");
    }

    #[test]
    fn push_history_ignores_blank_entries() {
        let mut editor = LineEditor::new(
            "> ",
            vec!["/help".to_string()],
            CompletionProvider::default(),
        );
        editor.push_history("   ");
        editor.push_history("/help");

        assert_eq!(editor.editor.history().len(), 1);
    }

    #[test]
    fn set_completions_replaces_and_normalizes_candidates() {
        let mut editor = LineEditor::new(
            "> ",
            vec!["/help".to_string()],
            CompletionProvider::default(),
        );
        editor.set_completions(vec![
            "/model opus".to_string(),
            "/model opus".to_string(),
            "status".to_string(),
        ]);

        let helper = editor.editor.helper().expect("helper should exist");
        assert_eq!(helper.completions, vec!["/model opus".to_string()]);
    }

    #[test]
    fn detects_at_file_prefix() {
        assert_eq!(at_file_prefix("@src/main", 9), Some("src/main".to_string()));
    }

    #[test]
    fn detects_at_file_prefix_empty_path() {
        assert_eq!(at_file_prefix("@", 1), Some(String::new()));
    }

    #[test]
    fn rejects_at_not_at_end() {
        assert_eq!(at_file_prefix("@src/main more", 9), None);
    }

    #[test]
    fn rejects_at_preceded_by_non_whitespace() {
        assert_eq!(at_file_prefix("email@host.com", 15), None);
    }

    #[test]
    fn detects_at_after_text() {
        assert_eq!(at_file_prefix("read @src/", 10), Some("src/".to_string()));
    }

    #[test]
    fn detects_at_at_start_of_line() {
        assert_eq!(
            at_file_prefix("@Cargo.toml", 11),
            Some("Cargo.toml".to_string())
        );
    }

    // --- @-file completion integration tests ---

    #[test]
    fn completes_at_file_ref_in_helper() {
        let _guard = cwd_lock();
        let workspace = file_completion_workspace("helper");
        let helper = SlashCommandHelper::new(Vec::new(), CompletionProvider::default());
        let history = DefaultHistory::new();
        let ctx = Context::new(&history);
        let (start, matches) = with_current_dir(&workspace, || {
            helper
                .complete("@src", 4, &ctx)
                .expect("completion should work")
        });

        // start should be 1 (right after @), replacements should be just the path
        assert_eq!(start, 1);
        assert!(!matches.is_empty());
        for candidate in &matches {
            assert!(!candidate.replacement.starts_with('@'));
            assert!(candidate.replacement.starts_with("src/"));
        }
    }

    #[test]
    fn completes_at_file_ref_with_prefix() {
        let _guard = cwd_lock();
        let workspace = file_completion_workspace("prefix");
        let helper = SlashCommandHelper::new(Vec::new(), CompletionProvider::default());
        let history = DefaultHistory::new();
        let ctx = Context::new(&history);
        let (start, matches) = with_current_dir(&workspace, || {
            helper
                .complete("read @Cargo", 11, &ctx)
                .expect("completion should work")
        });

        // "read @Cargo" -> prefix="Cargo", start=6 (after @), pos=11
        assert_eq!(start, 6);
        assert!(!matches.is_empty());
        for candidate in &matches {
            assert!(candidate.replacement.starts_with("Cargo"));
        }
    }

    #[test]
    fn no_at_file_completions_for_unknown_prefix() {
        let helper = SlashCommandHelper::new(Vec::new(), CompletionProvider::default());
        let history = DefaultHistory::new();
        let ctx = Context::new(&history);
        let (_, matches) = helper
            .complete("@zzz_nonexistent_prefix_xyz_", 28, &ctx)
            .expect("completion should work");

        assert!(matches.is_empty());
    }

    #[test]
    fn at_file_hint_shows_first_match_suffix() {
        let _guard = cwd_lock();
        let workspace = file_completion_workspace("hint");
        let helper = SlashCommandHelper::new(Vec::new(), CompletionProvider::default());
        let history = DefaultHistory::new();
        let ctx = Context::new(&history);
        let hint = with_current_dir(&workspace, || helper.hint("@src", 4, &ctx));

        // Should hint with remaining path after "src"
        assert!(hint.is_some(), "hint should appear for @src prefix");
        let hint = hint.unwrap();
        assert!(!hint.is_empty(), "hint should be non-empty");
        // Hint should not include the prefix we already typed
        assert!(!hint.contains("@src"));
    }

    #[test]
    fn at_file_hint_none_for_unknown_prefix() {
        let helper = SlashCommandHelper::new(Vec::new(), CompletionProvider::default());
        let history = DefaultHistory::new();
        let ctx = Context::new(&history);
        let hint = helper.hint("@zzz_nonexistent_xyz", 21, &ctx);
        assert!(hint.is_none());
    }

    #[test]
    fn at_file_hint_none_when_cursor_not_at_end() {
        let helper = SlashCommandHelper::new(Vec::new(), CompletionProvider::default());
        let history = DefaultHistory::new();
        let ctx = Context::new(&history);
        let hint = helper.hint("@src/main.rs", 5, &ctx);
        assert!(hint.is_none());
    }
}
