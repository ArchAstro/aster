//! Narrowing a test command to the tests selected by
//! `aster affected --related`.
//!
//! Each function recognises its language's test runner in a plain command
//! (no shell operators, no interpreter wrapper) and returns the commands
//! that run only the selected tests. `None` means the command is not one
//! the function can narrow, so it runs as written. A command that already
//! lists test files keeps the ones that are selected.

use super::{FilesListPlan, RelatedTest};
use crate::executor::quote_command_argument as quote_any;
use std::collections::BTreeSet;
use std::path::Path;

const SHELL_OPERATORS: &[&str] = &["&&", "||", "|", ";", "&", ">", ">>", "<", "2>&1"];
const INTERPRETERS: &[&str] = &["sh", "bash", "dash", "zsh", "fish", "env", "xargs", "sudo"];

/// Quote an argument only when the shell would not read it as one word.
fn quote_command_argument(value: &str) -> String {
    let plain = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if plain {
        value.to_string()
    } else {
        quote_any(value)
    }
}

/// A command split into arguments, with the index of its program.
pub(super) struct Argv {
    pub(super) parts: Vec<String>,
    pub(super) program: usize,
    /// For each part, its source text when the shell would expand it
    /// (`$QF`, `"${OUT:-x.xml}"`). [`Self::render`] writes those back
    /// verbatim: quoting the value would turn the expansion into a literal.
    /// Empty when no part expands.
    pub(super) expanding: Vec<Option<String>>,
}

/// The source text of each word of `command`, split the way the shell does
/// (whitespace outside quotes). `None` for an unterminated quote.
fn raw_words(command: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') | (None, '\\') => {
                word.push(c);
                word.extend(chars.next());
                continue;
            }
            (None, '\'' | '"') => quote = Some(c),
            (None, c) if c.is_whitespace() => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
                continue;
            }
            _ => {}
        }
        word.push(c);
    }
    if quote.is_some() {
        return None;
    }
    if !word.is_empty() {
        words.push(word);
    }
    Some(words)
}

/// Whether the shell expands something in a word's source text: a `$` that
/// is not single-quoted or escaped and starts a name, `${`, or a special
/// parameter. A `$` that ends a word (`'x$'`) is literal.
pub(super) fn expands(raw: &str) -> bool {
    let mut quote: Option<char> = None;
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('\''), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            (_, '\\') => {
                chars.next();
            }
            (_, '$')
                if chars
                    .peek()
                    .is_some_and(|n| n.is_ascii_alphanumeric() || "_{?@*#!$-".contains(*n)) =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

fn is_assignment(value: &str) -> bool {
    value.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Why a test command is not read as plain arguments, and so is not
/// narrowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// `{files}`, a backtick or `$(`.
    Substitution(&'static str),
    /// The quoting does not balance.
    Quoting,
    /// A shell operator joins several commands or redirects one.
    ShellOperator(String),
    /// Only environment assignments.
    NoProgram,
    /// The program is an interpreter, so what it runs is in its arguments.
    Interpreter(String),
    /// The shell expands a word in a way that cannot be written back.
    Expansion,
    /// The program word itself is expanded.
    ExpandedProgram,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Substitution("{files}") => write!(
                f,
                "it contains a `{{files}}` placeholder, which is filled only for a target with the files_list capability"
            ),
            Self::Substitution(what) => {
                write!(f, "it contains `{what}`, a shell substitution")
            }
            Self::Quoting => write!(f, "its quoting does not balance"),
            Self::ShellOperator(op) => write!(f, "it uses the shell operator `{op}`"),
            Self::NoProgram => write!(f, "it has no program, only environment assignments"),
            Self::Interpreter(name) => write!(
                f,
                "it runs through the interpreter `{name}` as a script Aster does not read (a wrapper program can be listed in `[affected.related] wrappers`)"
            ),
            Self::Expansion => write!(f, "a shell expansion in its arguments cannot be preserved"),
            Self::ExpandedProgram => write!(f, "its program is a shell expansion"),
        }
    }
}

/// Why `command` is not read as plain arguments; `None` when it is.
pub(crate) fn refusal(command: &str) -> Option<Refusal> {
    Argv::try_parse(command).err()
}

/// The simple commands inside `command`: its parts between shell
/// operators, and the script of an interpreter's `-c` argument, read the
/// same way. Used to tell whether a refused command contains a test runner.
pub(crate) fn simple_commands(command: &str) -> Vec<String> {
    fn collect(command: &str, depth: usize, out: &mut Vec<String>) {
        let Ok(parts) = shell_words::split(command) else {
            return;
        };
        let mut current: Vec<String> = Vec::new();
        let flush = |current: &mut Vec<String>, out: &mut Vec<String>| {
            let words = std::mem::take(current);
            let Some(program) = words.iter().position(|w| !is_assignment(w)) else {
                return;
            };
            let name = Path::new(&words[program])
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            if INTERPRETERS.contains(&name) && depth < 4 {
                let script = words[program + 1..]
                    .iter()
                    .position(|w| w.starts_with('-') && !w.starts_with("--") && w.ends_with('c'))
                    .and_then(|at| words.get(program + 2 + at));
                match script {
                    Some(script) => collect(script, depth + 1, out),
                    // `env VAR=1 cmd`, `sudo cmd`, `xargs cmd`.
                    None => {
                        let rest: Vec<String> = words[program + 1..]
                            .iter()
                            .skip_while(|w| is_assignment(w))
                            .cloned()
                            .collect();
                        if !rest.is_empty() {
                            out.push(rest.join(" "));
                        }
                    }
                }
            } else {
                out.push(
                    words
                        .iter()
                        .map(|w| quote_command_argument(w))
                        .collect::<Vec<_>>()
                        .join(" "),
                );
            }
        };
        for part in parts {
            if SHELL_OPERATORS.contains(&part.as_str()) {
                flush(&mut current, out);
            } else {
                current.push(part);
            }
        }
        flush(&mut current, out);
    }
    let mut out = Vec::new();
    collect(command, 0, &mut out);
    out
}

/// The ways to read the end of `command` as a command of its own, after a
/// leading program and its arguments: `(skipped words, rest)`. A program Aster
/// does not know that runs the command given as its arguments (`mise exec
/// node@24 -- vitest run`) is what makes the whole command unreadable.
pub(crate) fn trailing_commands(command: &str) -> Vec<(String, String)> {
    let Ok(parts) = shell_words::split(command) else {
        return Vec::new();
    };
    let start = parts.iter().position(|w| !is_assignment(w)).unwrap_or(0);
    (start + 1..parts.len())
        .map(|at| {
            (
                parts[..at]
                    .iter()
                    .map(|w| quote_command_argument(w))
                    .collect::<Vec<_>>()
                    .join(" "),
                parts[at..]
                    .iter()
                    .map(|w| quote_command_argument(w))
                    .collect::<Vec<_>>()
                    .join(" "),
            )
        })
        .collect()
}

impl Argv {
    pub(super) fn parse(command: &str) -> Option<Self> {
        Self::try_parse(command).ok()
    }

    fn try_parse(command: &str) -> Result<Self, Refusal> {
        for (needle, what) in [("{files}", "{files}"), ("`", "`"), ("$(", "$(")] {
            if command.contains(needle) {
                return Err(Refusal::Substitution(what));
            }
        }
        let parts = shell_words::split(command).map_err(|_| Refusal::Quoting)?;
        if let Some(op) = parts.iter().find(|p| SHELL_OPERATORS.contains(&p.as_str())) {
            return Err(Refusal::ShellOperator(op.clone()));
        }
        let program = parts
            .iter()
            .position(|p| !is_assignment(p))
            .ok_or(Refusal::NoProgram)?;
        let name = Path::new(&parts[program])
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or(Refusal::NoProgram)?;
        if INTERPRETERS.contains(&name) {
            return Err(Refusal::Interpreter(name.to_string()));
        }
        // Keep the text of the words the shell expands. If the words cannot
        // be matched up with the split, rendering would quote an expansion
        // away, so the command is not understood.
        let mut expanding = Vec::new();
        if command.contains('$') {
            let raw = raw_words(command)
                .filter(|raw| raw.len() == parts.len())
                .ok_or(Refusal::Expansion)?;
            if raw.iter().any(|word| expands(word)) {
                expanding = raw
                    .into_iter()
                    .map(|word| expands(&word).then_some(word))
                    .collect();
            }
        }
        // An expansion in the program position could run anything.
        if expanding.get(program).is_some_and(Option::is_some) {
            return Err(Refusal::ExpandedProgram);
        }
        Ok(Self {
            parts,
            program,
            expanding,
        })
    }

    /// One argument as written in a rendered command.
    fn word(&self, index: usize) -> String {
        if let Some(Some(raw)) = self.expanding.get(index) {
            return raw.clone();
        }
        if index < self.program {
            self.parts[index].clone()
        } else {
            quote_command_argument(&self.parts[index])
        }
    }

    /// The command with the arguments at `drop` removed and `add` appended.
    pub(super) fn render(&self, drop: &[usize], add: &[String]) -> String {
        self.parts
            .iter()
            .enumerate()
            .filter(|(i, _)| !drop.contains(i))
            // Environment assignments stay unquoted to be read as
            // assignments.
            .map(|(i, _)| self.word(i))
            .chain(add.iter().map(|a| quote_command_argument(a)))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The command with `add` inserted before the argument at `at` (or
    /// appended when `at` is past the end).
    pub(super) fn render_inserting(&self, at: usize, add: &[String]) -> String {
        let mut parts: Vec<String> = Vec::with_capacity(self.parts.len() + add.len());
        for i in 0..self.parts.len() {
            if i == at {
                parts.extend(add.iter().map(|a| quote_command_argument(a)));
            }
            parts.push(self.word(i));
        }
        if at >= self.parts.len() {
            parts.extend(add.iter().map(|a| quote_command_argument(a)));
        }
        parts.join(" ")
    }

    /// Indexes after `from` of arguments that are not options or option
    /// values. `value_flags` take the next argument as their value.
    pub(super) fn positionals(&self, from: usize, value_flags: &[&str]) -> Vec<usize> {
        let mut out = Vec::new();
        let mut i = from;
        while i < self.parts.len() {
            let part = self.parts[i].as_str();
            if part == "--" {
                out.extend(i + 1..self.parts.len());
                break;
            }
            if part.starts_with('-') {
                if value_flags.contains(&part) {
                    i += 1;
                }
            } else {
                out.push(i);
            }
            i += 1;
        }
        out
    }
}

/// A shell one-liner that ends in a test command, possibly in another
/// directory: `bash -c 'setup; cd ../core && PARTITION=1 exec mix test'`.
/// This is how a shard project runs a slice of another project's tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Delegated {
    /// The shell invocation up to the script: `bash`, `-c`.
    shell: Vec<String>,
    /// The script up to the test command, kept verbatim.
    head: String,
    /// The directory the test command runs in, relative to the target's.
    pub(crate) dir: Option<String>,
    /// The test command.
    pub(crate) command: String,
}

impl Delegated {
    pub(crate) fn parse(command: &str) -> Option<Self> {
        Self::parse_with(command, false)
    }

    /// [`Self::parse`] for callers that only append flags to the test
    /// command: it may also contain shell expansions (`$FILTER`,
    /// `${OUT:-x.xml}`), which are carried through untouched. Callers that
    /// read the command's arguments (to narrow it to files, say) use
    /// [`Self::parse`], which treats an expansion as an unknown command.
    ///
    /// Still refused: an expansion in the program position, command
    /// substitution (`$(...)`, backticks) and an unbalanced `${`, none of
    /// which leave the shape of the arguments predictable.
    pub(crate) fn parse_allowing_expansions(command: &str) -> Option<Self> {
        Self::parse_with(command, true)
    }

    fn parse_with(command: &str, allow_expansions: bool) -> Option<Self> {
        let mut parts = shell_words::split(command).ok()?;
        let script = parts.pop()?;
        let (program, flags) = parts.split_first()?;
        let shell = Path::new(program).file_name()?.to_str()?;
        // Exactly `<shell> [-flags] -c <script>`; flags may be combined (`-lc`).
        let runs_script = flags
            .last()
            .is_some_and(|f| f.starts_with('-') && !f.starts_with("--") && f.ends_with('c'));
        if !matches!(shell, "sh" | "bash" | "zsh" | "dash") || !runs_script {
            return None;
        }

        // Top-level `&&` and `;`, as (start, end) byte offsets.
        let mut separators = Vec::new();
        let mut quote: Option<char> = None;
        let chars: Vec<(usize, char)> = script.char_indices().collect();
        let mut i = 0;
        while i < chars.len() {
            let (at, c) = chars[i];
            match quote {
                Some(q) => {
                    if c == '\\' && q == '"' {
                        i += 1;
                    } else if c == q {
                        quote = None;
                    }
                }
                None => match c {
                    '\'' | '"' => quote = Some(c),
                    '\\' => i += 1,
                    ';' => separators.push((at, at + 1)),
                    '&' if chars.get(i + 1).map(|next| next.1) == Some('&') => {
                        separators.push((at, at + 2));
                        i += 1;
                    }
                    _ => {}
                },
            }
            i += 1;
        }
        if quote.is_some() {
            return None;
        }
        let tail_start = separators.last().map_or(0, |s| s.1);
        let mut head = script[..tail_start].to_string();
        let mut tail = script[tail_start..].trim();
        // The last command must be a plain one.
        if tail.is_empty() || tail.contains(['|', '&', '<', '>', '(', ')', '`', '\n']) {
            return None;
        }
        if tail.contains('$')
            && (!allow_expansions || tail.matches("${").count() != tail.matches('}').count())
        {
            return None;
        }
        if !head.is_empty() {
            head.push(' ');
        }
        // Leading assignments and `exec` stay with the head.
        loop {
            let word = tail.split_whitespace().next()?;
            if word != "exec" && !is_assignment(word) {
                break;
            }
            if word.contains(['\'', '"']) {
                return None;
            }
            head.push_str(word);
            head.push(' ');
            tail = tail[word.len()..].trim_start();
        }
        // The program is not a word to guess at.
        if tail.split_whitespace().next()?.contains('$') {
            return None;
        }
        // `cd <dir> &&` immediately before the command.
        let dir = match separators.as_slice() {
            [.., last] if last.1 - last.0 == 2 => {
                let start = separators
                    .len()
                    .checked_sub(2)
                    .map_or(0, |previous| separators[previous].1);
                let words: Vec<&str> = script[start..last.0].split_whitespace().collect();
                match words.as_slice() {
                    ["cd", dir] if !dir.contains(['\'', '"', '$', '`', '~']) => {
                        Some(dir.to_string())
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        // Any other `cd` leaves the working directory unknown.
        let cds = script[..tail_start]
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .filter(|word| *word == "cd" || *word == "pushd")
            .count();
        if cds != usize::from(dir.is_some()) {
            return None;
        }
        Some(Self {
            shell: parts,
            head,
            dir,
            command: tail.to_string(),
        })
    }

    /// The environment assignments written before the test command
    /// (`A=1 exec mix test` has `A=1`), each followed by a space.
    pub(crate) fn environment(&self) -> String {
        let segment = self
            .head
            .rsplit("&&")
            .next()
            .and_then(|rest| rest.rsplit(';').next())
            .unwrap_or("");
        segment
            .split_whitespace()
            .filter(|word| is_assignment(word))
            .map(|word| format!("{word} "))
            .collect()
    }

    /// Whether the script before the test command may change its
    /// environment in a way [`Self::environment`] does not show:
    /// `export`, `source env.sh`, `set -a`, `eval` and the like.
    pub(crate) fn hides_environment(&self) -> bool {
        self.head
            .split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')'))
            .any(|word| {
                matches!(
                    word,
                    "export" | "source" | "." | "set" | "eval" | "declare" | "typeset" | "unset"
                )
            })
    }

    /// The same one-liner without the environment assignments of `name`
    /// written before the test command.
    pub(crate) fn without_assignment(&self, name: &str) -> Self {
        let prefix = format!("{name}=");
        let head = self
            .head
            .split(' ')
            .filter(|word| !word.starts_with(&prefix))
            .collect::<Vec<_>>()
            .join(" ");
        Self {
            head,
            ..self.clone()
        }
    }

    /// The one-liner with its test command replaced.
    pub(crate) fn render(&self, command: &str) -> String {
        let script = format!("{}{command}", self.head);
        self.shell
            .iter()
            .map(|part| quote_command_argument(part))
            .chain([quote_any(&script)])
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Programs configured in `[affected.related] wrappers` that run the command
/// given as their arguments.
#[derive(Debug, Clone, Default)]
pub(crate) struct Wrappers {
    /// `(program, until)`.
    entries: Vec<(String, Option<String>)>,
}

/// A command split into its wrappers and the command they run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Wrapped {
    /// Everything before the wrapped command, as written, with a trailing
    /// space.
    pub(crate) prefix: String,
    /// The wrapped command.
    pub(crate) command: String,
}

impl Wrappers {
    /// More than this many nested wrappers is not a command to guess at.
    const MAX_DEPTH: usize = 4;

    pub(crate) fn new(config: &[crate::config::RelatedWrapperConfig]) -> Self {
        Self {
            entries: config
                .iter()
                .map(|wrapper| {
                    let (program, until) = wrapper.parts();
                    (program.to_string(), until.map(str::to_string))
                })
                .collect(),
        }
    }

    /// Whether the written program word is the configured one: the same
    /// word, or the same file name when the entry names no directory.
    fn is_program(entry: &str, word: &str) -> bool {
        word == entry
            || (!entry.contains('/')
                && Path::new(word)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|name| name == entry))
    }

    /// The command a wrapper runs, when `command` starts with one (after any
    /// `NAME=value` assignments). Wrappers may nest. Words keep their
    /// quoting, and runs of spaces collapse to one.
    pub(crate) fn strip(&self, command: &str) -> Option<Wrapped> {
        if self.entries.is_empty() || command.contains("{files}") {
            return None;
        }
        let mut words = raw_words(command)?;
        let mut taken = 0;
        let mut prefix = Vec::new();
        for _ in 0..Self::MAX_DEPTH {
            let program = words.iter().position(|w| !is_assignment(w))?;
            let found = self.entries.iter().find_map(|(entry, until)| {
                if !Self::is_program(entry, &words[program]) {
                    return None;
                }
                match until {
                    None => Some(program + 1),
                    Some(until) => words[program + 1..]
                        .iter()
                        .position(|w| w == until)
                        .map(|at| program + 2 + at),
                }
            });
            let Some(start) = found.filter(|&start| start < words.len()) else {
                break;
            };
            prefix.extend(words.drain(..start));
            taken += 1;
        }
        if taken == 0 {
            return None;
        }
        Some(Wrapped {
            prefix: format!("{} ", prefix.join(" ")),
            command: words.join(" "),
        })
    }
}

/// The selected files that the paths already listed in a command cover:
/// all of them when none is listed. A listed path covers itself and, when
/// it is a directory, everything under it.
fn covered(selected: &[String], listed: &[&str]) -> Vec<String> {
    if listed.is_empty() {
        return selected.to_vec();
    }
    selected
        .iter()
        .filter(|file| {
            listed.iter().any(|path| {
                let path = path.trim_start_matches("./").trim_end_matches('/');
                if is_glob(path) {
                    return glob_covers(path, file);
                }
                path == "." || *file == path || file.starts_with(&format!("{path}/"))
            })
        })
        .cloned()
        .collect()
}

/// Whether a listed path is a pattern the shell (or the runner) expands.
fn is_glob(path: &str) -> bool {
    path.contains(['*', '?', '[', '{'])
}

/// Whether the pattern `glob` matches `file` or a directory above it. `*`
/// does not cross a `/`, as in a shell.
fn glob_covers(glob: &str, file: &str) -> bool {
    let Ok(glob) = globset::GlobBuilder::new(glob)
        .literal_separator(true)
        .build()
    else {
        return false;
    };
    let matcher = glob.compile_matcher();
    Path::new(file)
        .ancestors()
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
        .any(|ancestor| matcher.is_match(ancestor))
}

/// Whether a listed path points outside the project, where the project's
/// selection says nothing about it.
fn escapes(listed: &[&str]) -> bool {
    listed
        .iter()
        .any(|path| path.starts_with("..") || path.starts_with('/'))
}

fn files(tests: &[RelatedTest]) -> Vec<String> {
    tests
        .iter()
        .map(|t| t.file.to_string_lossy().replace('\\', "/"))
        .collect()
}

fn plan(commands: Vec<String>) -> Option<FilesListPlan> {
    Some(if commands.is_empty() {
        FilesListPlan::Nothing
    } else {
        FilesListPlan::Commands(commands)
    })
}

/// `mix test`, including `mix do --app name test`.
pub(super) fn mix(
    project_dir: &Path,
    command: &str,
    tests: &[RelatedTest],
) -> Option<FilesListPlan> {
    let argv = Argv::parse(command)?;
    let mix = argv.parts.iter().position(|p| p == "mix")?;
    let test = mix + argv.parts[mix..].iter().position(|p| p == "test")?;
    // `mix do a, b` runs several tasks; only a lone `test` can be narrowed.
    if argv.parts.iter().any(|p| p.ends_with(',')) {
        return None;
    }
    let listed: Vec<usize> = argv
        .positionals(
            test + 1,
            &[
                "--only",
                "--exclude",
                "--include",
                "--seed",
                "--max-failures",
                "--partitions",
                "--timeout",
                "--formatter",
                "--slowest",
                "--max-cases",
                "--cover-filter",
                "--exit-status",
            ],
        )
        .into_iter()
        .filter(|&i| {
            let path = argv.parts[i].split(':').next().unwrap_or("");
            path.ends_with(".exs") || project_dir.join(path).is_dir()
        })
        .collect();
    let listed_paths: Vec<&str> = listed
        .iter()
        .map(|&i| argv.parts[i].split(':').next().unwrap_or(""))
        .collect();
    if escapes(&listed_paths) {
        return None;
    }
    let keep = covered(&files(tests), &listed_paths);
    if keep.is_empty() {
        return plan(Vec::new());
    }
    // `--partitions N` deals the files it is given out in turn, and
    // partition K of fewer than K files gets none: Mix then fails, saying
    // the paths match nothing.
    if let Some(partition) = mix_partition(&argv, test) {
        let given: BTreeSet<&String> = keep.iter().collect();
        if given.len() < partition {
            return plan(Vec::new());
        }
    }
    let (mut drop, mut add) = MIX_COVERAGE.edit(&argv, test + 1);
    drop.extend(&listed);
    add.splice(0..0, keep);
    plan(vec![argv.render(&drop, &add)])
}

/// Which partition a `mix test --partitions N` command runs, when
/// `MIX_TEST_PARTITION` is written in front of it.
fn mix_partition(argv: &Argv, test: usize) -> Option<usize> {
    let arguments = &argv.parts[test + 1..];
    let total: usize = arguments.iter().enumerate().find_map(|(i, part)| {
        match part.strip_prefix("--partitions") {
            Some("") => arguments.get(i + 1)?.parse().ok(),
            Some(rest) => rest.strip_prefix('=')?.parse().ok(),
            None => None,
        }
    })?;
    let partition: usize = argv.parts[..argv.program]
        .iter()
        .rev()
        .find_map(|part| part.strip_prefix("MIX_TEST_PARTITION="))?
        .parse()
        .ok()?;
    (total > 1 && (1..=total).contains(&partition)).then_some(partition)
}

const JS_RUNNERS: &[&str] = &["vitest", "jest", "mocha"];
const JS_LAUNCHERS: &[&str] = &[
    "npx",
    "bunx",
    "pnpx",
    "pnpm",
    "yarn",
    "npm",
    "bun",
    "exec",
    "x",
    "dlx",
    "cross-env",
];
pub(super) const JS_VALUE_FLAGS: &[&str] = &[
    "--config",
    "-c",
    "--project",
    "--root",
    "-r",
    "--dir",
    "--reporter",
    "--outputFile",
    "--environment",
    "--pool",
    "--maxWorkers",
    "--minWorkers",
    "--testTimeout",
    "--retry",
    "--shard",
    "--mode",
    "-t",
    "--testNamePattern",
    "--bail",
    "--coverage.provider",
    "--testPathPattern",
    "--rootDir",
    "--require",
    "--import",
    "--loader",
    "--timeout",
    "--env-file",
    "--test-reporter",
    "--test-reporter-destination",
    "--test-name-pattern",
    "--test-timeout",
    "--test-concurrency",
    "--test-shard",
];

/// Where a JavaScript runner's own arguments start, if `argv` runs one
/// directly: `vitest run`, `pnpm exec jest`, `bun test`, `node --test`.
pub(super) fn js_runner(argv: &Argv) -> Option<usize> {
    let parts = &argv.parts[argv.program..];
    let name = |part: &str| {
        Path::new(part)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(part)
            .to_string()
    };
    for (offset, part) in parts.iter().enumerate() {
        let at = argv.program + offset;
        let part_name = name(part);
        if JS_RUNNERS.contains(&part_name.as_str()) {
            // `vitest related` and `vitest watch` choose files themselves.
            return match parts.get(offset + 1).map(String::as_str) {
                Some("related" | "watch" | "dev" | "bench" | "list" | "init") => None,
                Some("run") => Some(at + 2),
                _ => Some(at + 1),
            };
        }
        if part_name == "bun" && parts.get(offset + 1).map(String::as_str) == Some("test") {
            return Some(at + 2);
        }
        if matches!(part_name.as_str(), "node" | "tsx") {
            return parts[offset + 1..]
                .iter()
                .any(|p| p == "--test")
                .then_some(at + 1);
        }
        let launcher = JS_LAUNCHERS.contains(&part_name.as_str())
            || part.starts_with('-')
            || is_assignment(part);
        if !launcher {
            return None;
        }
    }
    None
}

/// The package script a command runs: `npm test`, `pnpm run test:unit`.
pub(super) fn js_script(argv: &Argv) -> Option<&str> {
    let parts: Vec<&str> = argv.parts[argv.program..]
        .iter()
        .map(String::as_str)
        .collect();
    match parts.as_slice() {
        ["npm" | "pnpm" | "yarn" | "bun", "run", script] => Some(script),
        ["npm" | "pnpm" | "yarn", "test"] => Some("test"),
        _ => None,
    }
}

/// The package script a command runs, by any spelling the launchers accept:
/// `npm test`, `pnpm run test:ci`, `yarn test:unit`, `bun run test`.
fn script_name(argv: &Argv) -> Option<&str> {
    let parts: Vec<&str> = argv.parts[argv.program..]
        .iter()
        .map(String::as_str)
        .collect();
    match parts.as_slice() {
        ["npm" | "pnpm" | "yarn" | "bun", "run" | "run-script", script] => Some(script),
        ["npm" | "pnpm" | "yarn", "test"] => Some("test"),
        ["pnpm" | "yarn" | "bun", script] if !script.starts_with('-') => Some(script),
        _ => None,
    }
}

/// How many package scripts deep [`package_script_runners`] follows a script
/// that runs another one.
const SCRIPT_DEPTH: usize = 8;

/// The test runner commands that running `command` in `project_dir` runs: the
/// command itself when it is a runner, or, for a package script call
/// (`pnpm test`), the runners in the script's body. A body that chains
/// commands with `&&` is read segment by segment, following the scripts it
/// calls (with a depth and cycle guard); `pre` and `post` hooks are not
/// followed. Empty when `command` runs no runner this way, when the script is
/// missing, or when a script changes directory first, since its runner then
/// belongs to another project.
pub(crate) fn package_script_runners(project_dir: &Path, command: &str) -> Vec<String> {
    fn collect(
        scripts: &serde_json::Map<String, serde_json::Value>,
        command: &str,
        seen: &mut Vec<String>,
        out: &mut Vec<String>,
    ) {
        let start = out.len();
        for segment in simple_commands(command) {
            let Some(argv) = Argv::parse(&segment) else {
                continue;
            };
            if matches!(
                argv.parts.get(argv.program).map(String::as_str),
                Some("cd" | "pushd")
            ) {
                out.truncate(start);
                return;
            }
            if js_runner(&argv).is_some() {
                out.push(segment);
                continue;
            }
            let Some(name) = script_name(&argv) else {
                continue;
            };
            let Some(body) = scripts.get(name).and_then(|b| b.as_str()) else {
                continue;
            };
            if seen.len() >= SCRIPT_DEPTH || seen.iter().any(|s| s == name) {
                continue;
            }
            seen.push(name.to_string());
            collect(scripts, body, seen, out);
            seen.pop();
        }
    }
    let Ok(manifest) = std::fs::read_to_string(project_dir.join("package.json")) else {
        return Vec::new();
    };
    let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&manifest) else {
        return Vec::new();
    };
    let Some(scripts) = manifest.get("scripts").and_then(|s| s.as_object()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect(scripts, command, &mut Vec::new(), &mut out);
    out
}

/// Flags that turn coverage on or configure it, for the runners Aster
/// narrows. Coverage thresholds measure a whole suite, so a run narrowed to a
/// few files fails them although the full run passes; a narrowed run
/// disables coverage instead.
struct CoverageFlags {
    /// A flag starting with one of these is dropped.
    prefixes: &'static [&'static str],
    /// Of those, the ones whose value is the next argument when it is not
    /// written with `=`.
    value_flags: &'static [&'static str],
    /// A flag that switches coverage off, added to a narrowed run.
    off: Option<&'static str>,
    /// Add `off` only when the command turned coverage on itself: the flag
    /// belongs to a plugin that may not be installed.
    off_only_if_present: bool,
}

const VITEST_COVERAGE: CoverageFlags = CoverageFlags {
    prefixes: &["--coverage"],
    value_flags: &[
        "--coverage.provider",
        "--coverage.reporter",
        "--coverage.reportsDirectory",
        "--coverage.include",
        "--coverage.exclude",
        "--coverage.thresholds.statements",
        "--coverage.thresholds.branches",
        "--coverage.thresholds.functions",
        "--coverage.thresholds.lines",
    ],
    // Also overrides `coverage.enabled: true` in the config file.
    off: Some("--coverage.enabled=false"),
    off_only_if_present: false,
};

const JEST_COVERAGE: CoverageFlags = CoverageFlags {
    prefixes: &["--coverage", "--collectCoverage"],
    value_flags: &[
        "--coverageProvider",
        "--coverageReporters",
        "--coverageDirectory",
        "--coverageThreshold",
        "--coveragePathIgnorePatterns",
        "--collectCoverageFrom",
    ],
    off: Some("--coverage=false"),
    off_only_if_present: false,
};

const BUN_COVERAGE: CoverageFlags = CoverageFlags {
    prefixes: &["--coverage"],
    value_flags: &["--coverage-reporter", "--coverage-dir"],
    off: None,
    off_only_if_present: false,
};

const NODE_COVERAGE: CoverageFlags = CoverageFlags {
    prefixes: &["--experimental-test-coverage", "--test-coverage-"],
    value_flags: &[],
    off: None,
    off_only_if_present: false,
};

const PYTEST_COVERAGE: CoverageFlags = CoverageFlags {
    prefixes: &["--cov"],
    value_flags: &[
        "--cov",
        "--cov-report",
        "--cov-config",
        "--cov-fail-under",
        "--cov-context",
    ],
    off: Some("--no-cov"),
    off_only_if_present: true,
};

const MIX_COVERAGE: CoverageFlags = CoverageFlags {
    prefixes: &["--cover", "--export-coverage"],
    value_flags: &["--export-coverage"],
    off: None,
    off_only_if_present: false,
};

impl CoverageFlags {
    /// The indexes of the coverage arguments in `argv` from `from`, and the
    /// flags to add.
    fn edit(&self, argv: &Argv, from: usize) -> (Vec<usize>, Vec<String>) {
        let mut drop = Vec::new();
        let mut i = from;
        while i < argv.parts.len() {
            let part = argv.parts[i].as_str();
            if part == "--" {
                break;
            }
            if self.prefixes.iter().any(|prefix| part.starts_with(prefix)) {
                drop.push(i);
                let next = argv.parts.get(i + 1);
                if self.value_flags.contains(&part) && next.is_some_and(|n| !n.starts_with('-')) {
                    drop.push(i + 1);
                    i += 1;
                }
            }
            i += 1;
        }
        let add = self
            .off
            .filter(|_| !self.off_only_if_present || !drop.is_empty())
            .map(|flag| vec![flag.to_string()])
            .unwrap_or_default();
        (drop, add)
    }
}

/// The coverage flags of the JavaScript runner `argv` runs.
fn js_coverage(argv: &Argv) -> &'static CoverageFlags {
    let names: Vec<&str> = argv.parts[argv.program..]
        .iter()
        .filter_map(|p| Path::new(p).file_name().and_then(|n| n.to_str()))
        .collect();
    if names.contains(&"vitest") {
        &VITEST_COVERAGE
    } else if names.contains(&"jest") {
        &JEST_COVERAGE
    } else if names.contains(&"bun") {
        &BUN_COVERAGE
    } else {
        &NODE_COVERAGE
    }
}

/// Vitest, Jest, Mocha, `bun test` and `node --test`, run directly, through a
/// package script that is nothing but the runner, or through a package
/// script (`npm`/`pnpm`) whose `&&` chain contains runners; see
/// [`script_chain`].
pub(super) fn node(
    project_dir: &Path,
    command: &str,
    tests: &[RelatedTest],
) -> Option<FilesListPlan> {
    let argv = Argv::parse(command)?;
    let selected = files(tests);
    if let Some(start) = js_runner(&argv) {
        // `node --test` takes file patterns, which the shell or the runner
        // expands; the other runners read an argument as a filter.
        let patterns = start > 0
            && matches!(
                Path::new(&argv.parts[start - 1])
                    .file_name()
                    .and_then(|n| n.to_str()),
                Some("node" | "tsx")
            );
        let listed: Vec<usize> = argv
            .positionals(start, JS_VALUE_FLAGS)
            .into_iter()
            .filter(|&i| {
                project_dir.join(&argv.parts[i]).exists() || (patterns && is_glob(&argv.parts[i]))
            })
            .collect();
        // `node --test` runs the files it is given. One that does not exist
        // yet (compiled by an earlier step) is a test the selection cannot
        // name, so the command is left alone.
        if patterns && argv.positionals(start, JS_VALUE_FLAGS).len() != listed.len() {
            return None;
        }
        let listed_paths: Vec<&str> = listed.iter().map(|&i| argv.parts[i].as_str()).collect();
        if escapes(&listed_paths) {
            return None;
        }
        let keep = covered(&selected, &listed_paths);
        if keep.is_empty() {
            return plan(Vec::new());
        }
        let (mut drop, mut add) = js_coverage(&argv).edit(&argv, argv.program);
        drop.extend(&listed);
        add.splice(0..0, keep);
        return plan(vec![argv.render(&drop, &add)]);
    }
    plain_script(project_dir, &argv, selected).or_else(|| script_chain(project_dir, &argv, tests))
}

/// Whether `command` is a JavaScript test runner (Vitest, Jest, Mocha,
/// `bun test`, `node --test`) written directly.
pub(crate) fn is_js_runner(command: &str) -> bool {
    Argv::parse(command).is_some_and(|argv| js_runner(&argv).is_some())
}

/// A package script that is nothing but a runner (`pnpm test` where `test` is
/// `vitest run`): the script call keeps its name and gets the files as
/// arguments.
fn plain_script(project_dir: &Path, argv: &Argv, selected: Vec<String>) -> Option<FilesListPlan> {
    let script = js_script(argv)?;
    let manifest = std::fs::read_to_string(project_dir.join("package.json")).ok()?;
    let manifest: serde_json::Value = serde_json::from_str(&manifest).ok()?;
    let body = manifest.get("scripts")?.get(script)?.as_str()?;
    let script_argv = Argv::parse(body)?;
    let start = js_runner(&script_argv)?;
    // A script that already names paths would run them as well.
    if !script_argv.positionals(start, JS_VALUE_FLAGS).is_empty() {
        return None;
    }
    // The script turns coverage on: its flags cannot be removed from the
    // call, so the script is read as a chain instead.
    let coverage = js_coverage(&script_argv);
    if !coverage
        .edit(&script_argv, script_argv.program)
        .0
        .is_empty()
    {
        return None;
    }
    if selected.is_empty() {
        return plan(Vec::new());
    }
    // npm passes a script its arguments only after `--`. pnpm, yarn and bun
    // pass them as they are and would hand a `--` on to the runner, which
    // Vitest then reads as the end of options: it drops the file filters and
    // runs everything.
    let launcher = Path::new(&argv.parts[argv.program]).file_name()?.to_str()?;
    let mut add = Vec::new();
    if launcher == "npm" {
        add.push("--".to_string());
    }
    add.extend(selected);
    add.extend(coverage.off.map(str::to_string));
    plan(vec![argv.render(&[], &add)])
}

/// Programs that start a package script or a package's binary themselves.
const PACKAGE_LAUNCHERS: &[&str] = &["npm", "pnpm", "yarn", "bun", "npx", "pnpx", "bunx"];

/// The `package.json` scripts of `project_dir`.
fn package_scripts(project_dir: &Path) -> Option<serde_json::Map<String, serde_json::Value>> {
    let manifest = std::fs::read_to_string(project_dir.join("package.json")).ok()?;
    let manifest: serde_json::Value = serde_json::from_str(&manifest).ok()?;
    manifest.get("scripts")?.as_object().cloned()
}

/// The package script `command` calls and its body, for a message about a
/// script Aster cannot narrow.
pub(crate) fn package_script_body(project_dir: &Path, command: &str) -> Option<(String, String)> {
    let argv = Argv::parse(command)?;
    let name = script_name(&argv)?;
    let body = package_scripts(project_dir)?
        .get(name)?
        .as_str()?
        .to_string();
    Some((name.to_string(), body))
}

/// Whether the source text of a word has shell syntax outside quotes that
/// joins or redirects commands.
fn has_shell_syntax(raw: &str) -> bool {
    let mut quote: Option<char> = None;
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('\''), _) => {}
            (Some('"'), '\\') | (None, '\\') => {
                chars.next();
            }
            (None, '\'' | '"') => quote = Some(c),
            (_, '`') => return true,
            (_, '$') if chars.peek() == Some(&'(') => return true,
            (None, ';' | '|' | '&' | '<' | '>' | '(' | ')') => return true,
            _ => {}
        }
    }
    false
}

/// The commands of a script body joined by `&&`, each as source text. `None`
/// when the body uses any other operator, so what runs is not known.
fn and_chain(body: &str) -> Option<Vec<String>> {
    let mut segments = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for word in raw_words(body)? {
        if word == "&&" {
            if current.is_empty() {
                return None;
            }
            segments.push(current.join(" "));
            current.clear();
        } else if has_shell_syntax(&word) {
            return None;
        } else {
            current.push(word);
        }
    }
    if current.is_empty() {
        return None;
    }
    segments.push(current.join(" "));
    Some(segments)
}

/// The commands a package script runs, in order: its `&&` chain with the
/// scripts it calls replaced by their own commands. `None` when the script
/// itself cannot be read reliably: another shell operator, a `cd`, an
/// interpreter running a script, a cycle, nesting deeper than
/// [`SCRIPT_DEPTH`], or a `pre`/`post` hook that npm or pnpm would run around
/// it. A called script that cannot be read is left as the call.
fn script_commands(
    scripts: &serde_json::Map<String, serde_json::Value>,
    name: &str,
    seen: &mut Vec<String>,
    out: &mut Vec<String>,
) -> Option<()> {
    if seen.len() >= SCRIPT_DEPTH || seen.iter().any(|s| s == name) {
        return None;
    }
    if scripts.contains_key(&format!("pre{name}")) || scripts.contains_key(&format!("post{name}")) {
        return None;
    }
    let body = scripts.get(name)?.as_str()?;
    seen.push(name.to_string());
    for segment in and_chain(body)? {
        let argv = Argv::parse(&segment)?;
        let program = Path::new(&argv.parts[argv.program])
            .file_name()
            .and_then(|n| n.to_str())?;
        if matches!(program, "cd" | "pushd") {
            return None;
        }
        match script_name(&argv).filter(|n| scripts.contains_key(*n)) {
            Some(called) if PACKAGE_LAUNCHERS.contains(&program) => {
                let (kept_commands, kept_seen) = (out.len(), seen.len());
                if script_commands(scripts, called, seen, out).is_none() {
                    // A script that cannot be read (a cycle, a `cd`, a hook)
                    // stays a call, which the package manager runs as it
                    // always did.
                    out.truncate(kept_commands);
                    seen.truncate(kept_seen);
                    out.push(segment);
                }
            }
            _ => out.push(segment),
        }
    }
    seen.pop();
    Some(())
}

/// A command as the package script that contained it would have run it:
/// with the project's `node_modules/.bin` on the path. Calls to a package
/// launcher or to a path start a program themselves and are left alone.
fn in_package(command: &str, launcher: &str) -> String {
    let Some(words) = raw_words(command) else {
        return command.to_string();
    };
    let Some(program) = words.iter().position(|w| !is_assignment(w)) else {
        return command.to_string();
    };
    let name = Path::new(&words[program])
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if PACKAGE_LAUNCHERS.contains(&name) || words[program].contains('/') {
        return command.to_string();
    }
    if launcher == "pnpm" {
        // The assignments stay in front, where the shell reads them.
        let mut out = words[..program].to_vec();
        out.push("pnpm exec".to_string());
        out.extend(words[program..].iter().cloned());
        out.join(" ")
    } else {
        format!("npm exec -c {}", quote_any(command))
    }
}

/// A package script (`npm` or `pnpm`) whose body is a chain of commands:
/// `pnpm test-ci` where `test-ci` is `pnpm test:unit && tsc --noEmit`, with
/// `test:unit` itself `vitest run`.
///
/// The chain is expanded through the scripts it calls ([`script_commands`])
/// and each command is narrowed or kept: a test runner command runs only the
/// selected files it covers, or is dropped when none; every other command
/// (`tsc --noEmit`, a generator's `--check`) is kept as written. Each command
/// is started through `pnpm exec` or `npm exec -c`, so it finds the binaries
/// in `node_modules/.bin` as it did inside the script. The commands run in
/// order and the target fails at the first that fails, as `&&` does.
///
/// A runner whose own arguments cannot be narrowed (it lists a test file that
/// does not exist yet, say) is kept as written, so it runs in full. `None`
/// for a chain with no test runner (it is not a test command), one that
/// cannot be read, or one in which no runner was narrowed or dropped. When
/// every runner is dropped the plan is [`FilesListPlan::Nothing`] and the
/// commands that are not runners do not run alone.
fn script_chain(project_dir: &Path, argv: &Argv, tests: &[RelatedTest]) -> Option<FilesListPlan> {
    let launcher = Path::new(&argv.parts[argv.program]).file_name()?.to_str()?;
    if !matches!(launcher, "npm" | "pnpm") {
        return None;
    }
    let scripts = package_scripts(project_dir)?;
    let mut leaves = Vec::new();
    script_commands(&scripts, script_name(argv)?, &mut Vec::new(), &mut leaves)?;
    let mut commands = Vec::new();
    // Runners narrowed to some files, dropped for selecting none, and kept
    // as written because their own arguments could not be narrowed.
    let (mut narrowed, mut dropped, mut kept) = (0, 0, 0);
    for leaf in leaves {
        let leaf_argv = Argv::parse(&leaf)?;
        if js_runner(&leaf_argv).is_none() {
            commands.push(in_package(&leaf, launcher));
            continue;
        }
        match node(project_dir, &leaf, tests) {
            Some(FilesListPlan::Commands(selected)) => {
                narrowed += 1;
                // Vitest and Jest exit with an error when a config's
                // `include` matches none of the files they are given, as
                // happens when the selection belongs to another config.
                let names_a_runner = leaf_argv.parts.iter().any(|p| {
                    matches!(
                        Path::new(p).file_name().and_then(|n| n.to_str()),
                        Some("vitest" | "jest")
                    )
                });
                for command in selected {
                    let command = if names_a_runner && !command.contains("--passWithNoTests") {
                        format!("{command} --passWithNoTests")
                    } else {
                        command
                    };
                    commands.push(in_package(&command, launcher));
                }
            }
            Some(FilesListPlan::Nothing) => dropped += 1,
            // Over-approximating is safe: it runs whatever it ran before.
            _ => {
                kept += 1;
                commands.push(in_package(&leaf, launcher));
            }
        }
    }
    if narrowed + dropped + kept == 0 || narrowed + dropped == 0 {
        // No runner, or none that the selection could narrow.
        return None;
    }
    if narrowed + kept == 0 {
        return plan(Vec::new());
    }
    plan(commands)
}

pub(super) const GO_VALUE_FLAGS: &[&str] = &[
    "-tags",
    "-timeout",
    "-count",
    "-p",
    "-parallel",
    "-cpu",
    "-coverprofile",
    "-covermode",
    "-coverpkg",
    "-ldflags",
    "-gcflags",
    "-asmflags",
    "-o",
    "-exec",
    "-bench",
    "-benchtime",
    "-shuffle",
    "-skip",
    "-mod",
    "-modfile",
    "-overlay",
    "-pkgdir",
    "-fuzz",
    "-fuzztime",
    "-list",
    "-outputdir",
    "-vet",
    "-C",
];

/// `go test` flags that change which files a package builds from or how
/// packages resolve, and so must reach `go list` too. `-race`, `-msan` and
/// `-asan` set the build tag of the same name.
const GO_CONTEXT_VALUE_FLAGS: &[&str] = &["C", "tags", "mod", "modfile", "overlay", "compiler"];
const GO_CONTEXT_BOOL_FLAGS: &[&str] = &["race", "msan", "asan"];

/// A `go test` command over relative package patterns. Selected packages
/// run filtered to the selected test functions with `-run`.
pub(super) struct GoTest {
    argv: Argv,
    /// Indexes of the package patterns in `argv`.
    packages: Vec<usize>,
}

impl GoTest {
    pub(super) fn parse(command: &str) -> Option<Self> {
        let argv = Argv::parse(command)?;
        let parts = &argv.parts;
        if Path::new(&parts[argv.program]).file_name()?.to_str()? != "go"
            || parts.get(argv.program + 1).map(String::as_str) != Some("test")
        {
            return None;
        }
        let start = argv.program + 2;
        // A command that already filters tests is left alone.
        if parts[start..].iter().any(|p| {
            p == "-run" || p.starts_with("-run=") || p == "--run" || p.starts_with("--run=")
        }) {
            return None;
        }
        let mut packages = Vec::new();
        for i in argv.positionals(start, GO_VALUE_FLAGS) {
            // Positionals after `-args` belong to the test binary.
            if parts[start..i]
                .iter()
                .any(|p| p == "-args" || p == "--args")
            {
                return None;
            }
            if !(parts[i] == "." || parts[i].starts_with("./")) {
                return None;
            }
            packages.push(i);
        }
        Some(Self { argv, packages })
    }

    /// The package directory of a test file, relative to the project: `.`
    /// for the project root.
    pub(super) fn package_dir(test: &RelatedTest) -> String {
        test.file
            .parent()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| ".".to_string())
    }

    /// Whether the command's package patterns cover the package in `dir`.
    pub(super) fn covers(&self, dir: &str) -> bool {
        let parts = &self.argv.parts;
        self.packages.is_empty() && dir == "."
            || self.packages.iter().any(|&i| {
                let pattern = parts[i].trim_start_matches("./");
                match pattern.strip_suffix("...") {
                    Some(prefix) => {
                        let prefix = prefix.trim_end_matches('/');
                        let below = if prefix.is_empty() {
                            Some(if dir == "." { "" } else { dir })
                        } else if dir == prefix {
                            Some("")
                        } else {
                            dir.strip_prefix(&format!("{prefix}/"))
                        };
                        // `...` skips `testdata` and directories whose
                        // names start with `.` or `_`, as `go test` does.
                        below.is_some_and(|below| {
                            !below.split('/').any(|name| {
                                name == "testdata" || name.starts_with('.') || name.starts_with('_')
                            })
                        })
                    }
                    None => dir == pattern.trim_end_matches('/') || (pattern == "." && dir == "."),
                }
            })
    }

    /// The program that runs the command.
    pub(super) fn program(&self) -> &str {
        &self.argv.parts[self.argv.program]
    }

    /// The environment assignments written before the program.
    pub(super) fn environment(&self) -> Vec<(String, String)> {
        self.argv.parts[..self.argv.program]
            .iter()
            .filter_map(|p| p.split_once('='))
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    /// shell_words removes quoting but does not expand shell expressions.
    /// Never pass those literal expressions to the probe. Even quoted or
    /// escaped literals conservatively fall back: evaluating shell is unsafe.
    pub(super) fn context_is_static(&self) -> bool {
        self.argv.parts[..self.argv.program]
            .iter()
            .chain(self.build_context_flags().iter())
            .all(|part| !part.contains(['$', '`', '~', '*', '?', '[', '{']))
    }

    /// The command's flags that decide which files each package builds
    /// from, in their order, ready to pass to `go list`.
    pub(super) fn build_context_flags(&self) -> Vec<String> {
        let parts = &self.argv.parts;
        let mut flags = Vec::new();
        let mut i = self.argv.program + 2;
        while i < parts.len() {
            let part = parts[i].as_str();
            if part == "--" || part == "-args" || part == "--args" {
                break;
            }
            let Some(flag) = part.strip_prefix("--").or_else(|| part.strip_prefix('-')) else {
                i += 1;
                continue;
            };
            let (name, inline) = match flag.split_once('=') {
                Some((name, _)) => (name, true),
                None => (flag, false),
            };
            if GO_CONTEXT_BOOL_FLAGS.contains(&name)
                || (GO_CONTEXT_VALUE_FLAGS.contains(&name) && inline)
            {
                flags.push(part.to_string());
            } else if GO_CONTEXT_VALUE_FLAGS.contains(&name) {
                flags.push(part.to_string());
                if let Some(value) = parts.get(i + 1) {
                    flags.push(value.clone());
                }
                i += 1;
            } else if GO_VALUE_FLAGS.contains(&part) {
                i += 1;
            }
            i += 1;
        }
        flags
    }

    /// The commands that run `tests`: packages with whole-file selections
    /// in full, the rest filtered to the selected test functions with
    /// `-run`. Tests outside the command's packages are ignored.
    pub(super) fn plan(&self, tests: &[RelatedTest]) -> Option<FilesListPlan> {
        let mut whole: BTreeSet<String> = BTreeSet::new();
        let mut named: BTreeSet<String> = BTreeSet::new();
        let mut names: BTreeSet<&str> = BTreeSet::new();
        for test in tests {
            let dir = Self::package_dir(test);
            if !self.covers(&dir) {
                continue;
            }
            let package = if dir == "." {
                ".".to_string()
            } else {
                format!("./{dir}")
            };
            if test.names.is_empty() {
                whole.insert(package);
            } else {
                named.insert(package);
                names.extend(test.names.iter().map(String::as_str));
            }
        }
        named.retain(|package| !whole.contains(package));
        let mut commands = Vec::new();
        if !whole.is_empty() {
            commands.push(
                self.argv
                    .render(&self.packages, &whole.into_iter().collect::<Vec<_>>()),
            );
        }
        if !named.is_empty() {
            let pattern = format!("^({})$", names.into_iter().collect::<Vec<_>>().join("|"));
            let mut add = vec!["-run".to_string(), pattern];
            add.extend(named);
            commands.push(self.argv.render(&self.packages, &add));
        }
        plan(commands)
    }
}

pub(super) const PYTEST_VALUE_FLAGS: &[&str] = &[
    "-k",
    "-m",
    "-p",
    "-c",
    "-o",
    "-n",
    "-W",
    "-r",
    "--rootdir",
    "--confcutdir",
    "--basetemp",
    "--maxfail",
    "--tb",
    "--durations",
    "--junitxml",
    "--cov",
    "--cov-report",
    "--cov-config",
    "--deselect",
    "--ignore",
    "--ignore-glob",
    "--import-mode",
    "--timeout",
    "--dist",
    "--override-ini",
    "--log-level",
    "--color",
];

/// `pytest`, however it is launched (`python -m pytest`, `uv run pytest`).
pub(super) fn pytest(
    project_dir: &Path,
    command: &str,
    tests: &[RelatedTest],
) -> Option<FilesListPlan> {
    let argv = Argv::parse(command)?;
    let runner = argv.parts.iter().position(|p| {
        let name = Path::new(p)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(p);
        name == "pytest" || name == "py.test"
    })?;
    let listed: Vec<usize> = argv
        .positionals(runner + 1, PYTEST_VALUE_FLAGS)
        .into_iter()
        .filter(|&i| {
            let path = argv.parts[i].split("::").next().unwrap_or("");
            project_dir.join(path).exists()
        })
        .collect();
    let listed_paths: Vec<&str> = listed
        .iter()
        .map(|&i| argv.parts[i].split("::").next().unwrap_or(""))
        .collect();
    if escapes(&listed_paths) {
        return None;
    }
    let keep = covered(&files(tests), &listed_paths);
    if keep.is_empty() {
        return plan(Vec::new());
    }
    let (mut drop, mut add) = PYTEST_COVERAGE.edit(&argv, runner + 1);
    drop.extend(&listed);
    add.splice(0..0, keep);
    plan(vec![argv.render(&drop, &add)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn go(command: &str, tests: &[RelatedTest]) -> Option<FilesListPlan> {
        GoTest::parse(command)?.plan(tests)
    }

    fn tests_of(files: &[(&str, &[&str])]) -> Vec<RelatedTest> {
        files
            .iter()
            .map(|(file, names)| RelatedTest {
                file: PathBuf::from(file),
                names: names.iter().map(|n| n.to_string()).collect(),
            })
            .collect()
    }

    fn commands(plan: Option<FilesListPlan>) -> Vec<String> {
        match plan {
            Some(FilesListPlan::Commands(commands)) => commands,
            other => panic!("expected commands, got {other:?}"),
        }
    }

    #[test]
    fn shell_one_liners_expose_their_final_test_command() {
        let shard = Delegated::parse(
            "bash -c 'cd ../core && MIX_TEST_PARTITION=1 exec mix test --warnings-as-errors --partitions 2'",
        )
        .unwrap();
        assert_eq!(shard.dir.as_deref(), Some("../core"));
        assert_eq!(
            shard.command,
            "mix test --warnings-as-errors --partitions 2"
        );
        let rendered = shard.render("mix test --partitions 2 test/a_test.exs");
        assert_eq!(
            shell_words::split(&rendered).unwrap(),
            [
                "bash",
                "-c",
                "cd ../core && MIX_TEST_PARTITION=1 exec mix test --partitions 2 test/a_test.exs"
            ]
        );

        let preamble = Delegated::parse(
            r#"bash -c 'if [ -n "${GITHUB_OUTPUT:-}" ]; then echo started=true >> "$GITHUB_OUTPUT"; fi; cd ../archdev && exec pnpm exec vitest run --shard=1/2'"#,
        )
        .unwrap();
        assert_eq!(preamble.dir.as_deref(), Some("../archdev"));
        assert_eq!(preamble.command, "pnpm exec vitest run --shard=1/2");
        let script =
            shell_words::split(&preamble.render("pnpm exec vitest run --shard=1/2 a.test.ts"))
                .unwrap()
                .pop()
                .unwrap();
        assert_eq!(
            script,
            r#"if [ -n "${GITHUB_OUTPUT:-}" ]; then echo started=true >> "$GITHUB_OUTPUT"; fi; cd ../archdev && exec pnpm exec vitest run --shard=1/2 a.test.ts"#
        );

        let local = Delegated::parse("bash -c 'bash check.sh && go test ./...'").unwrap();
        assert_eq!(local.dir, None);
        assert_eq!(local.command, "go test ./...");
    }

    #[test]
    fn one_liners_that_set_the_environment_out_of_sight_say_so() {
        for (command, hides) in [
            ("bash -c 'go test ./...'", false),
            (
                "bash -c 'cd ../core && GOFLAGS=-tags=x exec go test ./...'",
                false,
            ),
            ("bash -c 'make setup && go test ./...'", false),
            ("bash -c 'export GOFLAGS=-tags=x; go test ./...'", true),
            ("bash -c 'source env.sh && go test ./...'", true),
            ("bash -c '. ./env.sh; go test ./...'", true),
            ("bash -c 'set -a; . .env; set +a; go test ./...'", true),
            (
                "sh -c 'eval \"$(direnv export bash)\"; go test ./...'",
                true,
            ),
        ] {
            let delegated = Delegated::parse(command);
            assert_eq!(
                delegated.as_ref().map(Delegated::hides_environment),
                Some(hides),
                "{command}"
            );
        }
    }

    #[test]
    fn expansions_are_written_back_as_they_were() {
        let argv = Argv::parse(r#"vitest run $QF "${OUT:-a b}" 'x$' "\$HOME" -t "a b""#).unwrap();
        assert_eq!(
            argv.render(&[], &["--flag".to_string()]),
            r#"vitest run $QF "${OUT:-a b}" 'x$' '$HOME' -t 'a b' --flag"#
        );
        assert_eq!(
            argv.render_inserting(2, &["--flag".to_string()]),
            r#"vitest --flag run $QF "${OUT:-a b}" 'x$' '$HOME' -t 'a b'"#
                .replace("--flag run", "run --flag")
        );
        // An expansion in the program position is not a known command.
        assert!(Argv::parse("$RUNNER run").is_none());
        assert!(Argv::parse("A=1 ${RUNNER} run").is_none());
    }

    #[test]
    fn only_callers_that_append_flags_see_through_expansions() {
        let command = r#"bash -c 'exec vitest run ${QF:-flaky} "$OTHER" -t "a $B"'"#;
        assert_eq!(Delegated::parse(command), None);
        let delegated = Delegated::parse_allowing_expansions(command).unwrap();
        assert_eq!(
            delegated.command,
            r#"vitest run ${QF:-flaky} "$OTHER" -t "a $B""#
        );
        assert_eq!(
            delegated.render("vitest run --x"),
            r#"bash -c 'exec vitest run --x'"#
        );
        // The boundary: what runs, or the shape of its arguments, must not
        // depend on an expansion.
        for refused in [
            "bash -c '$RUNNER run'",
            "bash -c 'exec ${RUNNER} run'",
            "bash -c 'FOO=1 $RUNNER run'",
            "bash -c 'vitest run $(ls)'",
            "bash -c 'vitest run \"$(ls)\"'",
            "bash -c 'vitest run `ls`'",
            "bash -c 'vitest run ${QF:-a'",
            "bash -c 'cd \"$DIR\" && vitest run $QF'",
        ] {
            assert_eq!(
                Delegated::parse_allowing_expansions(refused),
                None,
                "{refused}"
            );
        }
    }

    #[test]
    fn shell_one_liners_that_do_not_end_in_a_plain_command_are_left_alone() {
        // The directory is not known.
        assert_eq!(Delegated::parse(r#"bash -c 'cd "$DIR" && mix test'"#), None);
        assert_eq!(
            Delegated::parse("bash -c 'cd ../core; cd lib && mix test'"),
            None
        );
        // The last command is piped or runs in a subshell.
        assert_eq!(Delegated::parse("bash -c 'mix test | tee log'"), None);
        assert_eq!(Delegated::parse("bash -c '(cd ../core && mix test)'"), None);
        // Not a `-c` script.
        assert_eq!(Delegated::parse("bash script.sh"), None);
        assert_eq!(Delegated::parse("mix test"), None);
        // The last command of a block is the block's terminator, which no
        // runner recognises.
        let block = Delegated::parse("bash -c 'if x; then mix test; fi'").unwrap();
        assert_eq!(block.command, "fi");
    }

    #[test]
    fn mix_appends_files_and_keeps_environment_and_umbrella_form() {
        let dir = TempDir::new().unwrap();
        let tests = tests_of(&[("test/a_test.exs", &[]), ("test/b_test.exs", &[])]);
        assert_eq!(
            commands(mix(dir.path(), "MIX_ENV=test mix do --app shop test --warnings-as-errors", &tests)),
            ["MIX_ENV=test mix do --app shop test --warnings-as-errors test/a_test.exs test/b_test.exs"]
        );
        assert_eq!(
            mix(dir.path(), "mix test", &[]),
            Some(FilesListPlan::Nothing)
        );
        assert_eq!(mix(dir.path(), "mix do compile, test", &tests), None);
        assert_eq!(mix(dir.path(), "bash -c 'mix test'", &tests), None);
        assert_eq!(mix(dir.path(), "mix credo", &tests), None);
    }

    #[test]
    fn listed_files_are_intersected_with_the_selection() {
        let dir = TempDir::new().unwrap();
        let tests = tests_of(&[("test/b_test.exs", &[])]);
        assert_eq!(
            commands(mix(
                dir.path(),
                "mix test test/a_test.exs test/b_test.exs --seed 0",
                &tests
            )),
            ["mix test --seed 0 test/b_test.exs"]
        );
        assert_eq!(
            mix(dir.path(), "mix test test/a_test.exs", &tests),
            Some(FilesListPlan::Nothing)
        );
        // A test outside the project is not covered by its selection.
        assert_eq!(
            mix(dir.path(), "mix test ../other/x_test.exs", &tests),
            None
        );
    }

    fn wrappers() -> Wrappers {
        use crate::config::{RelatedWrapperConfig, RelatedWrapperTable};
        Wrappers::new(&[
            RelatedWrapperConfig::Program("scripts/with-toolchain".into()),
            RelatedWrapperConfig::Program("env-run".into()),
            RelatedWrapperConfig::Detailed(RelatedWrapperTable {
                program: "mise".into(),
                until: Some("--".into()),
            }),
        ])
    }

    #[test]
    fn wrappers_expose_the_command_they_run() {
        let wrappers = wrappers();
        let strip = |command: &str| wrappers.strip(command).map(|w| (w.prefix, w.command));
        assert_eq!(
            strip("scripts/with-toolchain cargo test -p a"),
            Some(("scripts/with-toolchain ".into(), "cargo test -p a".into()))
        );
        assert_eq!(
            strip("CI=1 mise exec node@24 -- vitest run --reporter 'a b'"),
            Some((
                "CI=1 mise exec node@24 -- ".into(),
                "vitest run --reporter 'a b'".into()
            ))
        );
        // A name without a directory matches the program's file name.
        assert_eq!(
            strip("/usr/bin/env-run mix test"),
            Some(("/usr/bin/env-run ".into(), "mix test".into()))
        );
        // Wrappers nest.
        assert_eq!(
            strip("mise exec -- scripts/with-toolchain cargo test"),
            Some((
                "mise exec -- scripts/with-toolchain ".into(),
                "cargo test".into()
            ))
        );
        // Not configured, no `until` word, nothing wrapped, or a path that
        // is not the configured one.
        assert_eq!(strip("cargo test"), None);
        assert_eq!(strip("mise exec node@24"), None);
        assert_eq!(strip("mise exec node@24 --"), None);
        assert_eq!(strip("scripts/with-toolchain"), None);
        assert_eq!(strip("other/scripts/with-toolchain cargo test"), None);
        assert_eq!(
            Wrappers::default().strip("scripts/with-toolchain cargo test"),
            None
        );
    }

    #[test]
    fn node_narrows_direct_runners_and_plain_scripts() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"test":"vitest run --passWithNoTests","test:all":"vitest run && tsc","test:src":"vitest run src"}}"#,
        )
        .unwrap();
        let tests = tests_of(&[("src/a.test.ts", &[])]);
        assert_eq!(
            commands(node(
                dir.path(),
                "pnpm exec vitest run --config vitest.e2e.config.ts",
                &tests
            )),
            ["pnpm exec vitest run --config vitest.e2e.config.ts src/a.test.ts --coverage.enabled=false"]
        );
        assert_eq!(
            commands(node(dir.path(), "pnpm test", &tests)),
            ["pnpm test src/a.test.ts --coverage.enabled=false"]
        );
        // Only npm needs `--` to pass arguments to a script; pnpm, yarn and
        // bun would forward it to the runner, which then ignores the files.
        for (command, expected) in [
            (
                "npm test",
                "npm test -- src/a.test.ts --coverage.enabled=false",
            ),
            (
                "npm run test",
                "npm run test -- src/a.test.ts --coverage.enabled=false",
            ),
            (
                "pnpm run test",
                "pnpm run test src/a.test.ts --coverage.enabled=false",
            ),
            (
                "yarn test",
                "yarn test src/a.test.ts --coverage.enabled=false",
            ),
            (
                "yarn run test",
                "yarn run test src/a.test.ts --coverage.enabled=false",
            ),
            (
                "bun run test",
                "bun run test src/a.test.ts --coverage.enabled=false",
            ),
        ] {
            assert_eq!(commands(node(dir.path(), command, &tests)), [expected]);
        }
        // A script with other commands in its chain is expanded; see
        // `package_script_chain_tests`.
        assert_eq!(
            commands(node(dir.path(), "pnpm run test:all", &tests)),
            [
                "pnpm exec vitest run src/a.test.ts --coverage.enabled=false --passWithNoTests",
                "pnpm exec tsc"
            ]
        );
        // A script that names a directory of its own is read through, so
        // its path can be replaced by the selected files.
        std::fs::create_dir(dir.path().join("src")).unwrap();
        assert_eq!(
            commands(node(dir.path(), "pnpm run test:src", &tests)),
            ["pnpm exec vitest run src/a.test.ts --coverage.enabled=false --passWithNoTests"]
        );
        assert_eq!(
            node(dir.path(), "pnpm exec vitest related x.ts", &tests),
            None
        );
        assert_eq!(node(dir.path(), "pnpm run build", &tests), None);
        assert_eq!(
            commands(node(dir.path(), "bun test --timeout=60000", &tests)),
            ["bun test --timeout=60000 src/a.test.ts"]
        );
    }

    #[test]
    fn go_runs_whole_packages_and_named_tests_separately() {
        for (command, known) in [
            ("go test -tags devtool ./...", true),
            ("GOFLAGS=-tags=devtool go test ./...", true),
            ("go test -tags \"$DEV_TAGS\" ./...", false),
            ("GOFLAGS=-tags=$DEV_TAGS go test ./...", false),
            ("go test -tags `tag-command` ./...", false),
            ("go test -overlay ~/overlay.json ./...", false),
        ] {
            assert_eq!(
                GoTest::parse(command).is_some_and(|go| go.context_is_static()),
                known,
                "{command}"
            );
        }

        let tests = tests_of(&[
            ("cart/cart_test.go", &["TestTotal", "TestAdd"]),
            ("stock/stock_test.go", &[]),
            ("root_test.go", &["TestRoot"]),
        ]);
        assert_eq!(
            commands(go("go test -race ./... -count=1", &tests)),
            [
                "go test -race -count=1 ./stock",
                "go test -race -count=1 -run '^(TestAdd|TestRoot|TestTotal)$' . ./cart",
            ]
        );
        assert_eq!(
            commands(go("go test ./cart/...", &tests)),
            ["go test -run '^(TestAdd|TestTotal)$' ./cart"]
        );
        assert_eq!(go("go test ./other", &tests), Some(FilesListPlan::Nothing));
        assert_eq!(go("go test -run TestX ./...", &tests), None);
        assert_eq!(go("go vet ./...", &tests), None);
    }

    #[test]
    fn pytest_replaces_listed_directories_with_selected_files() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("tests/unit")).unwrap();
        let tests = tests_of(&[("tests/unit/test_a.py", &[])]);
        assert_eq!(
            commands(pytest(
                dir.path(),
                "poetry run pytest tests --ignore=tests/slow -q",
                &tests
            )),
            ["poetry run pytest --ignore=tests/slow -q tests/unit/test_a.py"]
        );
        assert_eq!(
            commands(pytest(dir.path(), ".venv/bin/python -m pytest", &tests)),
            [".venv/bin/python -m pytest tests/unit/test_a.py"]
        );
        assert_eq!(pytest(dir.path(), "python -m build", &tests), None);
    }
}

#[cfg(test)]
mod package_script_runner_tests {
    use super::package_script_runners;

    fn runners(scripts: &str, command: &str) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            format!("{{\"scripts\":{{{scripts}}}}}"),
        )
        .unwrap();
        package_script_runners(dir.path(), command)
    }

    #[test]
    fn a_chain_is_read_through_the_scripts_it_calls() {
        let scripts = r#""test":"pnpm test:state && pnpm test:cov","test:state":"vitest run --project state","test:cov":"vitest run --coverage""#;
        assert_eq!(
            runners(scripts, "pnpm test"),
            ["vitest run --project state", "vitest run --coverage"]
        );
    }

    #[test]
    fn scripts_that_call_each_other_stop_at_the_cycle() {
        let scripts = r#""test":"pnpm other && vitest run","other":"pnpm test""#;
        assert_eq!(runners(scripts, "pnpm test"), ["vitest run"]);
    }

    #[test]
    fn a_script_that_changes_directory_or_is_missing_has_no_runner() {
        assert!(runners(r#""test":"cd web && vitest run""#, "pnpm test").is_empty());
        assert!(runners(r#""lint":"eslint .""#, "pnpm test").is_empty());
    }
}

#[cfg(test)]
mod package_script_chain_tests {
    use super::*;
    use std::path::PathBuf;

    fn selected(files: &[&str]) -> Vec<RelatedTest> {
        files
            .iter()
            .map(|f| RelatedTest {
                file: PathBuf::from(f),
                names: Vec::new(),
            })
            .collect()
    }

    /// `node` over a project with these scripts (a JSON object body) and
    /// existing `files`.
    fn narrow(
        scripts: &str,
        files: &[&str],
        command: &str,
        tests: &[&str],
    ) -> Option<FilesListPlan> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            format!("{{\"scripts\":{{{scripts}}}}}"),
        )
        .unwrap();
        for file in files {
            let path = dir.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        node(dir.path(), command, &selected(tests))
    }

    fn run(plan: Option<FilesListPlan>) -> Vec<String> {
        match plan {
            Some(FilesListPlan::Commands(commands)) => commands,
            other => panic!("expected commands, got {other:?}"),
        }
    }

    const NESTED: &str = r#""test-ci":"pnpm unit && pnpm e2e && tsc --noEmit","unit":"vitest run --project unit","e2e":"pnpm run ensure && vitest run --config e2e.config.ts","ensure":"node setup.mjs --check""#;

    #[test]
    fn nested_scripts_expand_into_one_command_per_step() {
        let plan = narrow(NESTED, &[], "pnpm test-ci", &["src/a.test.ts"]);
        assert_eq!(
            run(plan),
            [
                "pnpm exec vitest run --project unit src/a.test.ts --coverage.enabled=false --passWithNoTests",
                "pnpm exec node setup.mjs --check",
                "pnpm exec vitest run --config e2e.config.ts src/a.test.ts --coverage.enabled=false --passWithNoTests",
                "pnpm exec tsc --noEmit",
            ]
        );
    }

    #[test]
    fn a_runner_that_selects_nothing_is_dropped_and_the_rest_keep_their_order() {
        let scripts = r#""t":"vitest run src/old && node gen.mjs --check && vitest run src/new""#;
        let plan = narrow(
            scripts,
            &["src/old/x.test.ts", "src/new/y.test.ts"],
            "pnpm t",
            &["src/new/y.test.ts"],
        );
        assert_eq!(
            run(plan),
            [
                "pnpm exec node gen.mjs --check",
                "pnpm exec vitest run src/new/y.test.ts --coverage.enabled=false --passWithNoTests"
            ]
        );
    }

    #[test]
    fn when_every_runner_is_dropped_the_other_commands_do_not_run_alone() {
        let scripts = r#""t":"node gen.mjs --check && vitest run src/old && tsc""#;
        let plan = narrow(
            scripts,
            &["src/old/x.test.ts"],
            "pnpm t",
            &["src/new/y.test.ts"],
        );
        assert_eq!(plan, Some(FilesListPlan::Nothing));
        let plan = narrow(scripts, &["src/old/x.test.ts"], "pnpm t", &[]);
        assert_eq!(plan, Some(FilesListPlan::Nothing));
    }

    #[test]
    fn a_chain_without_a_runner_is_not_a_test_command() {
        let scripts = r#""t":"eslint . && tsc --noEmit""#;
        assert_eq!(narrow(scripts, &[], "pnpm t", &["a.test.ts"]), None);
    }

    #[test]
    fn node_test_commands_replace_their_files_and_globs() {
        let scripts = r#""t":"NODE_OPTIONS=--max-old-space-size=8192 node --import tsx --test --test-timeout=600000 test/*.test.ts && tsx --test --test-force-exit tests/a.test.ts tests/b.test.ts""#;
        let plan = narrow(
            scripts,
            &["tests/a.test.ts", "tests/b.test.ts"],
            "pnpm run t",
            &["test/x.test.ts", "tests/b.test.ts", "other/c.test.ts"],
        );
        assert_eq!(
            run(plan),
            [
                "NODE_OPTIONS=--max-old-space-size=8192 pnpm exec node --import tsx --test --test-timeout=600000 test/x.test.ts",
                "pnpm exec tsx --test --test-force-exit tests/b.test.ts",
            ]
        );
        // A glob only covers the files it matches, `*` stopping at a `/`.
        let plan = narrow(
            scripts,
            &["tests/a.test.ts", "tests/b.test.ts"],
            "pnpm t",
            &["test/deep/x.test.ts"],
        );
        assert_eq!(plan, Some(FilesListPlan::Nothing));
    }

    #[test]
    fn npm_runs_each_command_in_a_shell_with_the_package_binaries() {
        let plan = narrow(NESTED, &[], "npm run test-ci", &["src/a.test.ts"]);
        let commands = run(plan);
        assert_eq!(commands.len(), 4);
        assert_eq!(
            commands[0],
            "npm exec -c 'vitest run --project unit src/a.test.ts --coverage.enabled=false --passWithNoTests'"
        );
        assert_eq!(commands[3], "npm exec -c 'tsc --noEmit'");
    }

    #[test]
    fn a_launcher_or_a_path_is_not_wrapped_again() {
        let scripts = r#""t":"./scripts/check.sh && pnpm exec vitest run && npx tsc""#;
        let plan = narrow(scripts, &[], "pnpm t", &["a.test.ts"]);
        assert_eq!(
            run(plan),
            [
                "./scripts/check.sh",
                "pnpm exec vitest run a.test.ts --coverage.enabled=false --passWithNoTests",
                "npx tsc"
            ]
        );
    }

    #[test]
    fn a_chain_that_cannot_be_read_is_left_as_written() {
        for scripts in [
            // A directory change.
            r#""t":"cd web && vitest run""#,
            // Another operator.
            r#""t":"vitest run || true""#,
            r#""t":"vitest run; tsc""#,
            r#""t":"vitest run | cat""#,
            // A hook npm or pnpm would run around the script.
            r#""t":"vitest run","pret":"node prep.mjs""#,
            // A script file through an interpreter.
            r#""t":"bash run.sh && vitest run""#,
            // A missing script.
            r#""other":"vitest run""#,
        ] {
            // Not the plain form either: the chain has two commands or more.
            let scripts = if scripts.contains("pret") {
                scripts.replace("vitest run\"", "vitest run && tsc\"")
            } else {
                scripts.to_string()
            };
            assert_eq!(
                narrow(&scripts, &[], "pnpm t", &["a.test.ts"]),
                None,
                "{scripts}"
            );
        }
        // Yarn and bun have no `exec` that finds the package binaries the
        // way a script does.
        let scripts = r#""t":"vitest run && tsc""#;
        assert_eq!(narrow(scripts, &[], "yarn t", &["a.test.ts"]), None);
        assert_eq!(narrow(scripts, &[], "bun run t", &["a.test.ts"]), None);
    }

    #[test]
    fn a_called_script_that_cannot_be_read_stays_a_call() {
        // `build` has a `cd`; `e2e` has a hook npm and pnpm run around it; `a`
        // and `b` call each other.
        let scripts = r#""t":"pnpm run build && pnpm e2e && pnpm a && vitest run","build":"cd web && tsc","e2e":"vitest run --config e2e.config.ts","pree2e":"node prep.mjs","a":"pnpm b","b":"pnpm a""#;
        assert_eq!(
            run(narrow(scripts, &[], "pnpm t", &["src/a.test.ts"])),
            [
                "pnpm run build",
                "pnpm e2e",
                "pnpm a",
                "pnpm exec vitest run src/a.test.ts --coverage.enabled=false --passWithNoTests"
            ]
        );
    }

    #[test]
    fn a_runner_that_cannot_be_narrowed_runs_as_written_beside_the_ones_that_can() {
        // `dist/a.test.js` is built by an earlier step and is not there yet.
        let scripts = r#""t":"node --test dist/a.test.js && vitest run && tsc""#;
        let plan = narrow(scripts, &[], "pnpm t", &["src/a.test.ts"]);
        assert_eq!(
            run(plan),
            [
                "pnpm exec node --test dist/a.test.js",
                "pnpm exec vitest run src/a.test.ts --coverage.enabled=false --passWithNoTests",
                "pnpm exec tsc"
            ]
        );
        // With no runner narrowed, nothing is gained and the target runs as
        // written.
        let scripts = r#""t":"node --test dist/a.test.js && tsc""#;
        assert_eq!(narrow(scripts, &[], "pnpm t", &["src/a.test.ts"]), None);
        // A runner that is declined and one that is dropped: only the first
        // runs.
        let scripts = r#""t":"node --test dist/a.test.js && vitest run src/old && tsc""#;
        let plan = narrow(
            scripts,
            &["src/old/x.test.ts"],
            "pnpm t",
            &["src/new/y.test.ts"],
        );
        assert_eq!(
            run(plan),
            ["pnpm exec node --test dist/a.test.js", "pnpm exec tsc"]
        );
    }
}

#[cfg(test)]
mod coverage_tests {
    //! A narrowed run measures a few files, so a coverage threshold written
    //! for the whole suite would fail it: coverage is turned off.
    use super::*;
    use std::path::PathBuf;

    fn selected(files: &[&str]) -> Vec<RelatedTest> {
        files
            .iter()
            .map(|f| RelatedTest {
                file: PathBuf::from(f),
                names: Vec::new(),
            })
            .collect()
    }

    fn commands(plan: Option<FilesListPlan>) -> Vec<String> {
        match plan {
            Some(FilesListPlan::Commands(commands)) => commands,
            other => panic!("expected commands, got {other:?}"),
        }
    }

    fn project(scripts: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            format!("{{\"scripts\":{{{scripts}}}}}"),
        )
        .unwrap();
        dir
    }

    #[test]
    fn vitest_loses_its_coverage_flags_and_gets_coverage_disabled() {
        let dir = project("");
        let tests = selected(&["src/a.test.ts"]);
        assert_eq!(
            commands(node(
                dir.path(),
                "vitest run --coverage --config vitest.review-coverage.config.ts",
                &tests
            )),
            ["vitest run --config vitest.review-coverage.config.ts src/a.test.ts --coverage.enabled=false"]
        );
        // Dotted flags and their space-separated values go too; other flags stay.
        assert_eq!(
            commands(node(
                dir.path(),
                "vitest run --coverage.provider v8 --coverage.thresholds.lines 80 --coverage.reporter=text --bail 1",
                &tests
            )),
            ["vitest run --bail 1 src/a.test.ts --coverage.enabled=false"]
        );
        // A config that enables coverage is overridden even when the command
        // says nothing about it.
        assert_eq!(
            commands(node(dir.path(), "vitest run", &tests)),
            ["vitest run src/a.test.ts --coverage.enabled=false"]
        );
    }

    #[test]
    fn jest_and_bun_and_node_lose_theirs() {
        let dir = project("");
        let tests = selected(&["src/a.test.ts"]);
        assert_eq!(
            commands(node(
                dir.path(),
                "jest --coverage --coverageThreshold '{\"global\":{\"lines\":80}}' --ci",
                &tests
            )),
            ["jest --ci src/a.test.ts --coverage=false"]
        );
        assert_eq!(
            commands(node(
                dir.path(),
                "bun test --coverage --coverage-reporter lcov --timeout 5",
                &tests
            )),
            ["bun test --timeout 5 src/a.test.ts"]
        );
        assert_eq!(
            commands(node(
                dir.path(),
                "node --test --experimental-test-coverage --test-coverage-lines=80",
                &tests
            )),
            ["node --test src/a.test.ts"]
        );
    }

    #[test]
    fn a_plain_script_gets_the_flag_and_a_script_with_coverage_is_read_as_a_chain() {
        let dir = project(
            r#""test":"vitest run","test:coverage":"vitest run --coverage --config cov.config.ts""#,
        );
        let tests = selected(&["src/a.test.ts"]);
        assert_eq!(
            commands(node(dir.path(), "pnpm test", &tests)),
            ["pnpm test src/a.test.ts --coverage.enabled=false"]
        );
        assert_eq!(
            commands(node(dir.path(), "npm test", &tests)),
            ["npm test -- src/a.test.ts --coverage.enabled=false"]
        );
        // The flag is in the script body, which the call cannot edit.
        assert_eq!(
            commands(node(dir.path(), "pnpm test:coverage", &tests)),
            ["pnpm exec vitest run --config cov.config.ts src/a.test.ts --coverage.enabled=false --passWithNoTests"]
        );
        // yarn has no chain form, so it runs as written.
        assert_eq!(node(dir.path(), "yarn test:coverage", &tests), None);
    }

    #[test]
    fn pytest_drops_cov_flags_and_adds_no_cov_only_when_they_were_there() {
        let dir = tempfile::tempdir().unwrap();
        let tests = selected(&["tests/test_a.py"]);
        assert_eq!(
            commands(pytest(
                dir.path(),
                "pytest --cov=src --cov-report term --cov-fail-under=80 -q",
                &tests
            )),
            ["pytest -q tests/test_a.py --no-cov"]
        );
        assert_eq!(
            commands(pytest(dir.path(), "pytest --cov src --cov-branch", &tests)),
            ["pytest tests/test_a.py --no-cov"]
        );
        // `--no-cov` belongs to pytest-cov, which this command does not use.
        assert_eq!(
            commands(pytest(dir.path(), "pytest -q", &tests)),
            ["pytest -q tests/test_a.py"]
        );
    }

    #[test]
    fn mix_drops_cover() {
        let dir = tempfile::tempdir().unwrap();
        let tests = selected(&["test/a_test.exs"]);
        assert_eq!(
            commands(mix(
                dir.path(),
                "mix test --cover --export-coverage default --warnings-as-errors",
                &tests
            )),
            ["mix test --warnings-as-errors test/a_test.exs"]
        );
    }
}
