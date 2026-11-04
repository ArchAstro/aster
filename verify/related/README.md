# Related-test selection: model and conformance check

`aster affected <target> --related` picks tests from a source-level graph
(`src/related/`). This directory holds a [Quint](https://quint.sh) model of the
Elixir path through that engine and a check that runs the real engine against
the model.

```text
model.qnt        the repo, change and selection function; the oracle; the properties
repos.qnt        fixed repos (shop, workers)
universe.qnt     pseudo-random references laid over the shop (genDelta, genChange)
scenarios.qnt    @conformance runs: build a repo, change it, expected selection
witnesses.qnt    divergences, pinned on fixed repos in the model
properties.qnt   invariants checked on random repos with `quint run`
src/, tests/     the Rust side: lay a model repo out as Elixir files in a git
                 repo, run the aster binary, read back the selected tests
```

## What the model says

A repo is definitions in units (one file and module each): `fn`, `callback`,
`macro`, `route`, `top` (module-level code) and `test`. References are qualified
calls, bare calls, `use`, struct and schema uses, path literals, listing the
application's modules, and function names held as atoms. A change edits
definitions, edits comments, or edits a trailing comment.

`selectTests(repo, rules, changes)` is the least fixpoint of these rules, each
labelled with the name the model gives it:

| Rule | Reaches |
|---|---|
| `named` | definitions that name the one reached |
| `callback`, `macro`, `unnamed` | every user of the unit, narrowed by #353 and #347: a function nothing calls in a file with no `use` reaches nothing unless it is the edit; a lifecycle callback goes to what may start the process; a message clause to what may send its tag |
| `top-members`, `top-fanout` | the unit's definitions, and every user of the unit |
| `route-request` | definitions holding a path literal the route serves |
| `route-fallback` | every user of the router unit, when no literal does |
| reflection | definitions that list modules at run time reach every non-test change |

`Rules` holds pull requests as switches. `callbackClauses` (#347), `deadSilent`
and `processStarters` (#353) are on in `mainRules`, and `fullFallback`, the
"reaches no test, run in full" check #354 removed, is off. `schemaTop` (#348)
and `routeNothing` (#349) are open and off. Each switch is a small guard in
`usersOf` / `callbackUsers` / `fanRule` / `linksFrom`, so the old rule is one
`.with(..)` away.

`execEdges` is the run-time oracle: what may execute what, written from how
Elixir runs code and not from the analysis. Preload, run-time-built paths and
computed module names are its named carve-outs (`preload`, `built-path`,
`computed`).

## Properties

Checked on random repos by `npm run quint:properties`:

1. Soundness: a test that may run changed code through an edge kind the
   analysis follows is selected, outside the named carve-outs.
2. Precision: each reached definition has a witness path of modelled edges,
   and each fan-out comes from a definition of the right kind.
3. A comment-only change selects nothing.
4. Monotonic: a superset of edits selects a superset of tests (with `fullFallback` off).
5. Tests missed by the analysis are missed only through a carve-out.

Properties that do not hold are pinned in `witnesses.qnt` (`npm run quint:test`)
and, against the real engine, in `tests/divergences.rs`.

## Running it

```console
npm ci
npm run check            # generated files match the model
npm run typecheck
npm run quint:test       # scenarios and witnesses, in Quint
npm run quint:properties # properties on random repos
cargo build --bin aster --manifest-path ../../Cargo.toml
cargo test               # the engine against the model, and the divergences
```

`ASTER_BIN` points the check at another `aster` binary. `RELATED_MODEL_KEEP=1`
keeps each scratch repo on disk and prints its path, for rerunning `aster` by
hand.

The generated files (`quint-refinements.json`, `src/generated_refinement.rs`)
come from `npm run compile`; CI fails if they drift from the model.
`quint-refinements` is taken from a pinned commit of
`ArchAstro/quint-refinements` (see `package.json` and `Cargo.toml`): the npm
package is at 0.1.0 and the crate is not on crates.io yet.

## Not modelled

Go, JavaScript, Python and Rust paths; several projects and the declared
project graph; `defimpl` and its `via` rule; deleted and renamed definitions;
import-line edits; `alias`; data files and file-path literals; runner
configuration; the facts cache; `describe` and test names.
