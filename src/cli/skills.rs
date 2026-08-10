/// A workspace-independent Markdown reference intended for humans and LLMs.
pub const SKILLS_MARKDOWN: &str = include_str!("skills.md");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guide_covers_core_and_service_workflows() {
        assert!(SKILLS_MARKDOWN.starts_with("# Using Aster\n"));
        for example in [
            "aster list --json",
            "aster lint --all",
            "aster test --all",
            "aster affected test",
            "aster watch",
            "aster services up",
            "aster services logs",
        ] {
            assert!(SKILLS_MARKDOWN.contains(example), "missing {example}");
        }
        assert!(SKILLS_MARKDOWN.ends_with('\n'));
    }
}
