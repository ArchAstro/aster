//! Deterministic configuration bug-bash and fuzz regression harness.
//!
//! Keep this test dependency-free and reproducible: failures print the seed and
//! mutation number so a malformed input can be promoted to a focused test.

use aster::config::{parse_aster_toml, WorkspaceConfig};
use std::panic::{catch_unwind, AssertUnwindSafe};
use tempfile::TempDir;

#[derive(Clone, Copy, Debug)]
enum Expected {
    Accept,
    Reject,
}

#[derive(Clone, Copy, Debug)]
struct Scenario {
    name: &'static str,
    input: &'static str,
    expected: Expected,
}

fn parse_both(input: &str) -> (bool, bool) {
    let temp = TempDir::new().expect("create config scenario directory");
    let path = temp.path().join("aster.toml");
    std::fs::write(&path, input).expect("write config scenario");
    (
        parse_aster_toml(&path).is_ok(),
        WorkspaceConfig::load(temp.path()).is_ok(),
    )
}

#[test]
fn configuration_scenario_matrix() {
    let scenarios = [
        Scenario { name: "empty document", input: "", expected: Expected::Accept },
        Scenario { name: "comments only", input: "# aster workspace\n", expected: Expected::Accept },
        Scenario { name: "unicode project name", input: "name = \"服务-🚀\"\n", expected: Expected::Accept },
        Scenario { name: "simple target", input: "[targets]\ntest = \"cargo test\"\n", expected: Expected::Accept },
        Scenario { name: "rich target", input: "[targets.test]\ncommand = \"pytest {files}\"\ncapabilities = [\"files_list\"]\nfiles_glob = \"**/*_test.py\"\nstream = false\ninvalidates_cache = false\nexclusive_resources = [\"python-env\"]\n", expected: Expected::Accept },
        Scenario { name: "alias target", input: "[targets]\ntest = \"cargo test\"\ncheck = { alias = \"test\", depends_on = [\"//self:lint\"] }\n", expected: Expected::Accept },
        Scenario { name: "root dependencies", input: "depends_on = [\"//libs/a:build\", \"//libs/b\"]\n", expected: Expected::Accept },
        Scenario { name: "discovery ignores", input: "ignore = [\"vendor/**\", \"examples/**\"]\n", expected: Expected::Accept },
        Scenario { name: "watch configuration", input: "[watch]\nignore = [\"coverage/**\"]\nsuppress_paths = [\"dist/**\"]\ndebounce_ms = 9223372036854775807\n", expected: Expected::Accept },
        Scenario { name: "affected configuration", input: "[affected]\nignore = [\"docs/**\"]\n", expected: Expected::Accept },
        Scenario { name: "fixed boundary port", input: "[dev.ports]\nhttp = 65535\n", expected: Expected::Accept },
        Scenario { name: "resolved port one env", input: "[dev.ports.http]\nenv = \"PORT\"\ndefault = 4000\n", expected: Expected::Accept },
        Scenario { name: "resolved port many env", input: "[dev.ports.http]\nenv = [\"PORT\", \"HTTP_PORT\"]\nfile_env = []\ndefault = 4000\n", expected: Expected::Accept },
        Scenario { name: "derived port", input: "[dev.ports.base]\ndefault = 4000\n[dev.ports.web]\ndefault = 3000\noffset_from = \"base\"\noffset_base = 4000\nsaturating_offset = true\n", expected: Expected::Accept },
        Scenario { name: "development service", input: "[dev.services.api]\ntarget = \"//api:dev\"\nport = \"http\"\nopen_path = \"/health\"\nenv_files = [\".env\"]\nenv = { PORT = \"{port}\" }\ninherit_env = [\"PATH\"]\norder = -2147483648\n", expected: Expected::Accept },
        Scenario { name: "development service group", input: "[dev.services.api]\ntarget = \"//api:dev\"\n[dev.service_groups]\ncore = [\"api\"]\n", expected: Expected::Accept },
        Scenario { name: "development service group with control port", input: "[dev.ports]\ncore_control = 5001\n[dev.services.api]\ntarget = \"//api:dev\"\n[dev.service_groups]\ncore = { services = [\"api\"], control_port = \"core_control\" }\n", expected: Expected::Accept },
        Scenario { name: "unknown grouped service", input: "[dev.service_groups]\ncore = [\"missing\"]\n", expected: Expected::Reject },
        Scenario { name: "cache configuration", input: "[targets.build]\ncommand = \"cargo build\"\n[targets.build.cache]\nenabled = true\ninclude = [\"src/**\"]\nexclude = [\"**/*.tmp\"]\nenv = [\"RUSTFLAGS\"]\noutputs = [\"target/debug/app\"]\n", expected: Expected::Accept },
        Scenario { name: "quoted dotted target", input: "[targets.\"check:all\"]\ncommand = \"true\"\n", expected: Expected::Accept },
        Scenario { name: "literal multiline command", input: "[targets.run]\ncommand = '''printf 'one\\ntwo' '''\n", expected: Expected::Accept },
        Scenario { name: "unknown top-level key", input: "mystery = true\n", expected: Expected::Reject },
        Scenario { name: "unknown watch key", input: "[watch]\nmystery = true\n", expected: Expected::Reject },
        Scenario { name: "unknown affected key", input: "[affected]\nmystery = true\n", expected: Expected::Reject },
        Scenario { name: "unknown dev key", input: "[dev]\nmystery = true\n", expected: Expected::Reject },
        Scenario { name: "unknown port key", input: "[dev.ports.http]\ndefault = 4000\nmystery = true\n", expected: Expected::Reject },
        Scenario { name: "unknown service key", input: "[dev.services.api]\ntarget = \"//api:dev\"\nmystery = true\n", expected: Expected::Reject },
        Scenario { name: "unknown target key", input: "[targets.test]\ncommand = \"true\"\nmystery = true\n", expected: Expected::Reject },
        Scenario { name: "unknown alias key", input: "[targets.check]\nalias = \"test\"\nmystery = true\n", expected: Expected::Reject },
        Scenario { name: "unknown cache key", input: "[targets.test]\ncommand = \"true\"\n[targets.test.cache]\nmystery = true\n", expected: Expected::Reject },
        Scenario { name: "malformed table", input: "[targets.test\ncommand = \"true\"\n", expected: Expected::Reject },
        Scenario { name: "duplicate key", input: "name = \"a\"\nname = \"b\"\n", expected: Expected::Reject },
        Scenario { name: "wrong scalar type", input: "name = 42\n", expected: Expected::Reject },
        Scenario { name: "wrong list member type", input: "depends_on = [\"//a\", 42]\n", expected: Expected::Reject },
        Scenario { name: "invalid root dependency", input: "depends_on = [\"not-an-address\"]\n", expected: Expected::Reject },
        Scenario { name: "invalid rich target dependency", input: "[targets.test]\ncommand = \"true\"\ndepends_on = [\"lint\"]\n", expected: Expected::Reject },
        Scenario { name: "invalid alias dependency", input: "[targets]\ntest = \"true\"\ncheck = { alias = \"test\", depends_on = [\"lint\"] }\n", expected: Expected::Reject },
        Scenario { name: "unsupported capability", input: "[targets.test]\ncommand = \"true\"\ncapabilities = [\"file_list\"]\n", expected: Expected::Reject },
        Scenario { name: "invalid files glob", input: "[targets.test]\ncommand = \"true\"\nfiles_glob = \"[\"\n", expected: Expected::Reject },
        Scenario { name: "invalid cache glob", input: "[targets.test]\ncommand = \"true\"\n[targets.test.cache]\ninclude = [\"[\"]\n", expected: Expected::Reject },
        Scenario { name: "unterminated string", input: "name = \"aster\n", expected: Expected::Reject },
    ];

    for scenario in scenarios {
        let (project, workspace) = parse_both(scenario.input);
        let expected = matches!(scenario.expected, Expected::Accept);
        assert_eq!(project, expected, "project parser: {}", scenario.name);
        assert_eq!(workspace, expected, "workspace parser: {}", scenario.name);
    }
}

#[test]
fn deterministic_mutation_fuzz_never_panics() {
    const SEEDS: &[&str] = &[
        "",
        "name = \"app\"\n",
        "[targets.test]\ncommand = \"cargo test\"\n",
        "[targets.test.cache]\nenabled = true\ninclude = [\"src/**\"]\n",
        "[watch]\nignore = [\"target/**\"]\ndebounce_ms = 300\n",
        "[dev.ports.http]\nenv = [\"PORT\"]\ndefault = 4000\n",
        "[dev.services.api]\ntarget = \"//api:dev\"\nenv = { PORT = \"{port}\" }\n",
    ];
    const INSERTIONS: &[&str] = &[
        "\0", "[", "]", "{", "}", "=", "\"", "'", "\n", "#", "//", "🚀",
    ];

    let temp = TempDir::new().expect("create fuzz directory");
    let path = temp.path().join("aster.toml");
    let mut state = 0x6a09_e667_f3bc_c909_u64;

    for mutation in 0..10_000 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let seed = SEEDS[state as usize % SEEDS.len()];
        let split = state.rotate_left(19) as usize % (seed.len() + 1);
        let split = (0..=split)
            .rev()
            .find(|index| seed.is_char_boundary(*index))
            .unwrap();
        let insertion = INSERTIONS[state.rotate_left(37) as usize % INSERTIONS.len()];
        let mut input = String::with_capacity(seed.len() + insertion.len());
        input.push_str(&seed[..split]);
        input.push_str(insertion);
        input.push_str(&seed[split..]);
        std::fs::write(&path, input).expect("write fuzz input");

        let result = catch_unwind(AssertUnwindSafe(|| {
            let _ = parse_aster_toml(&path);
            let _ = WorkspaceConfig::load(temp.path());
        }));
        assert!(
            result.is_ok(),
            "configuration parser panicked at mutation {mutation}, state {state:#x}"
        );
    }
}
