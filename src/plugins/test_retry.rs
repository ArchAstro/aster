//! Narrowing a failed test command to the tests that failed, for
//! `--retry-failed`.
//!
//! Each function recognises its runner in a plain command, like
//! [`super::related_tests`], and returns the commands that rerun only the
//! failing tests with that runner's own selection flags. `None` means the
//! command is not one the function can narrow safely, and the failure
//! stands without a retry.
//!
//! A retry must never pass for a reason other than the failing tests now
//! passing, so a command that stops at its first failure (`-x`, `--bail`,
//! `--max-failures`, `go test -failfast`, `cargo test` without
//! `--no-fail-fast`) is not retried: the tests it never reached are unknown.
//!
//! Gradle and Maven are not retried.

use std::collections::BTreeMap;
use std::path::Path;

use super::related_tests::{
    js_runner, js_script, Argv, Delegated, GO_VALUE_FLAGS, JS_VALUE_FLAGS, PYTEST_VALUE_FLAGS,
};
use super::test_reports::{js_runner_kind, JsRunner};
use crate::test_results::TestCase;

/// The commands that rerun `failures`, a run of `command` in `working_dir`.
/// Several commands come back when the runner takes one selection per
/// package (`go test`).
pub(crate) fn commands(
    working_dir: &Path,
    command: &str,
    failures: &[&TestCase],
) -> Option<Vec<String>> {
    if failures.is_empty() {
        return None;
    }
    if let Some(found) = direct(working_dir, command, failures) {
        return Some(found);
    }
    // A shard project's `bash -c 'cd ../core && MIX_TEST_PARTITION=1 exec mix test'`.
    let delegated = Delegated::parse_allowing_expansions(command)?;
    let inner_dir = match &delegated.dir {
        Some(dir) => working_dir.join(dir),
        None => working_dir.to_path_buf(),
    };
    // A partition's retry runs the failing files, which are not that
    // partition's to deal out.
    let delegated = delegated.without_assignment("MIX_TEST_PARTITION");
    let found = direct(&inner_dir, &delegated.command, failures)?;
    Some(found.iter().map(|c| delegated.render(c)).collect())
}

fn direct(working_dir: &Path, command: &str, failures: &[&TestCase]) -> Option<Vec<String>> {
    mix(command, failures)
        .or_else(|| go(command, failures))
        .or_else(|| pytest(working_dir, command, failures))
        .or_else(|| js(working_dir, command, failures))
        .or_else(|| cargo(command, failures))
}

/// Escape the characters that are special in JavaScript and Go (RE2)
/// regular expressions.
pub(crate) fn escape_regex(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if "\\^$.*+?()[]{}|".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `^(a|b)$`, or `^a$` for one name.
fn anchored_alternation(names: &[String]) -> String {
    match names {
        [one] => format!("^{}$", escape_regex(one)),
        _ => format!(
            "^({})$",
            names
                .iter()
                .map(|n| escape_regex(n))
                .collect::<Vec<_>>()
                .join("|")
        ),
    }
}

/// The failing tests' files, sorted and unique. `None` when a failure does
/// not name its file.
fn files_of(failures: &[&TestCase]) -> Option<Vec<String>> {
    let mut files = Vec::new();
    for failure in failures {
        let file = failure.file.as_deref().filter(|f| !f.is_empty())?;
        files.push(file.replace('\\', "/"));
    }
    files.sort();
    files.dedup();
    Some(files)
}

/// Indexes from `from` of the arguments that spell any of `flags`: the flag
/// itself, its value when `takes_value`, and `flag=value`.
fn flag_indexes(parts: &[String], from: usize, flags: &[&str], takes_value: bool) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = from;
    while i < parts.len() {
        let part = parts[i].as_str();
        if flags.contains(&part) {
            out.push(i);
            if takes_value && i + 1 < parts.len() {
                out.push(i + 1);
                i += 1;
            }
        } else if flags
            .iter()
            .any(|f| f.starts_with("--") && part.starts_with(&format!("{f}=")))
        {
            out.push(i);
        }
        i += 1;
    }
    out
}

fn has_flag(parts: &[String], from: usize, flags: &[&str]) -> bool {
    !flag_indexes(parts, from, flags, false).is_empty()
}

const MIX_VALUE_FLAGS: &[&str] = &[
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
];

/// `mix test`: reruns the failing test files. The ExUnit report names each
/// test's file but not its line, so every test in those files runs again;
/// the ones that passed are not recorded as retried.
///
/// `--partitions` and `MIX_TEST_PARTITION` are dropped: partition K of the
/// failing files would skip most of them.
fn mix(command: &str, failures: &[&TestCase]) -> Option<Vec<String>> {
    let argv = Argv::parse(command)?;
    if argv.parts[argv.program] != "mix" {
        return None;
    }
    let test = argv.program
        + 1
        + argv.parts[argv.program + 1..]
            .iter()
            .position(|p| p == "test")?;
    if argv.parts.iter().any(|p| p.ends_with(',')) {
        return None;
    }
    if has_flag(&argv.parts, test + 1, &["--max-failures"]) {
        return None;
    }
    let files = files_of(failures)?;
    let mut drop: Vec<usize> = argv.positionals(test + 1, MIX_VALUE_FLAGS);
    drop.extend(flag_indexes(&argv.parts, test + 1, &["--partitions"], true));
    drop.extend(flag_indexes(
        &argv.parts,
        test + 1,
        &["--failed", "--stale"],
        false,
    ));
    drop.extend((0..argv.program).filter(|&i| argv.parts[i].starts_with("MIX_TEST_PARTITION=")));
    Some(vec![argv.render(&drop, &files)])
}

/// `go test`: `-run` per package. A subtest is anchored level by level
/// (`^TestA$/^sub$`); a failing parent whose subtests failed is replaced by
/// them.
fn go(command: &str, failures: &[&TestCase]) -> Option<Vec<String>> {
    let argv = Argv::parse(command)?;
    let parts = &argv.parts;
    if Path::new(&parts[argv.program]).file_name()?.to_str()? != "go"
        || parts.get(argv.program + 1).map(String::as_str) != Some("test")
    {
        return None;
    }
    let start = argv.program + 2;
    if has_flag(
        parts,
        start,
        &["-args", "--args", "-failfast", "--failfast"],
    ) || parts[start..]
        .iter()
        .any(|p| p == "-failfast=true" || p == "--failfast=true")
    {
        return None;
    }
    let mut value_flags: Vec<&str> = GO_VALUE_FLAGS.to_vec();
    value_flags.extend(["-run", "--run"]);
    let mut drop = argv.positionals(start, &value_flags);
    drop.extend(flag_indexes(parts, start, &["-run", "--run"], true));
    // `flag_indexes` only knows the double-dash spelling of `=` forms.
    drop.extend((start..parts.len()).filter(|&i| parts[i].starts_with("-run=")));

    let mut by_package: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for failure in failures {
        let package = failure.suite.as_deref().filter(|s| !s.is_empty())?;
        by_package.entry(package).or_default().push(&failure.name);
    }
    let mut commands = Vec::new();
    for (package, names) in by_package {
        for pattern in go_patterns(&names) {
            let add = ["-run".to_string(), pattern, package.to_string()];
            commands.push(argv.render(&drop, &add));
        }
    }
    Some(commands)
}

/// `-run` patterns that select exactly `names` within one package. Plain
/// top-level tests share one pattern; subtests of one parent and depth
/// share another.
fn go_patterns(names: &[&str]) -> Vec<String> {
    // A parent that failed because a subtest did is covered by the subtest.
    let leaves: Vec<&str> = names
        .iter()
        .copied()
        .filter(|name| {
            !names
                .iter()
                .any(|other| other.starts_with(&format!("{name}/")))
        })
        .collect();
    let mut top_level: Vec<String> = Vec::new();
    let mut nested: BTreeMap<(String, usize), Vec<Vec<&str>>> = BTreeMap::new();
    for leaf in leaves {
        let path: Vec<&str> = leaf.split('/').collect();
        if let [only] = path.as_slice() {
            if !top_level.contains(&only.to_string()) {
                top_level.push(only.to_string());
            }
        } else {
            nested
                .entry((path[0].to_string(), path.len()))
                .or_default()
                .push(path);
        }
    }
    let mut patterns = Vec::new();
    if !top_level.is_empty() {
        patterns.push(anchored_alternation(&top_level));
    }
    for ((top, depth), paths) in nested {
        let mut levels = vec![anchored_alternation(&[top])];
        for level in 1..depth {
            let mut elements: Vec<String> = Vec::new();
            for path in &paths {
                if !elements.contains(&path[level].to_string()) {
                    elements.push(path[level].to_string());
                }
            }
            levels.push(anchored_alternation(&elements));
        }
        patterns.push(levels.join("/"));
    }
    patterns
}

/// `pytest`: node ids, `file::Class::test`. A report without `file` names
/// the test by its dotted `classname`, which is resolved against the files
/// on disk.
fn pytest(working_dir: &Path, command: &str, failures: &[&TestCase]) -> Option<Vec<String>> {
    let argv = Argv::parse(command)?;
    let runner = argv.parts.iter().position(|p| {
        let name = Path::new(p)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(p);
        name == "pytest" || name == "py.test"
    })?;
    if has_flag(
        &argv.parts,
        runner + 1,
        &["-x", "--exitfirst", "--maxfail", "--sw", "--stepwise"],
    ) {
        return None;
    }
    let mut ids = Vec::new();
    for failure in failures {
        let id = pytest_node_id(working_dir, failure)?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids.sort();
    let drop: Vec<usize> = argv
        .positionals(runner + 1, PYTEST_VALUE_FLAGS)
        .into_iter()
        .filter(|&i| {
            let path = argv.parts[i].split("::").next().unwrap_or("");
            working_dir.join(path).exists()
        })
        .collect();
    Some(vec![argv.render(&drop, &ids)])
}

fn pytest_node_id(working_dir: &Path, case: &TestCase) -> Option<String> {
    let classname = case.suite.as_deref().unwrap_or("");
    let (file, classes): (String, Vec<&str>) = match case.file.as_deref().filter(|f| !f.is_empty())
    {
        Some(file) => {
            let file = file.replace('\\', "/");
            let module = file.trim_end_matches(".py").replace('/', ".");
            let rest = classname.strip_prefix(&module).unwrap_or("");
            (
                file,
                rest.split('.')
                    .filter(|c| !c.is_empty())
                    .collect::<Vec<_>>(),
            )
        }
        None => {
            let parts: Vec<&str> = classname.split('.').filter(|p| !p.is_empty()).collect();
            let (end, path) = (1..=parts.len()).rev().find_map(|end| {
                let path = format!("{}.py", parts[..end].join("/"));
                working_dir.join(&path).is_file().then_some((end, path))
            })?;
            (path, parts[end..].to_vec())
        }
    };
    let mut id = file;
    for class in classes {
        id.push_str("::");
        id.push_str(class);
    }
    id.push_str("::");
    id.push_str(&case.name);
    Some(id)
}

const NODE_VALUE_FLAGS: &[&str] = &[
    "--test-concurrency",
    "--test-timeout",
    "--test-reporter",
    "--test-reporter-destination",
    "--test-shard",
    "--test-name-pattern",
    "--test-skip-pattern",
    "--import",
    "--require",
    "-r",
    "--loader",
    "--env-file",
    "--conditions",
    "-C",
];

/// Vitest, Jest, `bun test` and `node --test`, run directly or through a
/// package script that is nothing but the runner: the failing files, and
/// for the first three a `-t` name pattern.
///
/// Vitest and Jest match `-t` against the describe names and the test
/// title joined by spaces, so `outer > inner > title` becomes
/// `outer inner title`. The pattern is anchored at the end only: Vitest
/// puts the file in front of the full name in some versions. Node matches
/// each test's own name, and takes one `--test-name-pattern` per name.
fn js(working_dir: &Path, command: &str, failures: &[&TestCase]) -> Option<Vec<String>> {
    let argv = Argv::parse(command)?;
    if let Some(start) = js_runner(&argv) {
        let (drop, add) = js_selection(working_dir, &argv, start, failures, &[])?;
        return Some(vec![argv.render(&drop, &add)]);
    }
    let script = js_script(&argv)?;
    let manifest = std::fs::read_to_string(working_dir.join("package.json")).ok()?;
    let manifest: serde_json::Value = serde_json::from_str(&manifest).ok()?;
    let body = manifest.get("scripts")?.get(script)?.as_str()?;
    let script_argv = Argv::parse(body)?;
    let start = js_runner(&script_argv)?;
    // A script that already names paths would run them as well. Other
    // positionals, such as `vitest run`'s subcommand, select nothing.
    if script_argv
        .positionals(start, JS_VALUE_FLAGS)
        .into_iter()
        .any(|i| working_dir.join(&script_argv.parts[i]).exists())
    {
        return None;
    }
    let launcher = Path::new(&argv.parts[argv.program])
        .file_name()?
        .to_str()?
        .to_string();
    // npm passes a script its arguments only after `--`; the others pass
    // them as they are.
    let leading = if launcher == "npm" {
        vec!["--".to_string()]
    } else {
        Vec::new()
    };
    // The script's own runner flags decide what can be narrowed.
    let (_, add) = js_selection(working_dir, &script_argv, start, failures, &leading)?;
    Some(vec![argv.render(&[], &add)])
}

/// The arguments of `argv` to drop (listed test paths and name patterns)
/// and those to add to select `failures`. `leading` goes before the added
/// arguments.
fn js_selection(
    working_dir: &Path,
    argv: &Argv,
    start: usize,
    failures: &[&TestCase],
    leading: &[String],
) -> Option<(Vec<usize>, Vec<String>)> {
    let kind = js_runner_kind(argv, start);
    let node = kind.is_none()
        && argv.parts[argv.program..start].iter().any(|p| {
            matches!(
                Path::new(p).file_name().and_then(|n| n.to_str()),
                Some("node" | "tsx")
            )
        });
    if kind.is_none() && !node {
        return None;
    }
    if has_flag(&argv.parts, start, &["--bail", "-b"]) {
        return None;
    }
    let files = files_of(failures)?;
    // A failure named after its file is a file that did not load, which no
    // name selects.
    let whole_files = failures
        .iter()
        .any(|f| f.file.as_deref() == Some(f.name.as_str()));

    let mut value_flags: Vec<&str> = JS_VALUE_FLAGS.to_vec();
    if node {
        value_flags.extend(NODE_VALUE_FLAGS);
    }
    let mut drop: Vec<usize> = argv
        .positionals(start, &value_flags)
        .into_iter()
        .filter(|&i| working_dir.join(&argv.parts[i]).exists())
        .collect();
    let name_flags: &[&str] = if node {
        &["--test-name-pattern"]
    } else {
        &["-t", "--testNamePattern", "--test-name-pattern"]
    };
    drop.extend(flag_indexes(&argv.parts, start, name_flags, true));

    let mut add: Vec<String> = leading.to_vec();
    if node {
        if !whole_files {
            let mut names: Vec<&str> = failures
                .iter()
                .map(|f| f.name.rsplit(" > ").next().unwrap_or(&f.name))
                .collect();
            names.sort_unstable();
            names.dedup();
            for name in names {
                add.push(format!(
                    "--test-name-pattern=(?:^|\\s){}$",
                    escape_regex(name)
                ));
            }
        }
        add.extend(files);
    } else {
        add.extend(files);
        if !whole_files {
            let mut names: Vec<String> = failures
                .iter()
                .map(|f| match kind {
                    Some(JsRunner::Bun) => f.name.clone(),
                    _ => f.name.replace(" > ", " "),
                })
                .collect();
            names.sort();
            names.dedup();
            let pattern = names
                .iter()
                .map(|n| format!("{}$", escape_regex(n)))
                .collect::<Vec<_>>()
                .join("|");
            add.push("-t".to_string());
            add.push(if names.len() > 1 {
                format!("({pattern})")
            } else {
                pattern
            });
        }
    }
    Some((drop, add))
}

const CARGO_VALUE_FLAGS: &[&str] = &[
    "-p",
    "--package",
    "--exclude",
    "-F",
    "--features",
    "--target",
    "--test",
    "--bin",
    "--example",
    "--bench",
    "--manifest-path",
    "-j",
    "--jobs",
    "--profile",
    "--color",
    "--target-dir",
    "--message-format",
    "--config",
    "-Z",
    // nextest
    "-E",
    "--filterset",
    "-P",
    "--test-threads",
    "--retries",
    "--cargo-profile",
    "--build-jobs",
    "--run-ignored",
    "--partition",
    "--config-file",
    "--user-config-file",
    "--archive-file",
];
const LIBTEST_VALUE_FLAGS: &[&str] = &[
    "--test-threads",
    "--skip",
    "--format",
    "--color",
    "--logfile",
    "--shuffle-seed",
    "-Z",
];

/// `cargo test` and `cargo nextest run`, only with `--no-fail-fast`: without
/// it the run stops at the first failing test binary and the tests after
/// it are unknown.
///
/// - `cargo test` reruns each failing test with libtest's `--exact` in
///   every test binary the original selected. A test of the same name in
///   another binary runs too.
/// - nextest selects with a filterset, `-E 'test(=a) | test(=b)'`, and only
///   for names made of identifier characters and `::`.
fn cargo(command: &str, failures: &[&TestCase]) -> Option<Vec<String>> {
    let argv = Argv::parse(command)?;
    let parts = &argv.parts;
    // Rebuilt below from parts of the command, which loses expansions.
    if Path::new(&parts[argv.program]).file_name()?.to_str()? != "cargo"
        || !argv.expanding.is_empty()
    {
        return None;
    }
    let mut i = argv.program + 1;
    while parts.get(i).is_some_and(|p| p.starts_with('+')) {
        i += 1;
    }
    let (nextest, args_start) = match (
        parts.get(i).map(String::as_str),
        parts.get(i + 1).map(String::as_str),
    ) {
        (Some("test"), _) => (false, i + 1),
        (Some("nextest"), Some("run")) => (true, i + 2),
        _ => return None,
    };
    let separator = parts[args_start..]
        .iter()
        .position(|p| p == "--")
        .map_or(parts.len(), |at| args_start + at);
    if !parts[args_start..separator]
        .iter()
        .any(|p| p == "--no-fail-fast")
    {
        return None;
    }

    // Everything before `--` except the positional name filters.
    let cargo_side = Argv {
        parts: parts[..separator].to_vec(),
        program: argv.program,
        expanding: Vec::new(),
    };
    let mut drop = cargo_side.positionals(args_start, CARGO_VALUE_FLAGS);
    let mut names: Vec<&str> = failures.iter().map(|f| f.name.as_str()).collect();
    names.sort_unstable();
    names.dedup();

    if nextest {
        if names.iter().any(|n| {
            n.is_empty()
                || !n
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
        }) {
            return None;
        }
        drop.extend(flag_indexes(
            &cargo_side.parts,
            args_start,
            &["-E", "--filterset"],
            true,
        ));
        // Arguments after `--` go to the test binaries.
        let tail: Vec<String> = parts[separator..].to_vec();
        let expression = names
            .iter()
            .map(|n| format!("test(={n})"))
            .collect::<Vec<_>>()
            .join(" | ");
        let mut add = vec!["-E".to_string(), expression];
        add.extend(tail);
        return Some(vec![cargo_side.render(&drop, &add)]);
    }

    // libtest's own arguments: keep its flags, replace its name filters.
    let tail = parts.get(separator + 1..).unwrap_or(&[]);
    let tail_argv = Argv {
        parts: tail.to_vec(),
        program: 0,
        expanding: Vec::new(),
    };
    let mut skip = tail_argv.positionals(0, LIBTEST_VALUE_FLAGS);
    skip.extend(flag_indexes(tail, 0, &["--exact"], false));
    let mut add = vec!["--".to_string()];
    add.extend(
        tail.iter()
            .enumerate()
            .filter(|(i, _)| !skip.contains(i))
            .map(|(_, p)| p.clone()),
    );
    add.push("--exact".to_string());
    add.extend(names.iter().map(|n| n.to_string()));
    Some(vec![cargo_side.render(&drop, &add)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_results::TestOutcome;
    use tempfile::TempDir;

    fn failure(file: Option<&str>, suite: Option<&str>, name: &str) -> TestCase {
        TestCase {
            file: file.map(str::to_string),
            suite: suite.map(str::to_string),
            name: name.to_string(),
            outcome: TestOutcome::Fail,
            attempt: 1,
            duration_ms: None,
            message: None,
        }
    }

    fn build(dir: &Path, command: &str, failures: &[TestCase]) -> Option<Vec<String>> {
        let refs: Vec<&TestCase> = failures.iter().collect();
        commands(dir, command, &refs)
    }

    fn one(dir: &Path, command: &str, failures: &[TestCase]) -> String {
        let mut found = build(dir, command, failures).expect("a retry command");
        assert_eq!(found.len(), 1, "{found:?}");
        found.remove(0)
    }

    #[test]
    fn escapes_regex_metacharacters() {
        assert_eq!(
            escape_regex("a.b (c) [d] {e} $f ^g |h \\i *+?"),
            "a\\.b \\(c\\) \\[d\\] \\{e\\} \\$f \\^g \\|h \\\\i \\*\\+\\?"
        );
        assert_eq!(escape_regex("plain name"), "plain name");
    }

    #[test]
    fn mix_reruns_failing_files_without_partitions() {
        let dir = Path::new("/w");
        let failures = [
            failure(Some("test/b_test.exs"), Some("BTest"), "test two"),
            failure(Some("test/a_test.exs"), Some("ATest"), "test one"),
            failure(Some("test/a_test.exs"), Some("ATest"), "test three"),
        ];
        assert_eq!(
            one(dir, "mix test", &failures),
            "mix test test/a_test.exs test/b_test.exs"
        );
        assert_eq!(
            one(
                dir,
                "MIX_TEST_PARTITION=2 mix test --partitions 3 --warnings-as-errors test/x_test.exs",
                &failures
            ),
            "mix test --warnings-as-errors test/a_test.exs test/b_test.exs"
        );
        assert_eq!(
            one(dir, "mix test --failed --partitions=2 --seed 0", &failures),
            "mix test --seed 0 test/a_test.exs test/b_test.exs"
        );
    }

    #[test]
    fn mix_is_left_alone_when_it_cannot_be_narrowed_safely() {
        let dir = Path::new("/w");
        let failures = [failure(Some("test/a_test.exs"), None, "test one")];
        assert_eq!(build(dir, "mix test --max-failures 1", &failures), None);
        assert_eq!(build(dir, "mix do compile, test", &failures), None);
        assert_eq!(
            build(dir, "mix test", &[failure(None, None, "test one")]),
            None
        );
    }

    #[test]
    fn mix_inside_a_shard_one_liner_drops_the_partition() {
        let dir = Path::new("/w/shard");
        let failures = [failure(Some("test/a_test.exs"), None, "test one")];
        let found = one(
            dir,
            "bash -c 'cd ../core && MIX_TEST_PARTITION=1 exec mix test --partitions 2'",
            &failures,
        );
        let script = shell_words::split(&found).unwrap().pop().unwrap();
        assert_eq!(script, "cd ../core && exec mix test test/a_test.exs");
    }

    #[test]
    fn vitest_in_a_one_liner_with_expansions_keeps_them_and_its_own_report() {
        let dir = Path::new("/w/web");
        let failures = [failure(Some("e2e/a.e2e.test.ts"), None, "flaky one")];
        let found = one(
            dir,
            "bash -c 'vitest run ${FILE:-e2e} --reporter=junit --outputFile.junit=\"${OUT:-/tmp/r.xml}\"'",
            &failures,
        );
        let script = shell_words::split(&found).unwrap().pop().unwrap();
        // The retry reads the command's own report again, so it keeps the
        // reporter flags and the expansions as written.
        assert_eq!(
            script,
            "vitest run ${FILE:-e2e} --reporter=junit --outputFile.junit=\"${OUT:-/tmp/r.xml}\" \
             e2e/a.e2e.test.ts -t 'flaky one$'"
        );
    }

    #[test]
    fn mix_retries_a_balanced_partition_with_only_its_failing_files() {
        let dir = Path::new("/w/shard");
        let original =
            "bash -c 'cd ../core && MIX_TEST_PARTITION=2 exec mix test --seed 0 --partitions 4'";
        let balanced = crate::plugins::mix_partitions::parse(dir, original)
            .unwrap()
            .render(&["test/a_test.exs".into(), "test/b_test.exs".into()]);
        let failures = [failure(Some("test/b_test.exs"), None, "test one")];
        let found = one(dir, &balanced, &failures);
        let script = shell_words::split(&found).unwrap().pop().unwrap();
        assert_eq!(
            script,
            "cd ../core && exec mix test --seed 0 test/b_test.exs"
        );
    }

    #[test]
    fn go_selects_tests_per_package() {
        let dir = Path::new("/w");
        let failures = [
            failure(None, Some("example.com/m/b"), "TestB"),
            failure(None, Some("example.com/m/a"), "TestA"),
            failure(None, Some("example.com/m/a"), "TestC"),
        ];
        assert_eq!(
            build(dir, "go test -count=1 ./...", &failures).unwrap(),
            vec![
                "go test -count=1 -run '^(TestA|TestC)$' example.com/m/a",
                "go test -count=1 -run '^TestB$' example.com/m/b",
            ]
        );
    }

    #[test]
    fn go_anchors_subtests_level_by_level_and_replaces_failed_parents() {
        let dir = Path::new("/w");
        let failures = [
            failure(None, Some("m/p"), "TestTable"),
            failure(None, Some("m/p"), "TestTable/case_a"),
            failure(None, Some("m/p"), "TestTable/case_b.c"),
            failure(None, Some("m/p"), "TestPlain"),
        ];
        assert_eq!(
            build(dir, "go test -v -run Old ./...", &failures).unwrap(),
            vec![
                "go test -v -run '^TestPlain$' m/p",
                "go test -v -run '^TestTable$/^(case_a|case_b\\.c)$' m/p",
            ]
        );
    }

    #[test]
    fn go_stops_at_first_failure_runs_are_not_retried() {
        let failures = [failure(None, Some("m/p"), "TestA")];
        let dir = Path::new("/w");
        assert_eq!(build(dir, "go test -failfast ./...", &failures), None);
        assert_eq!(build(dir, "go test ./... -args -x", &failures), None);
        assert_eq!(build(dir, "go build ./...", &failures), None);
    }

    #[test]
    fn pytest_builds_node_ids() {
        let dir = Path::new("/w");
        let failures = [
            failure(
                Some("tests/test_api.py"),
                Some("tests.test_api.TestUsers"),
                "test_create[a b]",
            ),
            failure(
                Some("tests/test_api.py"),
                Some("tests.test_api"),
                "test_top",
            ),
        ];
        assert_eq!(
            build(dir, "pytest -q", &failures).unwrap(),
            vec!["pytest -q 'tests/test_api.py::TestUsers::test_create[a b]' tests/test_api.py::test_top"]
        );
        assert_eq!(
            build(dir, "pytest -x", &failures),
            None,
            "-x leaves later tests unrun"
        );
    }

    #[test]
    fn pytest_resolves_a_classname_against_the_files() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("tests/unit")).unwrap();
        std::fs::write(tmp.path().join("tests/unit/test_a.py"), "").unwrap();
        let failures = [failure(
            None,
            Some("tests.unit.test_a.TestOuter.TestInner"),
            "test_x",
        )];
        assert_eq!(
            one(tmp.path(), "uv run pytest tests/unit --tb=short", &failures),
            "uv run pytest --tb=short tests/unit/test_a.py::TestOuter::TestInner::test_x"
        );
        let unknown = [failure(None, Some("nowhere.test_b"), "test_x")];
        assert_eq!(build(tmp.path(), "pytest", &unknown), None);
    }

    #[test]
    fn vitest_and_jest_select_files_and_an_escaped_name_pattern() {
        let dir = Path::new("/w");
        let failures = [
            failure(
                Some("src/math.test.ts"),
                None,
                "math > adds (1+1) [x] > returns 2",
            ),
            failure(Some("src/other.test.ts"), None, "top-level $ test"),
        ];
        assert_eq!(
            one(dir, "vitest run --coverage", &failures),
            "vitest run --coverage src/math.test.ts src/other.test.ts -t \
             '(math adds \\(1\\+1\\) \\[x\\] returns 2$|top-level \\$ test$)'"
        );
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/old.test.ts"), "").unwrap();
        assert_eq!(
            one(
                tmp.path(),
                "npx jest -t old src/old.test.ts",
                &failures[..1]
            ),
            "npx jest src/math.test.ts -t 'math adds \\(1\\+1\\) \\[x\\] returns 2$'"
        );
        assert_eq!(build(dir, "vitest run --bail 1", &failures), None);
    }

    #[test]
    fn bun_matches_the_test_name_alone() {
        let dir = Path::new("/w");
        let failures = [failure(Some("a.test.ts"), Some("math"), "adds up")];
        assert_eq!(
            one(dir, "bun test --timeout 5000", &failures),
            "bun test --timeout 5000 a.test.ts -t 'adds up$'"
        );
    }

    #[test]
    fn a_file_that_did_not_load_reruns_whole() {
        let dir = Path::new("/w");
        let failures = [failure(
            Some("src/broken.test.ts"),
            None,
            "src/broken.test.ts",
        )];
        assert_eq!(
            one(dir, "vitest run", &failures),
            "vitest run src/broken.test.ts"
        );
    }

    #[test]
    fn package_scripts_that_are_just_the_runner_get_the_selection() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"scripts": {"test": "vitest run", "ci": "pnpm lint && vitest run"}}"#,
        )
        .unwrap();
        let failures = [failure(Some("a.test.ts"), None, "suite > case")];
        assert_eq!(
            one(tmp.path(), "pnpm test", &failures),
            "pnpm test a.test.ts -t 'suite case$'"
        );
        assert_eq!(
            one(tmp.path(), "npm run test", &failures),
            "npm run test -- a.test.ts -t 'suite case$'"
        );
        assert_eq!(
            one(tmp.path(), "npm test", &failures),
            "npm test -- a.test.ts -t 'suite case$'"
        );
        assert_eq!(build(tmp.path(), "pnpm ci", &failures), None);
    }

    #[test]
    fn node_test_selects_names_and_files() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("tests")).unwrap();
        std::fs::write(tmp.path().join("tests/old.test.mjs"), "").unwrap();
        let dir = tmp.path();
        let failures = [
            failure(Some("tests/a.test.mjs"), None, "outer > it (works)"),
            failure(Some("tests/a.test.mjs"), None, "second"),
        ];
        assert_eq!(
            one(
                dir,
                "node --test --test-name-pattern=old tests/old.test.mjs",
                &failures
            ),
            "node --test '--test-name-pattern=(?:^|\\s)it \\(works\\)$' \
             '--test-name-pattern=(?:^|\\s)second$' tests/a.test.mjs"
        );
        // A chained package script is not a direct `node --test`.
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"scripts": {"test": "pnpm lint && node --test"}}"#,
        )
        .unwrap();
        assert_eq!(build(tmp.path(), "pnpm test", &failures), None);
    }

    #[test]
    fn cargo_test_reruns_names_exactly_and_needs_no_fail_fast() {
        let dir = Path::new("/w");
        let failures = [
            failure(None, Some("src/lib.rs"), "tests::divides"),
            failure(None, Some("tests/api.rs"), "adds"),
        ];
        assert_eq!(
            one(
                dir,
                "cargo test --workspace --no-fail-fast old_filter -- --nocapture --skip slow stale",
                &failures
            ),
            "cargo test --workspace --no-fail-fast -- --nocapture --skip slow --exact adds tests::divides"
        );
        assert_eq!(build(dir, "cargo test --workspace", &failures), None);
        assert_eq!(build(dir, "cargo build --no-fail-fast", &failures), None);
    }

    #[test]
    fn nextest_selects_with_a_filterset() {
        let dir = Path::new("/w");
        let failures = [
            failure(None, Some("demo"), "tests::divides"),
            failure(None, Some("demo"), "adds"),
        ];
        assert_eq!(
            one(
                dir,
                "cargo nextest run --no-fail-fast -E 'test(old)' -p demo",
                &failures
            ),
            "cargo nextest run --no-fail-fast -p demo -E 'test(=adds) | test(=tests::divides)'"
        );
        let odd = [failure(None, Some("demo"), "src/lib.rs - add (line 3)")];
        assert_eq!(build(dir, "cargo nextest run --no-fail-fast", &odd), None);
    }

    #[test]
    fn unknown_commands_and_no_failures_give_nothing() {
        let dir = Path::new("/w");
        assert_eq!(
            build(dir, "make test", &[failure(Some("a"), None, "t")]),
            None
        );
        assert_eq!(build(dir, "mix test", &[]), None);
    }
}
