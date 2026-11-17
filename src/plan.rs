use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::adapter::manifest::is_valid_name;
use crate::adapter::{Adapter, Catalog};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlanError {
    #[error("unknown plan \"{0}\"")]
    UnknownPlan(String),
    #[error("cannot read plan \"{name}\": {reason}")]
    Read { name: String, reason: String },
    #[error("invalid step \"{0}\": use adapter:action, writers:action or all:action")]
    BadStep(String),
    #[error("unknown adapter \"{0}\"")]
    UnknownAdapter(String),
    #[error("adapter \"{adapter}\" is invalid: {error}")]
    InvalidAdapter { adapter: String, error: String },
    #[error("adapter \"{adapter}\" has no action \"{action}\"")]
    UnknownAction { adapter: String, action: String },
    #[error("\"{0}\" matches no adapter")]
    NoMatch(String),
    #[error("step {adapter}:{action} needs {adapter}:{need} to run earlier in the same list")]
    UnmetNeed {
        adapter: String,
        action: String,
        need: String,
    },
    #[error("plan \"{0}\" has no steps")]
    Empty(String),
}

/// A plan file `<config>/plans/<name>.toml`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    /// The plain name in messages, such as "OS update".
    pub title: Option<String>,
    #[serde(default)]
    pub confirm: bool,
    pub steps: Vec<String>,
    #[serde(default)]
    pub on_failure: Vec<String>,
    /// Keep one Telegram message for each job of this plan, edited while it runs.
    #[serde(default = "yes")]
    pub notify: bool,
}

fn yes() -> bool {
    true
}

/// One adapter action, resolved against the catalog, ready to store and run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepSpec {
    pub adapter: String,
    pub action: String,
    pub adapter_dir: PathBuf,
    pub run: String,
    pub timeout: Duration,
}

/// What a job will run: the main steps and the recovery steps.
#[derive(Debug)]
pub struct JobPlan {
    pub target: String,
    pub confirm: bool,
    pub steps: Vec<StepSpec>,
    pub recovery: Vec<StepSpec>,
}

impl Plan {
    pub fn load(plans_dir: &Path, name: &str) -> Result<Self, PlanError> {
        if !is_valid_name(name) {
            return Err(PlanError::UnknownPlan(name.to_owned()));
        }
        let path = plans_dir.join(format!("{name}.toml"));
        let read_error = |reason: String| PlanError::Read {
            name: name.to_owned(),
            reason,
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(PlanError::UnknownPlan(name.to_owned()));
            }
            Err(error) => return Err(read_error(error.to_string())),
        };
        toml::from_str(&text).map_err(|error| read_error(error.to_string()))
    }
}

/// The names of the plan files in `plans_dir`, sorted. A missing folder has no plans.
pub fn names(plans_dir: &Path) -> Vec<String> {
    let entries = std::fs::read_dir(plans_dir).into_iter().flatten().flatten();
    let mut names: Vec<String> = entries
        .filter_map(|entry| {
            let file_name = entry.file_name().into_string().ok()?;
            let name = file_name.strip_suffix(".toml")?;
            is_valid_name(name).then(|| name.to_owned())
        })
        .collect();
    names.sort();
    names
}

/// A job gets a Telegram message when it runs a plan that does not set `notify = false`.
/// A single `adapter:action` is not announced.
pub fn notifies(target: &str, plans_dir: &Path) -> bool {
    !target.contains(':') && Plan::load(plans_dir, target).map_or(true, |plan| plan.notify)
}

impl JobPlan {
    /// Resolves `target`: `adapter:action` runs one action, anything else names a plan.
    pub fn resolve(target: &str, catalog: &Catalog, plans_dir: &Path) -> Result<Self, PlanError> {
        if target.contains(':') {
            return Ok(Self {
                target: target.to_owned(),
                confirm: false,
                steps: expand_list(&[target.to_owned()], catalog)?,
                recovery: Vec::new(),
            });
        }

        let plan = Plan::load(plans_dir, target)?;
        if plan.steps.is_empty() {
            return Err(PlanError::Empty(target.to_owned()));
        }
        Ok(Self {
            target: target.to_owned(),
            confirm: plan.confirm,
            steps: expand_list(&plan.steps, catalog)?,
            recovery: expand_list(&plan.on_failure, catalog)?,
        })
    }
}

/// Expands every step of a list and checks each action's `needs` against the steps before it.
pub fn expand_list(steps: &[String], catalog: &Catalog) -> Result<Vec<StepSpec>, PlanError> {
    let mut expanded: Vec<StepSpec> = Vec::new();
    for step in steps {
        for spec in expand_step(step, catalog)? {
            check_needs(&spec, &expanded, catalog)?;
            expanded.push(spec);
        }
    }
    Ok(expanded)
}

/// Expands `adapter:action`, `writers:action` or `all:action` in adapter name order.
pub fn expand_step(step: &str, catalog: &Catalog) -> Result<Vec<StepSpec>, PlanError> {
    let (selector, action) = step
        .split_once(':')
        .filter(|(selector, action)| is_valid_name(selector) && is_valid_name(action))
        .ok_or_else(|| PlanError::BadStep(step.to_owned()))?;

    let adapters: Vec<&Adapter> = match selector {
        "all" => catalog.adapters.values().collect(),
        "writers" => catalog.writers().collect(),
        name => vec![named_adapter(name, catalog)?],
    };
    if adapters.is_empty() {
        return Err(PlanError::NoMatch(step.to_owned()));
    }
    adapters
        .into_iter()
        .map(|adapter| step_spec(adapter, action))
        .collect()
}

fn named_adapter<'a>(name: &str, catalog: &'a Catalog) -> Result<&'a Adapter, PlanError> {
    if let Some(adapter) = catalog.get(name) {
        return Ok(adapter);
    }
    Err(match catalog.error_for(name) {
        Some(error) => PlanError::InvalidAdapter {
            adapter: name.to_owned(),
            error: error.error.clone(),
        },
        None => PlanError::UnknownAdapter(name.to_owned()),
    })
}

fn step_spec(adapter: &Adapter, action: &str) -> Result<StepSpec, PlanError> {
    let manifest = &adapter.manifest;
    let definition = manifest
        .actions
        .get(action)
        .ok_or_else(|| PlanError::UnknownAction {
            adapter: manifest.name.clone(),
            action: action.to_owned(),
        })?;
    Ok(StepSpec {
        adapter: manifest.name.clone(),
        action: action.to_owned(),
        adapter_dir: adapter.dir.clone(),
        run: manifest.run.clone(),
        timeout: definition.timeout,
    })
}

fn check_needs(spec: &StepSpec, earlier: &[StepSpec], catalog: &Catalog) -> Result<(), PlanError> {
    let needs = &catalog.adapters[&spec.adapter].manifest.actions[&spec.action].needs;
    let missing = needs.iter().find(|need| {
        !earlier
            .iter()
            .any(|step| step.adapter == spec.adapter && &step.action == *need)
    });
    match missing {
        None => Ok(()),
        Some(need) => Err(PlanError::UnmetNeed {
            adapter: spec.adapter.clone(),
            action: spec.action.clone(),
            need: need.clone(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::Manifest;

    fn adapter(name: &str, writes_data: bool, actions: &str) -> Adapter {
        let text =
            format!("name = \"{name}\"\nrun = \"./run\"\nwrites_data = {writes_data}\n{actions}");
        Adapter {
            manifest: Manifest::parse(&text, name).unwrap(),
            dir: PathBuf::from("/adapters").join(name),
        }
    }

    fn catalog() -> Catalog {
        let mut catalog = Catalog::default();
        let adapters = [
            adapter(
                "bench",
                true,
                "[actions.maintenance_on]\n[actions.backup]\n[actions.verify]\n",
            ),
            adapter(
                "relay",
                true,
                "[actions.maintenance_on]\n[actions.backup]\n[actions.verify]\n",
            ),
            adapter(
                "os",
                false,
                "[actions.snapshot]\n[actions.verify]\n[actions.update]\nneeds = [\"snapshot\"]\n",
            ),
        ];
        for adapter in adapters {
            catalog
                .adapters
                .insert(adapter.manifest.name.clone(), adapter);
        }
        catalog
    }

    fn names(specs: &[StepSpec]) -> Vec<String> {
        specs
            .iter()
            .map(|spec| format!("{}:{}", spec.adapter, spec.action))
            .collect()
    }

    #[test]
    fn writers_expand_to_adapters_that_write_data() {
        let specs = expand_step("writers:backup", &catalog()).unwrap();
        assert_eq!(names(&specs), ["bench:backup", "relay:backup"]);
    }

    #[test]
    fn all_expands_to_every_adapter_in_name_order() {
        let specs = expand_step("all:verify", &catalog()).unwrap();
        assert_eq!(names(&specs), ["bench:verify", "os:verify", "relay:verify"]);
    }

    #[test]
    fn selectors_fail_when_an_adapter_lacks_the_action() {
        let error = expand_step("all:backup", &catalog()).unwrap_err();
        assert_eq!(
            error,
            PlanError::UnknownAction {
                adapter: "os".into(),
                action: "backup".into()
            }
        );
    }

    #[test]
    fn named_step_resolves_one_adapter() {
        let specs = expand_step("os:snapshot", &catalog()).unwrap();
        assert_eq!(specs[0].adapter_dir, PathBuf::from("/adapters/os"));
        assert_eq!(specs[0].run, "./run");
    }

    #[test]
    fn rejects_bad_steps_and_unknown_adapters() {
        assert!(matches!(
            expand_step("backup", &catalog()),
            Err(PlanError::BadStep(_))
        ));
        assert!(matches!(
            expand_step("os:", &catalog()),
            Err(PlanError::BadStep(_))
        ));
        assert_eq!(
            expand_step("nope:verify", &catalog()),
            Err(PlanError::UnknownAdapter("nope".into()))
        );
    }

    #[test]
    fn writers_with_no_writer_is_an_error() {
        let mut catalog = catalog();
        catalog.adapters.retain(|name, _| name == "os");
        assert!(matches!(
            expand_step("writers:backup", &catalog),
            Err(PlanError::NoMatch(_))
        ));
    }

    #[test]
    fn needs_must_run_earlier_in_the_list() {
        let ordered = ["os:snapshot".to_owned(), "os:update".to_owned()];
        assert_eq!(names(&expand_list(&ordered, &catalog()).unwrap()), ordered);

        let error = expand_list(&["os:update".to_owned()], &catalog()).unwrap_err();
        assert!(matches!(error, PlanError::UnmetNeed { need, .. } if need == "snapshot"));
    }

    #[test]
    fn resolves_plan_files_and_single_actions() {
        let dir = tempfile::tempdir().unwrap();
        let plan = "confirm = true\nsteps = [\"writers:maintenance_on\", \"os:snapshot\"]\non_failure = [\"all:verify\"]\n";
        std::fs::write(dir.path().join("update.toml"), plan).unwrap();

        let job = JobPlan::resolve("update", &catalog(), dir.path()).unwrap();
        assert!(job.confirm);
        assert_eq!(
            names(&job.steps),
            [
                "bench:maintenance_on",
                "relay:maintenance_on",
                "os:snapshot"
            ]
        );
        assert_eq!(job.recovery.len(), 3);

        let single = JobPlan::resolve("bench:backup", &catalog(), dir.path()).unwrap();
        assert!(!single.confirm && single.recovery.is_empty());
        assert_eq!(
            JobPlan::resolve("missing", &catalog(), dir.path()).unwrap_err(),
            PlanError::UnknownPlan("missing".into())
        );
    }

    #[test]
    fn names_lists_valid_plan_files_in_order() {
        let dir = tempfile::tempdir().unwrap();
        for file in ["os-update.toml", "bench.toml", "notes.txt", "bad name.toml"] {
            std::fs::write(dir.path().join(file), "steps = []\n").unwrap();
        }
        assert_eq!(super::names(dir.path()), ["bench", "os-update"]);
        assert!(super::names(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn plans_notify_unless_they_opt_out() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("loud.toml"), "steps = [\"a:b\"]\n").unwrap();
        std::fs::write(
            dir.path().join("quiet.toml"),
            "steps = [\"a:b\"]\nnotify = false\n",
        )
        .unwrap();
        assert!(notifies("loud", dir.path()));
        assert!(!notifies("quiet", dir.path()));
        assert!(!notifies("os:update", dir.path()));
        assert!(notifies("removed", dir.path()));
    }
}
