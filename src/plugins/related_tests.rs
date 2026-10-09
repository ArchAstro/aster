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
struct Argv {
    parts: Vec<String>,
    program: usize,
}

fn is_assignment(value: &str) -> bool {
    value.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

impl Argv {
    fn parse(command: &str) -> Option<Self> {
        if command.contains("{files}") || command.contains('`') || command.contains("$(") {
            return None;
        }
        let parts = shell_words::split(command).ok()?;
        if parts.iter().any(|p| SHELL_OPERATORS.contains(&p.as_str())) {
            return None;
        }
        let program = parts.iter().position(|p| !is_assignment(p))?;
        let name = Path::new(&parts[program]).file_name()?.to_str()?;
        if INTERPRETERS.contains(&name) {
            return None;
        }
        Some(Self { parts, program })
    }

    /// The command with the arguments at `drop` removed and `add` appended.
    fn render(&self, drop: &[usize], add: &[String]) -> String {
        self.parts
            .iter()
            .enumerate()
            .filter(|(i, _)| !drop.contains(i))
            .map(|(i, part)| {
                // Environment assignments must stay unquoted to be read as
                // assignments.
                if i < self.program {
                    part.clone()
                } else {
                    quote_command_argument(part)
                }
            })
            .chain(add.iter().map(|a| quote_command_argument(a)))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Indexes after `from` of arguments that are not options or option
    /// values. `value_flags` take the next argument as their value.
    fn positionals(&self, from: usize, value_flags: &[&str]) -> Vec<usize> {
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
        if tail.is_empty() || tail.contains(['|', '&', '<', '>', '(', ')', '$', '`', '\n']) {
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
                path == "." || *file == path || file.starts_with(&format!("{path}/"))
            })
        })
        .cloned()
        .collect()
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
    plan(vec![argv.render(&listed, &keep)])
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
const JS_VALUE_FLAGS: &[&str] = &[
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
];

/// Where a JavaScript runner's own arguments start, if `argv` runs one
/// directly: `vitest run`, `pnpm exec jest`, `bun test`, `node --test`.
fn js_runner(argv: &Argv) -> Option<usize> {
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
fn js_script(argv: &Argv) -> Option<&str> {
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

/// Vitest, Jest, Mocha, `bun test` and `node --test`, run directly or
/// through a package script that is nothing but the runner.
pub(super) fn node(
    project_dir: &Path,
    command: &str,
    tests: &[RelatedTest],
) -> Option<FilesListPlan> {
    let argv = Argv::parse(command)?;
    let selected = files(tests);
    if let Some(start) = js_runner(&argv) {
        let listed: Vec<usize> = argv
            .positionals(start, JS_VALUE_FLAGS)
            .into_iter()
            .filter(|&i| project_dir.join(&argv.parts[i]).exists())
            .collect();
        let listed_paths: Vec<&str> = listed.iter().map(|&i| argv.parts[i].as_str()).collect();
        if escapes(&listed_paths) {
            return None;
        }
        let keep = covered(&selected, &listed_paths);
        if keep.is_empty() {
            return plan(Vec::new());
        }
        return plan(vec![argv.render(&listed, &keep)]);
    }
    let script = js_script(&argv)?;
    let manifest = std::fs::read_to_string(project_dir.join("package.json")).ok()?;
    let manifest: serde_json::Value = serde_json::from_str(&manifest).ok()?;
    let body = manifest.get("scripts")?.get(script)?.as_str()?;
    let script_argv = Argv::parse(body)?;
    let start = js_runner(&script_argv)?;
    // A script that already names paths would run them as well.
    if !script_argv.positionals(start, JS_VALUE_FLAGS).is_empty() {
        return None;
    }
    if selected.is_empty() {
        return plan(Vec::new());
    }
    let mut add = vec!["--".to_string()];
    add.extend(selected);
    plan(vec![argv.render(&[], &add)])
}

const GO_VALUE_FLAGS: &[&str] = &[
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

/// `go test` over relative package patterns. Selected packages run filtered
/// to the selected test functions with `-run`.
pub(super) fn go(command: &str, tests: &[RelatedTest]) -> Option<FilesListPlan> {
    let argv = Argv::parse(command)?;
    let parts = &argv.parts;
    if Path::new(&parts[argv.program]).file_name()?.to_str()? != "go"
        || parts.get(argv.program + 1).map(String::as_str) != Some("test")
    {
        return None;
    }
    let start = argv.program + 2;
    // A command that already filters tests is left alone.
    if parts[start..]
        .iter()
        .any(|p| p == "-run" || p.starts_with("-run=") || p == "--run" || p.starts_with("--run="))
    {
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
    let in_scope = |dir: &str| {
        packages.is_empty() && dir == "."
            || packages.iter().any(|&i| {
                let pattern = parts[i].trim_start_matches("./");
                match pattern.strip_suffix("...") {
                    Some(prefix) => {
                        let prefix = prefix.trim_end_matches('/');
                        prefix.is_empty() || dir == prefix || dir.starts_with(&format!("{prefix}/"))
                    }
                    None => dir == pattern.trim_end_matches('/') || (pattern == "." && dir == "."),
                }
            })
    };
    let mut whole: BTreeSet<String> = BTreeSet::new();
    let mut named: BTreeSet<String> = BTreeSet::new();
    let mut names: BTreeSet<&str> = BTreeSet::new();
    for test in tests {
        let dir = test
            .file
            .parent()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| ".".to_string());
        if !in_scope(&dir) {
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
        commands.push(argv.render(&packages, &whole.into_iter().collect::<Vec<_>>()));
    }
    if !named.is_empty() {
        let pattern = format!("^({})$", names.into_iter().collect::<Vec<_>>().join("|"));
        let mut add = vec!["-run".to_string(), pattern];
        add.extend(named);
        commands.push(argv.render(&packages, &add));
    }
    plan(commands)
}

const PYTEST_VALUE_FLAGS: &[&str] = &[
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
    plan(vec![argv.render(&listed, &keep)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

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
            ["pnpm exec vitest run --config vitest.e2e.config.ts src/a.test.ts"]
        );
        assert_eq!(
            commands(node(dir.path(), "pnpm test", &tests)),
            ["pnpm test -- src/a.test.ts"]
        );
        assert_eq!(node(dir.path(), "pnpm run test:all", &tests), None);
        assert_eq!(node(dir.path(), "pnpm run test:src", &tests), None);
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
