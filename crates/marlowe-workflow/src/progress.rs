//! The progress model the `/workflows` view renders (WF7): phases, agents with their label,
//! status, tokens and elapsed time, and the `log()` tail, folded from the run's events.

use std::collections::HashMap;
use std::time::Instant;

use marlowe_proto::{RunId, WorkflowAgentInfo, WorkflowAgentStatus, WorkflowInfo};

use crate::host::{AgentStatus, WfEvent};

/// Log lines the view keeps (the journal keeps all).
pub const LOG_TAIL: usize = 200;

/// Folds events into a [`WorkflowInfo`].
pub struct Progress {
    info: WorkflowInfo,
    started: HashMap<u32, Instant>,
}

impl Progress {
    pub fn new(run: RunId, name: impl Into<String>, phases: Vec<String>, budget_total: Option<u64>) -> Self {
        Progress {
            info: WorkflowInfo { run, name: name.into(), phases, phase: None, agents: Vec::new(), log: Vec::new(), budget_total, budget_spent: 0 },
            started: HashMap::new(),
        }
    }

    /// Applies one event; `true` when the view changed.
    pub fn apply(&mut self, ev: &WfEvent) -> bool {
        match ev {
            WfEvent::Phase(p) => {
                self.info.phase = Some(p.clone());
                if !self.info.phases.contains(p) {
                    self.info.phases.push(p.clone());
                }
            }
            WfEvent::Log(l) => {
                self.info.log.push(l.clone());
                if self.info.log.len() > LOG_TAIL {
                    let cut = self.info.log.len() - LOG_TAIL;
                    self.info.log.drain(..cut);
                }
            }
            WfEvent::Queued { call } => {
                if !self.info.agents.iter().any(|a| a.index == call.index) {
                    self.info.agents.push(WorkflowAgentInfo {
                        index: call.index,
                        label: call.display_label(),
                        phase: call.opts.phase.clone(),
                        status: WorkflowAgentStatus::Queued,
                        tokens: 0,
                        elapsed_ms: 0,
                    });
                }
            }
            WfEvent::Started { index } => {
                self.started.insert(*index, Instant::now());
                if let Some(a) = self.agent(*index) {
                    a.status = WorkflowAgentStatus::Running;
                }
            }
            WfEvent::Finished { call, result, .. } => {
                let elapsed = self.started.remove(&call.index).map(|t| t.elapsed().as_millis() as u64).unwrap_or(0);
                self.info.budget_spent += result.tokens;
                if let Some(a) = self.agent(call.index) {
                    a.status = match result.status {
                        AgentStatus::Done => WorkflowAgentStatus::Done,
                        AgentStatus::Failed => WorkflowAgentStatus::Failed,
                        AgentStatus::Skipped => WorkflowAgentStatus::Skipped,
                    };
                    a.tokens = result.tokens;
                    a.elapsed_ms = elapsed;
                }
            }
        }
        true
    }

    fn agent(&mut self, index: u32) -> Option<&mut WorkflowAgentInfo> {
        self.info.agents.iter_mut().find(|a| a.index == index)
    }

    /// The view now, with running agents' elapsed time up to date.
    pub fn snapshot(&self) -> WorkflowInfo {
        let mut info = self.info.clone();
        for a in &mut info.agents {
            if a.status == WorkflowAgentStatus::Running {
                if let Some(t) = self.started.get(&a.index) {
                    a.elapsed_ms = t.elapsed().as_millis() as u64;
                }
            }
        }
        info
    }

    /// Sets the pool's spend (the session's whole pool, not only this run).
    pub fn set_budget_spent(&mut self, spent: u64) {
        self.info.budget_spent = spent;
    }
}
