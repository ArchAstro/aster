//! Parse target command strings into direct process invocations.
//!
//! Aster intentionally does not invoke a shell. Shell-style quoting and escaping
//! are accepted. Unquoted shell operators such as `&&`, pipes and redirects
//! would reach the program as ordinary arguments, so configuration loading
//! rejects them with [`find_unquoted_shell_operator`].

use anyhow::{anyhow, Context, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedCommand {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

pub(crate) fn parse_command(command: &str) -> Result<ParsedCommand> {
    let parts = shell_words::split(command)
        .with_context(|| format!("invalid command quoting: {command}"))?;

    if parts.is_empty() {
        return Err(anyhow!("empty command"));
    }

    let mut env = Vec::new();
    let mut command_start = 0;

    for (index, part) in parts.iter().enumerate() {
        let Some((name, value)) = part.split_once('=') else {
            break;
        };
        if !is_environment_name(name) {
            break;
        }
        env.push((name.to_string(), value.to_string()));
        command_start = index + 1;
    }

    if command_start == parts.len() {
        return Err(anyhow!("empty command (only environment variables)"));
    }

    Ok(ParsedCommand {
        program: parts[command_start].clone(),
        args: parts[command_start + 1..].to_vec(),
        env,
    })
}

/// Returns the first word of `command` that contains an unquoted, unescaped
/// shell control or redirection character (`|`, `&`, `;`, `<`, `>`) or an
/// unquoted command substitution (`$(` or a backtick), as written.
///
/// A shell would interpret such a word; Aster passes it to the program as a
/// literal argument. Operators inside single or double quotes, or escaped
/// with a backslash, are deliberate literals and are not reported, so
/// `bash -c 'a && b'` and `grep '|'` are accepted.
pub(crate) fn find_unquoted_shell_operator(command: &str) -> Option<&str> {
    let mut chars = command.char_indices().peekable();
    let mut word_start: Option<usize> = None;
    let mut word_has_operator = false;
    let mut quote: Option<char> = None;

    while let Some((index, c)) = chars.next() {
        if let Some(open) = quote {
            match c {
                '\\' if open == '"' => {
                    chars.next();
                }
                c if c == open => quote = None,
                _ => {}
            }
            continue;
        }

        if c.is_whitespace() {
            if let Some(start) = word_start.take() {
                if word_has_operator {
                    return Some(&command[start..index]);
                }
            }
            word_has_operator = false;
            continue;
        }

        word_start.get_or_insert(index);
        match c {
            '\\' => {
                chars.next();
            }
            '\'' | '"' => quote = Some(c),
            '|' | '&' | ';' | '<' | '>' | '`' => word_has_operator = true,
            '$' if chars.peek().is_some_and(|(_, next)| *next == '(') => {
                word_has_operator = true;
            }
            _ => {}
        }
    }

    match word_start {
        Some(start) if word_has_operator => Some(&command[start..]),
        _ => None,
    }
}

pub(crate) fn quote_argument(value: &str) -> String {
    shell_words::quote(value).into_owned()
}

fn is_environment_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_quoted_arguments_and_environment() {
        assert_eq!(
            parse_command("MODE='hello world' tool --name \"two words\"").unwrap(),
            ParsedCommand {
                program: "tool".to_string(),
                args: vec!["--name".to_string(), "two words".to_string()],
                env: vec![("MODE".to_string(), "hello world".to_string())],
            }
        );
    }

    #[test]
    fn rejects_invalid_quoting() {
        let error = parse_command("tool 'unfinished").unwrap_err();
        assert!(error.to_string().contains("invalid command quoting"));
    }

    #[test]
    fn rejects_environment_without_program() {
        let error = parse_command("MODE=test").unwrap_err();
        assert!(error.to_string().contains("only environment variables"));
    }

    #[test]
    fn finds_standalone_shell_operators() {
        for (command, token) in [
            ("a && b", "&&"),
            ("a || b", "||"),
            ("a | b", "|"),
            ("a |& b", "|&"),
            ("a ; b", ";"),
            ("server &", "&"),
            ("a > out.txt", ">"),
            ("a >> out.txt", ">>"),
            ("a < in.txt", "<"),
            ("a 2> err.txt", "2>"),
            ("a > out.txt 2>&1", ">"),
            ("a 2>&1", "2>&1"),
            ("a &> out.txt", "&>"),
            ("a <<< text", "<<<"),
        ] {
            assert_eq!(
                find_unquoted_shell_operator(command),
                Some(token),
                "{command}"
            );
        }
    }

    #[test]
    fn finds_operators_attached_to_words() {
        assert_eq!(find_unquoted_shell_operator("a&&b"), Some("a&&b"));
        assert_eq!(find_unquoted_shell_operator("cd dir; make"), Some("dir;"));
        assert_eq!(
            find_unquoted_shell_operator("tool 2>/dev/null"),
            Some("2>/dev/null")
        );
        assert_eq!(
            find_unquoted_shell_operator("tool >out.txt"),
            Some(">out.txt")
        );
        assert_eq!(find_unquoted_shell_operator("x=1 tool a|b"), Some("a|b"));
    }

    #[test]
    fn finds_unquoted_command_substitution() {
        assert_eq!(
            find_unquoted_shell_operator("tool $(pwd)/bin"),
            Some("$(pwd)/bin")
        );
        assert_eq!(find_unquoted_shell_operator("tool `pwd`"), Some("`pwd`"));
    }

    #[test]
    fn allows_quoted_and_escaped_operators() {
        for command in [
            "bash -c 'a && b'",
            "bash -c \"a && b | c > out\"",
            "sh -c 'count=$(grep -c x f || true); echo $count >> log'",
            "grep '|' file",
            "grep \"|\" file",
            "grep \\| file",
            "echo 'a'\"&&\"'b'",
            "tool --pattern='a|b' --sep=\";\"",
            "bash -c \"echo \\\"&&\\\"\"",
            "MODE=test FOO='a && b' tool --flag",
            "tool $HOME/bin {files}",
            "",
        ] {
            assert_eq!(find_unquoted_shell_operator(command), None, "{command}");
        }
    }

    #[test]
    fn quotes_round_trip() {
        let value = "tests/a file's name.py";
        let command = format!("tool {}", quote_argument(value));
        let parsed = parse_command(&command).unwrap();
        assert_eq!(parsed.args, vec![value]);
    }
}
