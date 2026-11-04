//! The model's `Repo` value, decoded from the runtime values the generated
//! scenarios hand to the implementation.

use quint_refinements::RuntimeValue;
use std::collections::{BTreeMap, BTreeSet};

pub type Pair = (i64, i64);

#[derive(Debug, Clone, Default)]
pub struct Repo {
    pub kind: BTreeMap<i64, String>,
    pub unit: BTreeMap<i64, i64>,
    pub name: BTreeMap<i64, String>,
    pub test_units: BTreeSet<i64>,
    pub calls: BTreeSet<Pair>,
    pub bare: BTreeSet<Pair>,
    pub imports: BTreeSet<Pair>,
    pub shapes: BTreeSet<Pair>,
    pub routes: BTreeMap<i64, Vec<i64>>,
    pub serves: BTreeMap<i64, i64>,
    pub requests: Vec<(i64, Vec<i64>)>,
    pub reflects: BTreeSet<i64>,
    pub preloads: BTreeSet<Pair>,
    pub built_paths: BTreeSet<Pair>,
    pub computed: BTreeSet<Pair>,
    pub accepts: BTreeMap<i64, i64>,
    pub sends: BTreeSet<Pair>,
    pub sends_any: BTreeSet<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct Changes {
    pub edits: BTreeSet<i64>,
    pub comments: BTreeSet<i64>,
    pub trailing: BTreeSet<i64>,
}

fn field<'a>(value: &'a RuntimeValue, name: &str) -> Result<&'a RuntimeValue, String> {
    match value {
        RuntimeValue::Record(fields) => fields
            .get(name)
            .ok_or_else(|| format!("record has no field {name}: {value:?}")),
        other => Err(format!("expected a record with {name}, got {other:?}")),
    }
}

fn int(value: &RuntimeValue) -> Result<i64, String> {
    match value {
        RuntimeValue::Int(n) => Ok(*n),
        other => Err(format!("expected an integer, got {other:?}")),
    }
}

fn text(value: &RuntimeValue) -> Result<String, String> {
    match value {
        RuntimeValue::Text(s) => Ok(s.clone()),
        other => Err(format!("expected text, got {other:?}")),
    }
}

fn set(value: &RuntimeValue) -> Result<&BTreeSet<RuntimeValue>, String> {
    match value {
        RuntimeValue::Set(items) => Ok(items),
        other => Err(format!("expected a set, got {other:?}")),
    }
}

fn int_set(value: &RuntimeValue) -> Result<BTreeSet<i64>, String> {
    set(value)?.iter().map(int).collect()
}

fn int_list(value: &RuntimeValue) -> Result<Vec<i64>, String> {
    match value {
        RuntimeValue::List(items) => items.iter().map(int).collect(),
        other => Err(format!("expected a list, got {other:?}")),
    }
}

fn pairs(value: &RuntimeValue) -> Result<BTreeSet<Pair>, String> {
    set(value)?
        .iter()
        .map(|pair| Ok((int(field(pair, "from")?)?, int(field(pair, "to")?)?)))
        .collect()
}

fn map(value: &RuntimeValue) -> Result<&BTreeMap<RuntimeValue, RuntimeValue>, String> {
    match value {
        RuntimeValue::Map(entries) => Ok(entries),
        other => Err(format!("expected a map, got {other:?}")),
    }
}

impl Repo {
    pub fn decode(value: &RuntimeValue) -> Result<Self, String> {
        let get = |name| field(value, name);
        let mut repo = Repo {
            test_units: int_set(get("testUnits")?)?,
            calls: pairs(get("calls")?)?,
            bare: pairs(get("bare")?)?,
            imports: pairs(get("imports")?)?,
            shapes: pairs(get("shapes")?)?,
            reflects: int_set(get("reflects")?)?,
            preloads: pairs(get("preloads")?)?,
            built_paths: pairs(get("builtPaths")?)?,
            computed: pairs(get("computed")?)?,
            ..Repo::default()
        };
        for (k, v) in map(get("kindOf")?)? {
            repo.kind.insert(int(k)?, text(v)?);
        }
        for (k, v) in map(get("unitOf")?)? {
            repo.unit.insert(int(k)?, int(v)?);
        }
        for (k, v) in map(get("nameOf")?)? {
            repo.name.insert(int(k)?, text(v)?);
        }
        for (k, v) in map(get("routes")?)? {
            repo.routes.insert(int(k)?, int_list(v)?);
        }
        for (k, v) in map(get("serves")?)? {
            repo.serves.insert(int(k)?, int(v)?);
        }
        for (k, v) in map(get("accepts")?)? {
            repo.accepts.insert(int(k)?, int(v)?);
        }
        for send in set(get("sends")?)? {
            repo.sends
                .insert((int(field(send, "id")?)?, int(field(send, "tag")?)?));
        }
        repo.sends_any = int_set(get("sendsAny")?)?;
        for request in set(get("requests")?)? {
            repo.requests.push((
                int(field(request, "id")?)?,
                int_list(field(request, "path")?)?,
            ));
        }
        Ok(repo)
    }

    pub fn defs_of(&self, unit: i64) -> Vec<i64> {
        self.unit
            .iter()
            .filter(|(_, u)| **u == unit)
            .map(|(d, _)| *d)
            .collect()
    }

    pub fn units(&self) -> BTreeSet<i64> {
        self.unit.values().copied().collect()
    }
}

impl Changes {
    pub fn decode(value: &RuntimeValue) -> Result<Self, String> {
        Ok(Changes {
            edits: int_set(field(value, "edits")?)?,
            comments: int_set(field(value, "comments")?)?,
            trailing: int_set(field(value, "trailing")?)?,
        })
    }
}
