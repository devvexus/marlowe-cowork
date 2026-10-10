//! The build DAG strip: where a `build_v2` run is (scope, plan, contract, implement, integrate,
//! verify, review, fix, deliver) and its task graph, level by level.
//!
//! The playbook reports progress as a step whose `action` is `dag ` followed by JSON:
//!
//! ```text
//! dag {"stage":"implement","hubs":["Cargo.toml"],"tasks":[
//!      {"id":"models","goal":"add the models","deps":[],"state":"done"},
//!      {"id":"api","goal":"routes","deps":["models"],"state":"running"}]}
//! ```
//!
//! The client keeps the latest report per run and does not show it as a transcript line. Task
//! states are `waiting`, `running`, `done`, `failed`, `blocked` (needs a hub change) and `serial`
//! (re-running one task at a time on the merged branch). Anything else reads as waiting.

use serde::Deserialize;

/// The stages, in order.
pub const STAGES: [&str; 9] = ["scope", "plan", "contract", "implement", "integrate", "verify", "review", "fix", "deliver"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TaskState {
    #[default]
    Waiting,
    Running,
    Done,
    Failed,
    Blocked,
    Serial,
}

impl TaskState {
    fn parse(s: &str) -> TaskState {
        match s.trim().to_ascii_lowercase().as_str() {
            "running" | "active" | "started" => TaskState::Running,
            "done" | "passed" | "merged" => TaskState::Done,
            "failed" | "error" => TaskState::Failed,
            "blocked" | "need_hub_change" => TaskState::Blocked,
            "serial" | "retry" => TaskState::Serial,
            _ => TaskState::Waiting,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DagTask {
    pub id: String,
    pub goal: String,
    pub deps: Vec<String>,
    pub state: TaskState,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BuildDag {
    pub stage: Option<String>,
    pub tasks: Vec<DagTask>,
    /// Files frozen after the contract stage.
    pub hubs: Vec<String>,
}

#[derive(Deserialize)]
struct Wire {
    #[serde(default)]
    stage: Option<String>,
    #[serde(default)]
    tasks: Vec<WireTask>,
    #[serde(default)]
    hubs: Vec<String>,
}

#[derive(Deserialize)]
struct WireTask {
    id: String,
    #[serde(default)]
    goal: String,
    #[serde(default, alias = "depends_on")]
    deps: Vec<String>,
    #[serde(default)]
    state: String,
}

/// A `dag {json}` step action as a report. `None` for any other action, or JSON that is not one.
pub fn parse(action: &str) -> Option<BuildDag> {
    let json = action.trim_start().strip_prefix("dag ")?;
    let w: Wire = serde_json::from_str(json.trim()).ok()?;
    Some(BuildDag {
        stage: w.stage.map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty()),
        tasks: w
            .tasks
            .into_iter()
            .map(|t| DagTask { id: t.id, goal: t.goal, deps: t.deps, state: TaskState::parse(&t.state) })
            .collect(),
        hubs: w.hubs,
    })
}

impl BuildDag {
    /// Index of the current stage in [`STAGES`].
    pub fn stage_index(&self) -> Option<usize> {
        let s = self.stage.as_deref()?;
        STAGES.iter().position(|x| *x == s)
    }

    /// Tasks grouped by depth: a task is one level below its deepest dependency. A dependency on a
    /// task that is not in the report is ignored, and a cycle ends at the first repeat rather than
    /// looping.
    pub fn levels(&self) -> Vec<Vec<&DagTask>> {
        let mut depth: Vec<Option<usize>> = vec![None; self.tasks.len()];
        let index = |id: &str| self.tasks.iter().position(|t| t.id == id);
        fn level(i: usize, dag: &BuildDag, depth: &mut Vec<Option<usize>>, stack: &mut Vec<usize>, index: &dyn Fn(&str) -> Option<usize>) -> usize {
            if let Some(d) = depth[i] {
                return d;
            }
            if stack.contains(&i) {
                return 0;
            }
            stack.push(i);
            let d = dag.tasks[i]
                .deps
                .iter()
                .filter_map(|d| index(d))
                .map(|j| level(j, dag, depth, stack, index) + 1)
                .max()
                .unwrap_or(0);
            stack.pop();
            depth[i] = Some(d);
            d
        }
        let mut stack = Vec::new();
        for i in 0..self.tasks.len() {
            level(i, self, &mut depth, &mut stack, &index);
        }
        let max = depth.iter().flatten().copied().max().unwrap_or(0);
        let mut out: Vec<Vec<&DagTask>> = vec![Vec::new(); if self.tasks.is_empty() { 0 } else { max + 1 }];
        for (i, t) in self.tasks.iter().enumerate() {
            out[depth[i].unwrap_or(0)].push(t);
        }
        out
    }

    /// `3 of 5 done` and anything failing.
    pub fn summary(&self) -> String {
        let done = self.tasks.iter().filter(|t| t.state == TaskState::Done).count();
        let failed = self.tasks.iter().filter(|t| t.state == TaskState::Failed).count();
        let mut s = format!("{done} of {} tasks done", self.tasks.len());
        if failed > 0 {
            s.push_str(&format!(" · {failed} failed"));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPORT: &str = r#"dag {"stage":"Implement","hubs":["Cargo.toml"],"tasks":[
        {"id":"models","goal":"models","deps":[],"state":"done"},
        {"id":"schema","goal":"schema","deps":[],"state":"done"},
        {"id":"api","goal":"routes","depends_on":["models","schema"],"state":"running"},
        {"id":"ui","goal":"pages","deps":["api"],"state":"blocked"},
        {"id":"docs","goal":"docs","deps":["ghost"],"state":"weird"}]}"#;

    #[test]
    fn a_report_parses_with_its_stage_and_task_states() {
        let d = parse(REPORT).unwrap();
        assert_eq!(d.stage.as_deref(), Some("implement"));
        assert_eq!(d.stage_index(), Some(3));
        assert_eq!(d.tasks.len(), 5);
        assert_eq!(d.tasks[2].deps, ["models", "schema"], "depends_on is accepted");
        assert_eq!(d.tasks[3].state, TaskState::Blocked);
        assert_eq!(d.tasks[4].state, TaskState::Waiting);
        assert_eq!(d.hubs, ["Cargo.toml"]);
        assert_eq!(d.summary(), "2 of 5 tasks done");
        assert!(parse("web_search \"x\"").is_none());
        assert!(parse("dag not json").is_none());
    }

    #[test]
    fn levels_follow_the_dependencies() {
        let d = parse(REPORT).unwrap();
        let names: Vec<Vec<&str>> = d.levels().iter().map(|l| l.iter().map(|t| t.id.as_str()).collect()).collect();
        assert_eq!(names, vec![vec!["models", "schema", "docs"], vec!["api"], vec!["ui"]]);
    }

    #[test]
    fn a_cycle_does_not_loop() {
        let d = parse(r#"dag {"tasks":[{"id":"a","deps":["b"]},{"id":"b","deps":["a"]}]}"#).unwrap();
        assert_eq!(d.levels().iter().map(Vec::len).sum::<usize>(), 2);
    }
}
