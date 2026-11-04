//! The implementation the generated scenarios drive: a model repo laid out as
//! an Elixir project in a scratch git repo, with the real engine answering
//! `select`.

use crate::engine::{Plan, Workspace};
use crate::generated_refinement::Implementation;
use crate::repo::{Changes, Repo};
use quint_refinements::{NormalizedRuntimeEvidence, RuntimeValue};
use std::collections::{BTreeMap, BTreeSet};

/// The model's `state` as the engine's side reports it.
#[derive(Clone)]
pub struct Snapshot {
    state: RuntimeValue,
}

impl NormalizedRuntimeEvidence for Snapshot {
    fn resolve_name(&self, name: &str) -> Result<RuntimeValue, String> {
        match name {
            "state" => Ok(self.state.clone()),
            other => Err(format!("unknown evidence name {other}")),
        }
    }

    fn resolve_call(
        &self,
        _operator: &str,
        _arguments: &[RuntimeValue],
    ) -> Option<Result<RuntimeValue, String>> {
        None
    }
}

pub struct Shop {
    repo: RuntimeValue,
    rules: RuntimeValue,
    changes: RuntimeValue,
    selection: RuntimeValue,
    workspace: Option<Workspace>,
}

fn record(fields: &[(&str, RuntimeValue)]) -> RuntimeValue {
    RuntimeValue::Record(
        fields
            .iter()
            .map(|(name, value)| (name.to_string(), value.clone()))
            .collect::<BTreeMap<_, _>>(),
    )
}

fn no_selection() -> RuntimeValue {
    record(&[
        ("full", RuntimeValue::Bool(false)),
        ("tests", RuntimeValue::Set(BTreeSet::new())),
    ])
}

/// The model normalises a full run to "every test file".
fn selection(plan: &Plan, all_tests: &BTreeSet<i64>) -> Result<RuntimeValue, String> {
    let named = plan.units()?;
    let tests = if plan.full { all_tests } else { &named };
    Ok(record(&[
        ("full", RuntimeValue::Bool(plan.full)),
        (
            "tests",
            RuntimeValue::Set(tests.iter().map(|t| RuntimeValue::Int(*t)).collect()),
        ),
    ]))
}

fn field(value: &RuntimeValue, name: &str) -> Result<RuntimeValue, String> {
    match value {
        RuntimeValue::Record(fields) => fields
            .get(name)
            .cloned()
            .ok_or_else(|| format!("no field {name} in {value:?}")),
        other => Err(format!("expected a record, got {other:?}")),
    }
}

impl Implementation for Shop {
    type Evidence = Snapshot;

    fn from_initial_state(initial: &RuntimeValue) -> Result<Self, String> {
        Ok(Shop {
            repo: field(initial, "repo")?,
            rules: field(initial, "rules")?,
            changes: field(initial, "changes")?,
            selection: field(initial, "selection")?,
            workspace: None,
        })
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            state: record(&[
                ("repo", self.repo.clone()),
                ("rules", self.rules.clone()),
                ("changes", self.changes.clone()),
                ("selection", self.selection.clone()),
            ]),
        }
    }

    // Lay the repo out and commit it as the baseline.
    fn build(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let [repo, rules] = arguments else {
            return Err(format!("build expects a repo and rules: {arguments:?}"));
        };
        self.workspace = Some(Workspace::commit(&Repo::decode(repo)?)?);
        self.repo = repo.clone();
        self.rules = rules.clone();
        self.changes = record(&[
            ("edits", RuntimeValue::Set(BTreeSet::new())),
            ("comments", RuntimeValue::Set(BTreeSet::new())),
            ("trailing", RuntimeValue::Set(BTreeSet::new())),
        ]);
        self.selection = no_selection();
        Ok(())
    }

    // The same, for a repo given as a base and the references laid over it.
    fn build_with(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let [base, delta, rules] = arguments else {
            return Err(format!(
                "buildWith expects a base, a delta and rules: {arguments:?}"
            ));
        };
        let (RuntimeValue::Record(base_fields), RuntimeValue::Record(delta_fields)) = (base, delta)
        else {
            return Err(format!("buildWith expects records: {base:?} {delta:?}"));
        };
        let mut fields = base_fields.clone();
        fields.extend(delta_fields.clone());
        self.build(&[RuntimeValue::Record(fields), rules.clone()])
    }

    // Apply the edits to the working tree, as bumps of one line each.
    fn change(&mut self, arguments: &[RuntimeValue]) -> Result<(), String> {
        let [changes] = arguments else {
            return Err(format!("change expects one change set: {arguments:?}"));
        };
        let workspace = self.workspace.as_ref().ok_or("change before build")?;
        workspace.write(&Repo::decode(&self.repo)?, &Changes::decode(changes)?)?;
        self.changes = changes.clone();
        self.selection = no_selection();
        Ok(())
    }

    // Ask the real engine which tests the working tree's change selects.
    fn select_now(&mut self, _arguments: &[RuntimeValue]) -> Result<(), String> {
        let repo = Repo::decode(&self.repo)?;
        let workspace = self.workspace.as_ref().ok_or("select before build")?;
        let plan = workspace.plan()?;
        self.selection = selection(&plan, &repo.test_units)?;
        Ok(())
    }
}
