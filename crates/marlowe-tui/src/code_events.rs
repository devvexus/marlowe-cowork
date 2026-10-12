//! The reducer's half for the coding surface: daemon events that feed the panels, the todo and job
//! strips, the status line and tool details, and the answers to local [`Effect`]s.

use marlowe_model::Usage;
use marlowe_proto::{Event, Request, RewindScope};

use crate::coding::{Effect, LocalEvent, Settings, ToolDetail};
use crate::panel::Panel;
use crate::state::{Entry, NoteTone, State};

/// Output a job entry keeps; the rest is only in the job.
const JOB_OUTPUT_CAP: usize = 24_000;

/// The tools whose arguments are an edit.
fn is_edit_tool(tool: &str) -> bool {
    matches!(tool, "edit" | "multi_edit" | "write" | "str_replace" | "create" | "edit_lines" | "notebook_edit")
}

impl State {
    pub(crate) fn record_tool_start(&mut self, call_id: &str, tool: &str, summary: &str) {
        let (_, rest) = crate::state::split_summary(summary);
        let diff = if is_edit_tool(tool) && crate::diff::looks_like_diff(&rest) { crate::diff::parse_unified(&rest) } else { None };
        let d = self.code.tool_detail.entry(call_id.to_string()).or_default();
        d.tool = tool.to_string();
        d.started = summary.to_string();
        if diff.is_some() {
            d.diff = diff;
        }
    }

    pub(crate) fn record_tool_finish(&mut self, call_id: &str, ok: bool, rest: &str) {
        let d: &mut ToolDetail = self.code.tool_detail.entry(call_id.to_string()).or_default();
        d.ok = Some(ok);
        if rest.is_empty() {
            return;
        }
        d.output = rest.to_string();
        if d.diff.is_none() && crate::diff::looks_like_diff(rest) {
            d.diff = crate::diff::parse_unified(rest);
        }
        // Past a session's worth of calls the oldest detail goes.
        if self.code.tool_detail.len() > 2_000 {
            let live: std::collections::HashSet<&str> = self
                .entries
                .iter()
                .filter_map(|e| match e {
                    Entry::Tool { call_id, .. } => Some(call_id.as_str()),
                    _ => None,
                })
                .collect();
            let drop: Vec<String> = self.code.tool_detail.keys().filter(|k| !live.contains(k.as_str())).cloned().collect();
            for k in drop {
                self.code.tool_detail.remove(&k);
            }
        }
    }

    /// The live events say one line per tool call; the arguments and results (for an edit's diff
    /// and for Ctrl+O) are in the stored transcript, which the daemon saves before it says the
    /// turn is over. Asked for once the turn ends, for the calls that have no full detail.
    pub(crate) fn refresh_tool_details(&mut self, out: &mut Vec<Request>) {
        if !self.conn.is_online() || self.pending_open.is_some() || self.code.detail_refresh.is_some() {
            return;
        }
        let Some(session) = self.session.clone() else { return };
        let thin = self.entries.iter().any(|e| match e {
            Entry::Tool { call_id, .. } => !self.code.tool_detail.get(call_id).is_some_and(|d| d.full),
            _ => false,
        });
        if thin {
            self.code.detail_refresh = Some(session.clone());
            out.push(Request::OpenSession { session });
        }
    }

    /// Takes a stored transcript's tool arguments and results into the details, leaving the
    /// conversation alone. A call the transcript does not hold has nothing more to learn.
    pub(crate) fn merge_tool_details(&mut self, turns: &[marlowe_proto::StoredTurn]) {
        for (id, mut d) in crate::state::transcript_details(turns) {
            // A diff the live summary already carried is the more exact one.
            if let Some(old) = self.code.tool_detail.get(&id) {
                if d.diff.is_none() {
                    d.diff = old.diff.clone();
                }
            }
            self.code.tool_detail.insert(id, d);
        }
        let ids: Vec<String> = self
            .entries
            .iter()
            .filter_map(|e| match e {
                Entry::Tool { call_id, .. } => Some(call_id.clone()),
                _ => None,
            })
            .collect();
        for id in ids {
            self.code.tool_detail.entry(id).or_default().full = true;
        }
    }

    pub(crate) fn add_totals(&mut self, usage: &Usage) {
        let t = &mut self.code.totals;
        t.input += usage.input;
        t.output += usage.output;
        t.usd += usage.cost_usd.unwrap_or(0.0);
        t.turns += 1;
        self.code.statusline.dirty = true;
    }

    /// A session became this client's: send what was chosen before it existed.
    pub(crate) fn session_adopted(&mut self, out: &mut Vec<Request>) {
        let Some(session) = self.session.clone() else { return };
        if let Some(mode) = self.code.pending_mode.take() {
            out.push(Request::SetMode { session: session.clone(), mode });
        }
        if let Some(cmd) = self.code.pending_shell.take() {
            self.pending_user = None;
            out.push(Request::UserShell { session, command: cmd });
        }
        self.code.statusline.dirty = true;
    }

    /// Run-loop work that is not a reaction to an event or a key: the requests that follow a
    /// connection, the status line's debounce.
    pub fn on_idle(&mut self) -> Vec<Request> {
        let mut out = Vec::new();
        if !self.conn.is_online() {
            return out;
        }
        let ws = self.shown_workspace().map(str::to_string);
        if !self.code.bootstrapped || self.code.boot_ws != ws {
            self.code.bootstrapped = true;
            self.code.boot_ws = ws.clone();
            self.code.quiet_ops = 2;
            out.push(Request::GetSettings { workspace: ws.clone() });
            out.push(Request::ListCommands { workspace: ws });
        }
        self.statusline_tick();
        self.closing_tick(&mut out);
        out
    }

    /// The sessions the picker lists: all of them, or those whose title or directory has every
    /// word typed.
    pub fn session_rows(&self) -> Vec<&marlowe_proto::SessionSummary> {
        let words: Vec<String> = self.code.sessions_query.split_whitespace().map(str::to_lowercase).collect();
        self.sessions
            .iter()
            .flatten()
            .filter(|s| {
                let hay = format!("{} {}", s.title, s.workspace.as_deref().unwrap_or("")).to_lowercase();
                words.iter().all(|w| hay.contains(w.as_str()))
            })
            .collect()
    }

    /// Work for the run loop, taken once.
    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.code.effects)
    }

    /// The JSON the status-line command reads on stdin.
    pub fn statusline_input(&self) -> String {
        let model = self.live.as_ref().map(|l| l.model.clone()).or_else(|| self.library.conductor());
        let display = model
            .as_ref()
            .map(|m| crate::text::short_model(m, self.library.models.iter().find(|x| &x.model == m).map(|x| &x.info)))
            .unwrap_or_default();
        let (used, window) = self.context.usage.as_ref().map_or((0, 0), |(_, t, w)| (*t, *w));
        let pct = if window > 0 { (used as f64 / f64::from(window) * 100.0).round() as u64 } else { 0 };
        serde_json::json!({
            "session_id": self.session.as_ref().map(|s| s.as_str()).unwrap_or(""),
            "model": { "id": model.as_ref().map(|m| m.model_id.clone()).unwrap_or_default(), "display_name": display },
            "workspace": { "current_dir": self.shown_workspace().unwrap_or("") },
            "mode": self.code.mode.as_str(),
            "cost": {
                "total_cost_usd": self.code.totals.usd,
                "total_input_tokens": self.code.totals.input,
                "total_output_tokens": self.code.totals.output,
            },
            "context": { "used_tokens": used, "window": window, "used_percentage": pct },
            "todos": { "open": self.code.open_todos(), "total": self.code.todos.len() },
            "jobs": { "running": self.code.running_jobs() },
            "version": env!("CARGO_PKG_VERSION"),
        })
        .to_string()
    }

    /// Runs the status-line command when its input changed and 300 ms have passed since the last.
    fn statusline_tick(&mut self) {
        let Some(command) = self.code.statusline_command().map(str::to_string) else {
            self.code.statusline.text = None;
            return;
        };
        let sl = &self.code.statusline;
        if sl.running || !(sl.dirty || sl.text.is_none() && sl.error.is_none()) || self.now_ms - sl.last_run_ms < 300 {
            return;
        }
        let input = self.statusline_input();
        let sl = &mut self.code.statusline;
        sl.dirty = false;
        if sl.last_input == input && sl.text.is_some() {
            return;
        }
        sl.last_input = input.clone();
        sl.running = true;
        sl.last_run_ms = self.now_ms;
        let cwd = self.shown_workspace().map(str::to_string);
        self.code.effects.push(Effect::Statusline { command, input, cwd });
    }

    /// The answer to a local [`Effect`].
    pub(crate) fn local_event(&mut self, ev: LocalEvent, out: &mut Vec<Request>) {
        match ev {
            LocalEvent::Statusline(r) => {
                let sl = &mut self.code.statusline;
                sl.running = false;
                match r {
                    Ok(t) => {
                        sl.text = Some(t).filter(|t| !t.trim().is_empty());
                        sl.error = None;
                    }
                    Err(e) => {
                        sl.text = None;
                        sl.error = Some(e);
                    }
                }
            }
            LocalEvent::FileIndex { root, files } => {
                if self.code.index_root.as_deref() == Some(root.as_str()) {
                    self.code.file_index = Some(files);
                }
            }
            LocalEvent::Image { source, result } => self.image_read(source, result),
            LocalEvent::EditorClosed { path, result } => self.editor_closed(&path, &result, out),
            LocalEvent::Written { path, result } => match result {
                Ok(()) => self.flash(format!("wrote {}", crate::text::short_path(&path)), NoteTone::Dim),
                Err(e) => self.flash(format!("could not write {}: {}", crate::text::short_path(&path), crate::text::clean(&e)), NoteTone::Amber),
            },
            LocalEvent::Instructions(files) => {
                self.code.instructions = Some(files);
                self.clamp_panel();
            }
        }
    }

    /// The M4.6 events (everything `State::event` does not know by name).
    pub(crate) fn coding_event(&mut self, ev: &Event, reply: bool, out: &mut Vec<Request>) {
        match ev {
            Event::ModeChanged { session, mode } if self.mine(session) => {
                self.code.mode = *mode;
                self.code.statusline.dirty = true;
            }
            Event::Todos { session, items } if self.mine(session) => self.code.todos = items.clone(),
            Event::QuestionAsked { session, id, questions } if self.mine(session) => self.ask_questions(session, id, questions),
            Event::Rules { rules } => {
                self.code.rules = Some(rules.clone());
                self.clamp_panel();
            }
            Event::Hooks { hooks, digest } => {
                self.code.hooks = Some(hooks.clone());
                if !digest.is_empty() {
                    self.set_hooks_digest(digest);
                }
                self.clamp_panel();
            }
            Event::Settings { layers } => {
                let s = Settings::from_layers(layers);
                self.code.links.editor_url = s.user_str("editor_url").map(str::to_string);
                if self.code.mode == marlowe_proto::PermissionMode::Default && self.session.is_none() {
                    if let Some(m) = s.str("permissions.default_mode").and_then(crate::slash::parse_mode) {
                        self.code.mode = m;
                    }
                }
                self.code.settings = Some(s);
                self.code.statusline.dirty = true;
                self.clamp_panel();
            }
            Event::Commands { list } => self.code.commands = list.clone(),
            Event::AgentTypes { list } => {
                self.code.agents = Some(list.clone());
                self.clamp_panel();
            }
            Event::McpServers { list } => {
                self.code.mcp = Some(list.clone());
                self.clamp_panel();
            }
            Event::Checkpoints { session, list } if self.mine(session) => {
                self.code.checkpoints = Some(list.clone());
                self.clamp_panel();
            }
            Event::SessionCost { session, report } if self.mine(session) => self.code.cost = Some(report.clone()),
            Event::SessionDiff { session, diff, .. } if self.mine(session) => {
                if let Some(Panel::Diff(d)) = self.code.panel.as_mut() {
                    d.loading = false;
                    d.diff = crate::diff::parse_unified(diff);
                    d.raw = if d.diff.is_none() { diff.clone() } else { String::new() };
                }
            }
            Event::SessionExported { session, format, text } if self.mine(session) => {
                let ext = match format {
                    marlowe_proto::ExportFormat::Markdown => "md",
                    marlowe_proto::ExportFormat::Json => "json",
                };
                let dir = self.shown_workspace().unwrap_or(".").trim_end_matches(['/', '\\']).to_string();
                let short: String = session.as_str().chars().take(8).collect();
                self.code.effects.push(Effect::WriteFile { path: format!("{dir}/marlowe-session-{short}.{ext}"), text: text.clone() });
            }
            Event::Rewound { session, turn, scope } if self.mine(session) => {
                let what = match scope {
                    RewindScope::Code => "code",
                    RewindScope::Conversation => "conversation",
                    RewindScope::Both => "code and conversation",
                };
                if *scope != RewindScope::Code {
                    self.reload(out);
                }
                self.flash(format!("rewound {what} to turn {turn}"), NoteTone::Dim);
            }
            Event::Schedules { list } => {
                if list.is_empty() {
                    self.note("no scheduled prompts · /loop 10m <prompt> adds one", NoteTone::Dim);
                }
                for s in list {
                    let every = crate::slash::interval_label(s.every_secs);
                    self.note(&format!("every {every} · {}", crate::text::truncate(&crate::text::clean(&s.text), 80)), NoteTone::Dim);
                }
            }
            Event::PrOpened { url, .. } => {
                self.flash(format!("pull request opened: {}", crate::text::clean(url)), NoteTone::Dim);
                self.note(&format!("pull request opened: {}", crate::text::clean(url)), NoteTone::Dim);
            }
            Event::Jobs { list } => {
                self.code.jobs = list.iter().filter(|j| j.session.is_none() || j.session == self.session).cloned().collect();
                self.clamp_panel();
            }
            Event::JobStarted { job } if job.session.is_none() || job.session == self.session => {
                if !self.code.jobs.iter().any(|j| j.id == job.id) {
                    self.code.jobs.push(job.clone());
                    self.entries.push(Entry::Job { id: job.id.clone(), command: job.command.clone(), output: String::new(), code: None });
                    self.code.statusline.dirty = true;
                }
            }
            Event::JobOutput { session, id, text } if session.is_none() || *session == self.session => {
                if let Some(Entry::Job { output, .. }) = self.entries.iter_mut().rev().find(|e| matches!(e, Entry::Job { id: i, .. } if i == id)) {
                    output.push_str(text);
                    if output.len() > JOB_OUTPUT_CAP {
                        let cut = output.len() - JOB_OUTPUT_CAP;
                        let at = (cut..output.len()).find(|i| output.is_char_boundary(*i)).unwrap_or(output.len());
                        output.drain(..at);
                    }
                }
            }
            Event::JobExited { session, id, code } if session.is_none() || *session == self.session => {
                if let Some(j) = self.code.jobs.iter_mut().find(|j| &j.id == id) {
                    j.exit_code = Some(*code);
                }
                if let Some(Entry::Job { code: c, .. }) = self.entries.iter_mut().rev().find(|e| matches!(e, Entry::Job { id: i, .. } if i == id)) {
                    *c = Some(*code);
                }
                self.code.statusline.dirty = true;
            }
            Event::BlobStored { blob, mime, w, h } => {
                if let Some(up) = self.code.upload.as_mut() {
                    if let Some(ask) = up.stored(blob, mime, *w, *h) {
                        self.code.upload = None;
                        out.push(ask);
                        // More queued while the model works go in behind it.
                        if self.is_busy() {
                            self.flush_queue(out);
                        }
                    }
                }
            }
            _ => {}
        }
        let _ = reply;
    }

    /// A seam stub answered: this daemon does not do that yet. Said once per request kind and in
    /// amber, never red, and silently for what the client asked on its own at connect.
    pub(crate) fn unavailable(&mut self, message: &str) {
        let op = message.split('`').nth(1).unwrap_or("that").to_string();
        if self.code.quiet_ops > 0 {
            self.code.quiet_ops -= 1;
            self.code.unavailable.insert(op);
            return;
        }
        if self.code.unavailable.insert(op.clone()) {
            self.flash(format!("this daemon does not answer {op} yet"), NoteTone::Amber);
        }
    }
}
