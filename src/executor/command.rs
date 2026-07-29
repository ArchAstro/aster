//! Parse target command strings into direct process invocations.
//!
//! Aster intentionally does not invoke a shell. Shell-style quoting and escaping
//! are accepted, but operators such as pipes and redirects are ordinary arguments.

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
    fn quotes_round_trip() {
        let value = "tests/a file's name.py";
        let command = format!("tool {}", quote_argument(value));
        let parsed = parse_command(&command).unwrap();
        assert_eq!(parsed.args, vec![value]);
    }
}
