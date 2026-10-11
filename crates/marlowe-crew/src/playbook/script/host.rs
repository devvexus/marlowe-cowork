//! The crew's [`AgentHost`]: every `agent()` of a workflow is one crew step on the run's
//! [`AgentRunner`] (checkpointed under its call key, metered against the run budget, its shells
//! under the sandbox policy), on a thread of its own with a cancel token of its own so a skip
//! stops just that agent.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crossbeam_channel::Sender;
use marlowe_model::{CancelToken, Effort};
use marlowe_proto::{RunId, SandboxKind};
use marlowe_workflow::{AgentCall, AgentHost, AgentResult, AgentStatus, Completion};
use serde_json::Value;

use super::structured::{Slot, WithStructuredOutput, STRUCTURED_OUTPUT};
use super::RunObserver;
use crate::playbook::{AgentRunner, StepSpec};
use crate::sandbox::Sandbox;
use crate::CrewError;

/// The role card a workflow agent runs with when the script names none.
pub const WORKFLOW_ROLE: &str = "workflow";

/// Runs workflow agents as crew steps.
pub struct CrewAgentHost {
    pub runner: AgentRunner,
    pub run: RunId,
    pub home: PathBuf,
    pub observer: Option<Arc<dyn RunObserver>>,
    cancels: Mutex<HashMap<u32, CancelToken>>,
}

impl CrewAgentHost {
    pub fn new(runner: AgentRunner, run: RunId, home: PathBuf, observer: Option<Arc<dyn RunObserver>>) -> Arc<Self> {
        Arc::new(CrewAgentHost { runner, run, home, observer, cancels: Mutex::new(HashMap::new()) })
    }

    /// The crew role for `agent_type`: a card of that name, else the nearest built-in.
    pub fn role_for(&self, agent_type: Option<&str>) -> Result<String, String> {
        let want = agent_type.map(str::trim).filter(|t| !t.is_empty());
        let Some(t) = want else { return Ok(WORKFLOW_ROLE.into()) };
        if self.runner.roles.get(t).is_some() {
            return Ok(t.to_string());
        }
        let mapped = match t {
            "general" | "general-purpose" | "workflow" => WORKFLOW_ROLE,
            "explore" | "Explore" => "scout",
            "plan" | "Plan" => "analyst",
            "review" | "reviewer" => "reviewer",
            other => return Err(format!("no agent type `{other}` (a crew role card of that name)")),
        };
        Ok(mapped.to_string())
    }
}

fn effort(e: Option<&str>) -> Option<Effort> {
    Some(match e? {
        "low" => Effort::Low,
        "medium" => Effort::Medium,
        "high" => Effort::High,
        // `xhigh` maps to the top level where a provider has no step between (A15).
        "xhigh" | "max" => Effort::Max,
        _ => return None,
    })
}

/// The prompt a schema agent gets: the script's prompt and how to answer.
fn schema_prompt(prompt: &str) -> String {
    format!(
        "{prompt}\n\nWhen you have the answer, call the `{STRUCTURED_OUTPUT}` tool once with it: its parameters are the answer's \
         schema. That call is your answer; end your turn after it."
    )
}

impl AgentHost for CrewAgentHost {
    fn start(&self, call: AgentCall, done: Sender<Completion>) {
        let index = call.index;
        let child = CancelToken::new();
        self.cancels.lock().unwrap_or_else(|e| e.into_inner()).insert(index, child.clone());
        let parent = self.runner.cancel.clone();
        let mut runner = self.runner.clone();
        runner.cancel = child.clone();
        let role = self.role_for(call.opts.agent_type.as_deref());
        let home = self.home.clone();
        let run = self.run.clone();
        let observer = self.observer.clone();
        let done_err = done.clone();
        let spawned = std::thread::Builder::new().name(format!("wf-{}-{index}", run.short())).spawn(move || {
            // The run's cancel (stop, pause, shutdown) reaches this agent too.
            let _link = parent.on_cancel({
                let c = child.clone();
                move || c.cancel()
            });
            let result = match role {
                Ok(role) => run_agent(&runner, &home, &run, &call, &role, &child, observer.as_deref()),
                Err(why) => AgentResult::failed(why, 0),
            };
            let _ = done.send(Completion { index, result });
        });
        if let Err(e) = spawned {
            let _ = done_err.send(Completion { index, result: AgentResult::failed(format!("could not start the agent's thread: {e}"), 0) });
        }
    }

    fn cancel(&self, index: u32) {
        if let Some(c) = self.cancels.lock().unwrap_or_else(|e| e.into_inner()).remove(&index) {
            c.cancel();
        }
    }
}

fn run_agent(
    base: &AgentRunner,
    home: &std::path::Path,
    run: &RunId,
    call: &AgentCall,
    role: &str,
    cancel: &CancelToken,
    observer: Option<&dyn RunObserver>,
) -> AgentResult {
    let mut runner = base.clone();
    let mut prompt = call.prompt.clone();
    if let Some(extra) = observer.and_then(|o| o.agent_start(run, call)).filter(|e| !e.trim().is_empty()) {
        prompt = format!("{prompt}\n\n{extra}");
    }
    let slot: Slot = Arc::new(Mutex::new(None));
    if let Some(schema) = call.opts.schema.clone() {
        prompt = schema_prompt(&prompt);
        let slot = slot.clone();
        runner.step_tools = Some(Arc::new(move |_spec, inner| Arc::new(WithStructuredOutput::new(inner, schema.clone(), slot.clone()))));
    }
    // `isolation: "worktree"`: a child worktree off the run's, removed if unchanged.
    let mut child_sandbox = None;
    if call.opts.isolation.as_deref() == Some("worktree") {
        let child_id = RunId(format!("{}-a{}", run.as_str(), call.index));
        match Sandbox::create(home, &child_id, Some(&base.sandbox.root), SandboxKind::Worktree) {
            Ok(sb) if sb.kind == SandboxKind::Worktree => child_sandbox = Some(sb),
            Ok(sb) => {
                let _ = sb.remove();
            }
            Err(e) => return finish(observer, run, call, AgentResult::failed(format!("could not create the agent's worktree: {e}"), 0)),
        }
    }
    let spec = StepSpec {
        key: call.key.clone(),
        role: role.to_string(),
        label: call.display_label(),
        prompt,
        tools: None,
        effort: effort(call.opts.effort.as_deref()),
        max_output: None,
        sandbox: child_sandbox.clone(),
    };
    let stepped = crate::playbook::StepRunner::step(&runner, spec.clone());
    let mut result = match stepped {
        Ok(r) => {
            let mut tokens = r.usage.output;
            let mut res = match &call.opts.schema {
                None => AgentResult::text(r.text.clone(), tokens),
                Some(schema) => {
                    let answered = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
                    match answered {
                        Some(v) => AgentResult::object(v, tokens),
                        // No valid call: one finalisation call constrained to the schema (a repair).
                        None => match runner.finalise(&spec, &r.text, schema) {
                            Ok((v, usage)) => {
                                tokens += usage.output;
                                let errors = marlowe_workflow::schema::validate(&v, schema);
                                if errors.is_empty() {
                                    let mut ok = AgentResult::object(v, tokens);
                                    ok.note = Some("repair: answered through the finalisation call".into());
                                    ok
                                } else {
                                    AgentResult::failed(format!("no structured answer; the finalisation call broke the schema: {}", errors.join("; ")), tokens)
                                }
                            }
                            Err(e) => AgentResult::failed(format!("no structured answer: {e}"), tokens),
                        },
                    }
                }
            };
            res.tainted = r.tainted;
            res
        }
        Err(CrewError::Cancelled) if cancel.is_cancelled() => AgentResult::skipped(),
        Err(e) => AgentResult::failed(e.to_string(), 0),
    };
    if let Some(sb) = child_sandbox {
        match sb.diff_summary() {
            Ok(d) if d.files.is_empty() => {
                let _ = sb.remove();
            }
            Ok(d) => {
                if result.status == AgentStatus::Done {
                    let note = format!(
                        "[worktree {} on branch {} has changes in {} file(s)]",
                        sb.root.display(),
                        sb.branch.clone().unwrap_or_default(),
                        d.files.len()
                    );
                    if let Value::String(s) = &mut result.value {
                        s.push_str(&format!("\n\n{note}"));
                    }
                }
            }
            Err(_) => {}
        }
    }
    finish(observer, run, call, result)
}

fn finish(observer: Option<&dyn RunObserver>, run: &RunId, call: &AgentCall, result: AgentResult) -> AgentResult {
    if let Some(o) = observer {
        o.agent_stop(run, call, &result);
    }
    result
}
