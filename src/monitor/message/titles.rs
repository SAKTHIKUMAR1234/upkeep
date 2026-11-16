use std::collections::BTreeMap;
use std::path::Path;

use crate::adapter::Catalog;
use crate::plan::Plan;
use crate::store::Step;

/// Plain names for common actions, used when `adapter.toml` gives the action no title.
const ACTION_TITLES: &[(&str, &str)] = &[
    ("check_updates", "Check for updates"),
    ("maintenance_on", "Maintenance on"),
    ("maintenance_off", "Maintenance off"),
    ("backup", "Backup"),
    ("snapshot", "Snapshot"),
    ("retention", "Remove old snapshots"),
    ("update", "Install updates"),
    ("verify", "Check services"),
    ("rollback", "Roll back"),
    ("restore", "Restore"),
];

/// The plain names of a job's plan and steps. Messages never show `adapter:action`.
#[derive(Clone, Debug, Default)]
pub struct Titles {
    /// The plan's `title`; the plan name when absent.
    pub plan: Option<String>,
    /// Action titles from `adapter.toml`, keyed by adapter and action.
    pub actions: BTreeMap<(String, String), String>,
    /// Adapter display names from `adapter.toml`.
    pub adapters: BTreeMap<String, String>,
}

impl Titles {
    /// Reads the titles of plan `target` and of every adapter. Missing files leave the fallbacks.
    pub fn load(target: &str, catalog: &Catalog, plans_dir: &Path) -> Self {
        let plan = Plan::load(plans_dir, target)
            .ok()
            .and_then(|plan| plan.title);
        let mut titles = Self {
            plan,
            ..Self::default()
        };
        for (name, adapter) in &catalog.adapters {
            let manifest = &adapter.manifest;
            if let Some(display_name) = &manifest.display_name {
                titles.adapters.insert(name.clone(), display_name.clone());
            }
            for (action, definition) in &manifest.actions {
                if let Some(title) = &definition.title {
                    titles
                        .actions
                        .insert((name.clone(), action.clone()), title.clone());
                }
            }
        }
        titles
    }

    /// For example "OS UPDATE" from the title "OS update" or the plan name "os-update".
    pub fn plan_label(&self, target: &str) -> String {
        match &self.plan {
            Some(title) => title.to_uppercase(),
            None => words(target).to_uppercase(),
        }
    }

    /// The step's title. When another step of the same phase has the same title, the adapter's name follows.
    pub fn step(&self, step: &Step, steps: &[Step]) -> String {
        let title = self.action(&step.adapter, &step.action);
        let shared = steps.iter().any(|other| {
            other.phase == step.phase
                && other.adapter != step.adapter
                && self.action(&other.adapter, &other.action) == title
        });
        if !shared {
            return title;
        }
        let adapter = self
            .adapters
            .get(&step.adapter)
            .cloned()
            .unwrap_or_else(|| step.adapter.clone());
        format!("{title} ({adapter})")
    }

    fn action(&self, adapter: &str, action: &str) -> String {
        if let Some(title) = self.actions.get(&(adapter.to_owned(), action.to_owned())) {
            return title.clone();
        }
        match ACTION_TITLES.iter().find(|(name, _)| *name == action) {
            Some((_, title)) => (*title).to_owned(),
            None => super::capitalize(&words(action)),
        }
    }
}

/// A name with `_` and `-` as spaces, such as "clear cache" from "clear_cache".
fn words(name: &str) -> String {
    name.replace(['_', '-'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Phase, StepStatus};

    fn step(phase: Phase, adapter: &str, action: &str) -> Step {
        Step {
            job_id: 1,
            phase,
            index: 0,
            adapter: adapter.into(),
            action: action.into(),
            adapter_dir: String::new(),
            run: String::new(),
            timeout_secs: 60,
            status: StepStatus::Pending,
            exit_code: None,
            result: None,
            error: None,
            started_at: None,
            finished_at: None,
        }
    }

    #[test]
    fn manifest_titles_win_over_built_in_names_and_word_fallbacks() {
        let titles = Titles {
            actions: BTreeMap::from([(("os".into(), "snapshot".into()), "Taking snapshot".into())]),
            ..Titles::default()
        };
        let steps = [
            step(Phase::Main, "os", "snapshot"),
            step(Phase::Main, "os", "update"),
            step(Phase::Main, "os", "clear_cache"),
        ];
        let names: Vec<String> = steps.iter().map(|one| titles.step(one, &steps)).collect();
        assert_eq!(names, ["Taking snapshot", "Install updates", "Clear cache"]);
    }

    #[test]
    fn shared_titles_name_the_adapter() {
        let titles = Titles {
            adapters: BTreeMap::from([("hr-stage".into(), "HR staging".into())]),
            ..Titles::default()
        };
        let steps = [
            step(Phase::Main, "hr-stage", "verify"),
            step(Phase::Main, "os", "verify"),
            step(Phase::Recovery, "os", "rollback"),
            step(Phase::Recovery, "os", "verify"),
        ];
        let names: Vec<String> = steps.iter().map(|one| titles.step(one, &steps)).collect();
        assert_eq!(
            names,
            [
                "Check services (HR staging)",
                "Check services (os)",
                "Roll back",
                "Check services"
            ]
        );
    }

    #[test]
    fn plan_label_uses_the_title_or_the_plan_name() {
        let titled = Titles {
            plan: Some("OS update".into()),
            ..Titles::default()
        };
        assert_eq!(titled.plan_label("os-update"), "OS UPDATE");
        assert_eq!(Titles::default().plan_label("bench_update"), "BENCH UPDATE");
    }

    #[test]
    fn load_reads_plan_and_manifest_titles() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("os-update.toml"),
            "title = \"OS update\"\nsteps = [\"os:snapshot\"]\n",
        )
        .unwrap();
        let manifest = crate::adapter::Manifest::parse(
            "name = \"os\"\nrun = \"x\"\ndisplay_name = \"Server\"\n[actions.snapshot]\ntitle = \"Taking snapshot\"\n",
            "os",
        )
        .unwrap();
        let mut catalog = Catalog::default();
        catalog.adapters.insert(
            "os".into(),
            crate::adapter::Adapter {
                manifest,
                dir: dir.path().into(),
            },
        );
        let titles = Titles::load("os-update", &catalog, dir.path());
        assert_eq!(titles.plan.as_deref(), Some("OS update"));
        assert_eq!(titles.adapters["os"], "Server");
        assert_eq!(
            titles.actions[&("os".to_owned(), "snapshot".to_owned())],
            "Taking snapshot"
        );
        assert_eq!(Titles::load("missing", &catalog, dir.path()).plan, None);
    }
}
