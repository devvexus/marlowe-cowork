//! Workflow scripts are checked as they are written: after a successful call that changed a
//! `*.star` file (write, edit, multi_edit, edit_lines), the validator runs on the new text and
//! its errors are appended to the tool result, as code diagnostics are after an edit. A script
//! that validates counts as checked for the "you changed files, run checks" reminder.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use marlowe_agent::todo::VerifyState;
use marlowe_agent::{CallCtx, Mutation, ToolMiddleware, ToolOutcome};
use marlowe_model::ToolCall;

/// A call's `.star` targets, by call id, from `pre_mutation` to `after_call`.
#[derive(Default)]
pub struct StarValidator {
    pending: Mutex<HashMap<String, Vec<PathBuf>>>,
}

fn is_star(p: &std::path::Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("star"))
}

/// The validator's verdict on one file: `Ok` when it is a valid workflow, else its errors.
pub(super) fn verdict(path: &std::path::Path) -> Result<(), String> {
    let source = std::fs::read_to_string(path).map_err(|e| format!("cannot read it back: {e}"))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    marlowe_workflow::check(&name, &source).map(|_| ()).map_err(|d| marlowe_workflow::validate::render(&d))
}

impl ToolMiddleware for StarValidator {
    fn name(&self) -> &str {
        "star-validator"
    }

    fn pre_mutation(&self, call: &ToolCall, _cx: &CallCtx, mutation: &Mutation) -> Result<(), ToolOutcome> {
        if let Mutation::Paths(paths) = mutation {
            let stars: Vec<PathBuf> = paths.iter().filter(|p| is_star(p)).cloned().collect();
            if !stars.is_empty() {
                self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(call.id.clone(), stars);
            }
        }
        Ok(())
    }

    fn after_call(&self, call: &ToolCall, cx: &CallCtx, outcome: &mut ToolOutcome) {
        let Some(files) = self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&call.id) else { return };
        if outcome.is_error {
            return;
        }
        for f in files {
            match verdict(&f) {
                Ok(()) => {
                    // A script that validates has been checked: the stop check lets it go.
                    cx.scope.with_ext(|e| {
                        e.get_or_default::<VerifyState>().dirty.remove(&f);
                    });
                    outcome.content.push_str(&format!("\n{} is a valid workflow.", f.display()));
                }
                Err(errors) => outcome.content.push_str(&format!(
                    "\n<workflow-errors>\n{} is not a valid workflow; fix these (the workflow tool refuses it until you do):\n{errors}\n</workflow-errors>",
                    f.display()
                )),
            }
        }
    }
}
