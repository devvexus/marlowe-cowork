//! The installed Microsoft Edge as the `browser` verification tool's driver.
//!
//! One run is one headless Edge in a fresh profile inside a kill-on-close Job Object (see the
//! parent module): it opens the page, collects console messages, uncaught exceptions, browser log
//! entries and failed requests as they happen, runs the request's steps, then reads the page
//! outline and takes the screenshot.
//!
//! What the page may reach is the request's [`Reach`](marlowe_verify::browser::Reach), enforced
//! twice: Edge's DNS is pointed nowhere but the allowed names (`--host-resolver-rules`), and every
//! request the page makes, including `eval`, iframes, redirects and `goto`, is paused through the
//! DevTools Fetch domain and failed unless the reach allows it. What was refused shows in the
//! report instead of quietly leaving the machine or reading a file outside the workspace.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use headless_chrome::browser::tab::RequestPausedDecision;
use headless_chrome::protocol::cdp::types::Event;
use headless_chrome::protocol::cdp::{Emulation, Fetch, Network, Page, Runtime};
use headless_chrome::{Browser as Chrome, Tab};
use marlowe_model::CancelToken;
use marlowe_verify::browser::{
    BrowseError, BrowseReport, BrowseRequest, Browser, ConsoleEntry, FailedRequest, Level, Step, StepResult,
};
use serde_json::Value;

use super::{acquire, devtools_url, find_edge, spawn_edge, Profile, RenderError};

/// Console entries kept per run.
const MAX_CONSOLE: usize = 400;
/// After a step, how long the network must be quiet before the next one.
const QUIET: Duration = Duration::from_millis(350);
/// Longest wait for one step.
const STEP_TIMEOUT: Duration = Duration::from_secs(6);
/// Time kept back from the run's budget so a failed step still returns the console and the page.
const RESERVE: Duration = Duration::from_secs(2);

/// Edge as a [`Browser`].
#[derive(Debug, Clone)]
pub struct EdgeBrowser {
    pub edge: PathBuf,
    /// How long a page may keep loading after navigation before it is read anyway.
    pub settle: Duration,
}

impl EdgeBrowser {
    /// The installed Edge, or `None` (`MARLOWE_EDGE` overrides the location).
    pub fn find() -> Option<Self> {
        find_edge().map(|edge| Self { edge, settle: Duration::from_secs(4) })
    }
}

impl Browser for EdgeBrowser {
    fn name(&self) -> String {
        "Microsoft Edge".into()
    }

    fn available(&self) -> bool {
        self.edge.is_file()
    }

    fn run(&self, req: &BrowseRequest, cancel: &CancelToken) -> Result<BrowseReport, BrowseError> {
        if !self.edge.is_file() {
            return Err(BrowseError::NoBrowser);
        }
        let deadline = Instant::now() + req.timeout;
        let _slot = acquire(cancel, deadline).map_err(error)?;
        let profile = Profile::new().map_err(error)?;
        let mut extra = vec![
            format!("--window-size={},{}", req.viewport.0, req.viewport.1),
            "--force-device-scale-factor=1".to_string(),
            "--hide-scrollbars".to_string(),
        ];
        extra.push(format!("--host-resolver-rules={}", req.reach.resolver_rules()));
        let mut child = spawn_edge(&self.edge, &profile, &extra).map_err(error)?;
        let result = drive(&profile.0, req, self.settle, deadline, cancel);
        child.kill_tree();
        let _ = child.child_mut().wait();
        drop(profile);
        result
    }
}

fn error(e: RenderError) -> BrowseError {
    match e {
        RenderError::NoBrowser => BrowseError::NoBrowser,
        RenderError::Launch(m) => BrowseError::Launch(m),
        RenderError::Timeout(d) => BrowseError::Timeout(d),
        RenderError::Cancelled => BrowseError::Cancelled,
        RenderError::Navigation(m) => BrowseError::Navigation(m),
        RenderError::Refused => BrowseError::Navigation("the page ended on a URL it may not reach".into()),
    }
}

fn drive(
    profile: &std::path::Path,
    req: &BrowseRequest,
    settle: Duration,
    deadline: Instant,
    cancel: &CancelToken,
) -> Result<BrowseReport, BrowseError> {
    let ws = devtools_url(profile, deadline, cancel).map_err(error)?;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let req_owned = req.clone();
    let budget = deadline.saturating_duration_since(Instant::now());
    std::thread::Builder::new()
        .name("marlowe-browse".into())
        .spawn(move || {
            let _ = tx.send(browse(&ws, &req_owned, settle, budget));
        })
        .map_err(|e| BrowseError::Launch(e.to_string()))?;
    match crate::web::wait_for(&rx, cancel, deadline) {
        Ok(r) => r,
        Err(crate::web::Stopped::Cancelled) => Err(BrowseError::Cancelled),
        Err(_) => Err(BrowseError::Timeout(budget)),
    }
}

// ---- what the page reports ---------------------------------------------------------------------

#[derive(Default)]
struct Collector {
    console: Mutex<Vec<ConsoleEntry>>,
    failed: Mutex<Vec<FailedRequest>>,
    urls: Mutex<HashMap<String, String>>,
    /// Requests the reach refused, by url, with why.
    blocked: Mutex<HashMap<String, String>>,
    inflight: AtomicI64,
    last_event: Mutex<Option<Instant>>,
}

/// A request Edge makes on its own for every page; its 404 says nothing about the app.
fn noise(url: &str) -> bool {
    url.split(['?', '#']).next().is_some_and(|u| u.ends_with("/favicon.ico")) || url.starts_with("data:")
}

fn remote_text(o: &Runtime::RemoteObject) -> String {
    match &o.value {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => o
            .unserializable_value
            .clone()
            .or_else(|| o.description.clone())
            .or_else(|| o.class_name.clone())
            .unwrap_or_else(|| format!("{:?}", o.Type).to_ascii_lowercase()),
    }
}

fn loc(url: &str, line: u32, col: Option<u32>) -> Option<String> {
    if url.is_empty() {
        return None;
    }
    let name = url.rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or(url);
    Some(match col {
        Some(c) => format!("{name}:{}:{}", line + 1, c + 1),
        None => format!("{name}:{}", line + 1),
    })
}

impl Collector {
    fn touch(&self) {
        *self.last_event.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
    }

    fn quiet_for(&self) -> Duration {
        self.last_event.lock().unwrap_or_else(|p| p.into_inner()).map(|t| t.elapsed()).unwrap_or(Duration::from_secs(3600))
    }

    fn console(&self, level: Level, text: String, location: Option<String>) {
        let mut g = self.console.lock().unwrap_or_else(|p| p.into_inner());
        if g.len() < MAX_CONSOLE {
            g.push(ConsoleEntry { level, text, location });
        }
    }

    fn fail(&self, url: String, reason: String, status: Option<u16>) {
        if noise(&url) {
            return;
        }
        let mut g = self.failed.lock().unwrap_or_else(|p| p.into_inner());
        if g.len() < 200 && !g.iter().any(|f| f.url == url && f.reason == reason) {
            g.push(FailedRequest { url, reason, status });
        }
    }

    /// A request the page's reach refused: reported as a failed request with the reason.
    fn block(&self, url: &str, why: &str) {
        self.blocked.lock().unwrap_or_else(|p| p.into_inner()).insert(url.to_string(), why.to_string());
        self.fail(url.to_string(), format!("net::ERR_BLOCKED_BY_CLIENT ({why})"), None);
    }

    fn on_event(&self, ev: &Event) {
        use headless_chrome::protocol::cdp::Log::LogEntryLevel;
        use headless_chrome::protocol::cdp::Runtime::ConsoleAPICalledEventTypeOption as T;
        match ev {
            Event::RuntimeConsoleAPICalled(e) => {
                self.touch();
                let p = &e.params;
                let level = match p.Type {
                    T::Error | T::Assert => Level::Error,
                    T::Warning => Level::Warning,
                    T::Info => Level::Info,
                    T::Debug => Level::Debug,
                    T::Log | T::Dir | T::Dirxml | T::Table | T::Trace => Level::Log,
                    _ => return,
                };
                let text = p.args.iter().map(remote_text).collect::<Vec<_>>().join(" ");
                let location = p.stack_trace.as_ref().and_then(|s| s.call_frames.first()).and_then(|f| loc(&f.url, f.line_number, Some(f.column_number)));
                self.console(level, text, location);
            }
            Event::RuntimeExceptionThrown(e) => {
                self.touch();
                let d = &e.params.exception_details;
                let described = d.exception.as_ref().and_then(|x| x.description.clone()).filter(|t| !t.is_empty());
                let first = described.as_deref().and_then(|t| t.lines().next()).unwrap_or(&d.text).to_string();
                let text = if first.starts_with("Uncaught") { first } else { format!("Uncaught {first}") };
                self.console(Level::Error, text, d.url.as_deref().and_then(|u| loc(u, d.line_number, Some(d.column_number))));
            }
            Event::LogEntryAdded(e) => {
                self.touch();
                let en = &e.params.entry;
                if en.url.as_deref().is_some_and(noise) {
                    return;
                }
                let level = match en.level {
                    LogEntryLevel::Verbose => Level::Debug,
                    LogEntryLevel::Info => Level::Info,
                    LogEntryLevel::Warning => Level::Warning,
                    LogEntryLevel::Error => Level::Error,
                };
                let location = en.url.as_deref().and_then(|u| loc(u, en.line_number.unwrap_or(0).saturating_sub(1), None));
                self.console(level, en.text.clone(), location);
            }
            Event::NetworkRequestWillBeSent(e) => {
                self.touch();
                let r = &e.params.request;
                if !r.url.starts_with("data:") {
                    self.inflight.fetch_add(1, Ordering::SeqCst);
                    self.urls.lock().unwrap_or_else(|p| p.into_inner()).insert(e.params.request_id.clone(), r.url.clone());
                }
            }
            Event::NetworkLoadingFinished(_) => {
                self.touch();
                let n = self.inflight.fetch_sub(1, Ordering::SeqCst);
                if n <= 0 {
                    self.inflight.store(0, Ordering::SeqCst);
                }
            }
            Event::NetworkLoadingFailed(e) => {
                self.touch();
                let n = self.inflight.fetch_sub(1, Ordering::SeqCst);
                if n <= 0 {
                    self.inflight.store(0, Ordering::SeqCst);
                }
                if e.params.canceled != Some(true) {
                    let url = self.urls.lock().unwrap_or_else(|p| p.into_inner()).get(&e.params.request_id).cloned().unwrap_or_default();
                    // Already reported with its reason when the reach refused it.
                    let refused = e.params.error_text.contains("ERR_BLOCKED_BY_CLIENT") && self.blocked.lock().unwrap_or_else(|p| p.into_inner()).contains_key(&url);
                    if !refused {
                        self.fail(url, e.params.error_text.clone(), None);
                    }
                }
            }
            Event::NetworkResponseReceived(e) => {
                self.touch();
                let r = &e.params.response;
                if r.status >= 400 {
                    self.fail(r.url.clone(), format!("HTTP {}", r.status), Some(r.status as u16));
                }
            }
            _ => {}
        }
    }
}

// ---- driving the page --------------------------------------------------------------------------

fn browse(ws: &str, req: &BrowseRequest, settle: Duration, budget: Duration) -> Result<BrowseReport, BrowseError> {
    let started = Instant::now();
    // Steps get the budget less a reserve, so what the page reported is still collected.
    let end = started + budget.saturating_sub(RESERVE).max(RESERVE);
    let chrome = Chrome::connect_with_timeout(ws.to_string(), budget).map_err(|e| BrowseError::Launch(short(&e)))?;
    let tab = chrome.new_tab().map_err(|e| BrowseError::Launch(short(&e)))?;
    let _ = tab.call_method(Page::SetDownloadBehavior {
        behavior: Page::SetDownloadBehaviorBehaviorOption::Deny,
        download_path: None,
    });
    tab.set_default_timeout(budget);
    let collector = Arc::new(Collector::default());
    let c = Arc::clone(&collector);
    let weak = Arc::downgrade(&tab);
    tab.add_event_listener(Arc::new(move |ev: &Event| {
        c.on_event(ev);
        // An alert() would freeze the page's script for good; answer it and note it.
        if let Event::PageJavascriptDialogOpening(d) = ev {
            c.console(Level::Warning, format!("the page opened a dialog and it was accepted: \"{}\"", d.params.message), None);
            if let Some(tab) = weak.upgrade() {
                let _ = tab.call_method(Page::HandleJavaScriptDialog { accept: true, prompt_text: None });
            }
        }
    }))
    .map_err(setup)?;
    // Every request the page makes passes the reach first.
    let reach = req.reach.clone();
    let seen = Arc::clone(&collector);
    tab.enable_request_interception(Arc::new(move |_t, _s, ev: Fetch::events::RequestPausedEvent| {
        let r = &ev.params.request;
        match reach.check(&r.method, &r.url) {
            Ok(()) => RequestPausedDecision::Continue(None),
            Err(why) => {
                seen.block(&r.url, &why);
                RequestPausedDecision::Fail(Fetch::FailRequest { request_id: ev.params.request_id.clone(), error_reason: Network::ErrorReason::BlockedByClient })
            }
        }
    }))
    .map_err(setup)?;
    tab.enable_fetch(Some(&[Fetch::RequestPattern { url_pattern: Some("*".into()), resource_Type: None, request_stage: Some(Fetch::RequestStage::Request) }]), None).map_err(setup)?;
    tab.enable_runtime().map_err(setup)?;
    tab.enable_log().map_err(setup)?;
    tab.call_method(Network::Enable {
        max_total_buffer_size: None,
        max_resource_buffer_size: None,
        max_post_data_size: None,
        report_direct_socket_traffic: None,
        enable_durable_messages: None,
    })
    .map_err(setup)?;
    tab.call_method(Emulation::SetDeviceMetricsOverride {
        width: req.viewport.0,
        height: req.viewport.1,
        device_scale_factor: 1.0,
        mobile: false,
        scale: None,
        screen_width: None,
        screen_height: None,
        position_x: None,
        position_y: None,
        dont_set_visible_size: None,
        screen_orientation: None,
        viewport: None,
        display_feature: None,
        device_posture: None,
    })
    .map_err(setup)?;

    let nav_started = Instant::now();
    tab.navigate_to(&req.url).map_err(|e| BrowseError::Navigation(short(&e)))?.wait_until_navigated().map_err(|e| BrowseError::Navigation(short(&e)))?;
    let mut report = BrowseReport::default();
    settle_page(&tab, &collector, nav_started + settle.max(Duration::from_millis(500)).min(end.saturating_duration_since(nav_started)));
    report.load_ms = nav_started.elapsed().as_millis() as u64;

    if let Some(w) = &req.wait_for {
        let r = wait_for(&tab, w, step_time(end));
        report.steps.push(StepResult { raw: format!("wait_for {w}"), ok: r.is_ok(), detail: r.err().unwrap_or_default() });
    }
    if report.steps.iter().all(|s| s.ok) {
        for step in &req.steps {
            let raw = step.to_string();
            let r = run_step(&tab, step, req, step_time(end));
            let ok = r.is_ok();
            let detail = match r {
                Ok(d) | Err(d) => d,
            };
            report.steps.push(StepResult { raw, ok, detail });
            if !ok || Instant::now() >= end {
                break;
            }
            settle_page(&tab, &collector, Instant::now() + Duration::from_secs(2));
        }
    }

    report.final_url = tab.get_url();
    report.title = tab.get_title().unwrap_or_default();
    if req.snapshot {
        report.snapshot = eval(&tab, SNAPSHOT_JS, false).ok().and_then(|v| v.as_str().map(String::from));
    }
    if req.screenshot {
        report.screenshot = screenshot(&tab, req).ok();
    }
    // Late errors (a timer that throws a moment after load) still belong to the page.
    std::thread::sleep(Duration::from_millis(150));
    report.console = collector.console.lock().unwrap_or_else(|p| p.into_inner()).clone();
    report.failed_requests = collector.failed.lock().unwrap_or_else(|p| p.into_inner()).clone();
    Ok(report)
}

fn setup<E: std::fmt::Display>(e: E) -> BrowseError {
    BrowseError::Launch(short(&e))
}

fn step_time(end: Instant) -> Duration {
    end.saturating_duration_since(Instant::now()).min(STEP_TIMEOUT).max(Duration::from_millis(500))
}

fn short(e: &dyn std::fmt::Display) -> String {
    let s = e.to_string();
    let line = s.lines().next().unwrap_or("");
    line[..crate::text::floor_boundary(line, 240)].to_string()
}

/// Waits until the page has loaded and the network has been quiet for [`QUIET`], or `until`.
fn settle_page(tab: &Tab, c: &Collector, until: Instant) {
    loop {
        let ready = eval(tab, "document.readyState", false).ok().and_then(|v| v.as_str().map(|s| s == "complete")).unwrap_or(false);
        if ready && c.inflight.load(Ordering::SeqCst) <= 0 && c.quiet_for() >= QUIET {
            return;
        }
        if Instant::now() >= until {
            return;
        }
        std::thread::sleep(Duration::from_millis(60));
    }
}

/// Evaluates `expr` (promises awaited when asked) and returns its value; an exception is an error.
fn eval(tab: &Tab, expr: &str, await_promise: bool) -> Result<Value, String> {
    let r = tab
        .call_method(Runtime::Evaluate {
            expression: expr.to_string(),
            object_group: None,
            include_command_line_api: None,
            silent: Some(false),
            context_id: None,
            return_by_value: Some(true),
            generate_preview: None,
            user_gesture: Some(true),
            await_promise: Some(await_promise),
            throw_on_side_effect: None,
            timeout: None,
            disable_breaks: None,
            repl_mode: None,
            allow_unsafe_eval_blocked_by_csp: None,
            unique_context_id: None,
            serialization_options: None,
        })
        .map_err(|e| short(&e))?;
    if let Some(d) = r.exception_details {
        let text = d.exception.as_ref().and_then(|x| x.description.clone()).unwrap_or(d.text);
        return Err(text.lines().next().unwrap_or("exception").to_string());
    }
    Ok(r.result.value.unwrap_or_else(|| r.result.description.map(Value::String).unwrap_or(Value::Null)))
}

fn show(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "undefined".into(),
        other => other.to_string(),
    }
}

fn js(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

fn poll(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + timeout;
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(80));
    }
}

fn wait_for(tab: &Tab, what: &str, timeout: Duration) -> Result<(), String> {
    if let Some(text) = what.strip_prefix("text:") {
        wait_text(tab, text.trim(), timeout)
    } else {
        wait_selector(tab, what, timeout)
    }
}

fn wait_selector(tab: &Tab, sel: &str, timeout: Duration) -> Result<(), String> {
    let expr = format!("!!document.querySelector({})", js(sel));
    if poll(timeout, || eval(tab, &expr, false).ok().and_then(|v| v.as_bool()).unwrap_or(false)) {
        Ok(())
    } else {
        Err(format!("no element matches `{sel}` after {} s", timeout.as_secs()))
    }
}

fn wait_text(tab: &Tab, text: &str, timeout: Duration) -> Result<(), String> {
    let expr = format!("!!(document.body && document.body.innerText.includes({}))", js(text));
    if poll(timeout, || eval(tab, &expr, false).ok().and_then(|v| v.as_bool()).unwrap_or(false)) {
        Ok(())
    } else {
        Err(format!("the text \"{text}\" did not appear within {} s", timeout.as_secs()))
    }
}

fn run_step(tab: &Arc<Tab>, step: &Step, req: &BrowseRequest, timeout: Duration) -> Result<String, String> {
    match step {
        Step::Click(sel) => {
            wait_selector(tab, sel, timeout)?;
            let el = tab.find_element(sel).map_err(|e| format!("no element matches `{sel}`: {}", short(&e)))?;
            el.click().map_err(|e| format!("could not click `{sel}`: {}", short(&e)))?;
            Ok(String::new())
        }
        Step::Hover(sel) => {
            wait_selector(tab, sel, timeout)?;
            let el = tab.find_element(sel).map_err(|e| format!("no element matches `{sel}`: {}", short(&e)))?;
            el.move_mouse_over().map_err(|e| format!("could not hover `{sel}`: {}", short(&e)))?;
            Ok(String::new())
        }
        Step::Fill { selector, text } => {
            wait_selector(tab, selector, timeout)?;
            let expr = format!(
                "(function(){{const el=document.querySelector({sel});if(!el)return 'no element';el.focus();\
                 const d=Object.getOwnPropertyDescriptor(Object.getPrototypeOf(el),'value');\
                 if(d&&d.set){{d.set.call(el,{val});}}else{{el.value={val};}}\
                 el.dispatchEvent(new Event('input',{{bubbles:true}}));el.dispatchEvent(new Event('change',{{bubbles:true}}));return 'ok';}})()",
                sel = js(selector),
                val = js(text)
            );
            match eval(tab, &expr, false)?.as_str() {
                Some("ok") => Ok(String::new()),
                other => Err(format!("could not fill `{selector}`: {}", other.unwrap_or("no result"))),
            }
        }
        Step::Select { selector, value } => {
            wait_selector(tab, selector, timeout)?;
            let expr = format!(
                "(function(){{const el=document.querySelector({sel});if(!el||el.tagName!=='SELECT')return 'not a select';\
                 const v={val};let o=[...el.options].find(o=>o.value===v)||[...el.options].find(o=>o.text.trim()===v);\
                 if(!o)return 'no option '+v;el.value=o.value;\
                 el.dispatchEvent(new Event('input',{{bubbles:true}}));el.dispatchEvent(new Event('change',{{bubbles:true}}));return 'ok';}})()",
                sel = js(selector),
                val = js(value)
            );
            match eval(tab, &expr, false)?.as_str() {
                Some("ok") => Ok(String::new()),
                other => Err(format!("could not select in `{selector}`: {}", other.unwrap_or("no result"))),
            }
        }
        Step::Type(text) => tab.type_str(text).map(|_| String::new()).map_err(|e| short(&e)),
        Step::Press(key) => tab.press_key(key).map(|_| String::new()).map_err(|e| format!("could not press {key}: {}", short(&e))),
        Step::Wait(sel) => wait_selector(tab, sel, timeout).map(|_| String::new()),
        Step::WaitGone(sel) => {
            let expr = format!("!document.querySelector({})", js(sel));
            if poll(timeout, || eval(tab, &expr, false).ok().and_then(|v| v.as_bool()).unwrap_or(false)) {
                Ok(String::new())
            } else {
                Err(format!("`{sel}` was still on the page after {} s", timeout.as_secs()))
            }
        }
        Step::WaitText(text) => wait_text(tab, text, timeout).map(|_| String::new()),
        Step::Eval(code) => {
            // A statement list is not an expression; try as written, then wrapped.
            let v = eval(tab, code, true).or_else(|first| {
                if first.contains("SyntaxError") {
                    eval(tab, &format!("(async()=>{{{code}}})()"), true)
                } else {
                    Err(first)
                }
            })?;
            let text = show(&v);
            Ok(if text.len() > 600 { format!("{}... [+{} bytes]", &text[..crate::text::floor_boundary(&text, 600)], text.len() - 600) } else { text })
        }
        Step::Goto(target) => {
            let current = tab.get_url();
            let url = if target.starts_with("http://") || target.starts_with("https://") || target.starts_with("file://") {
                target.clone()
            } else if let Some(origin) = origin_of(&current) {
                format!("{origin}{}", if target.starts_with('/') { target.clone() } else { format!("/{target}") })
            } else {
                return Err(format!("cannot resolve `{target}` against {current}"));
            };
            if let Err(why) = req.reach.check("GET", &url) {
                return Err(format!("goto {url}: {why}"));
            }
            tab.navigate_to(&url).and_then(|t| t.wait_until_navigated()).map_err(|e| format!("navigation failed: {}", short(&e)))?;
            Ok(format!("now at {}", tab.get_url()))
        }
        Step::Scroll(to) => {
            let expr = match to.as_str() {
                "top" => "window.scrollTo(0,0);'ok'".to_string(),
                "bottom" => "window.scrollTo(0,document.documentElement.scrollHeight);'ok'".to_string(),
                sel => format!("(function(){{const el=document.querySelector({});if(!el)return 'no element';el.scrollIntoView({{block:'center'}});return 'ok';}})()", js(sel)),
            };
            match eval(tab, &expr, false)?.as_str() {
                Some("ok") => Ok(String::new()),
                other => Err(format!("could not scroll to `{to}`: {}", other.unwrap_or("no result"))),
            }
        }
        Step::Sleep(ms) => {
            std::thread::sleep(Duration::from_millis(*ms));
            Ok(String::new())
        }
    }
}

fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme == "file" {
        return None;
    }
    let authority = rest.split('/').next()?;
    Some(format!("{scheme}://{authority}"))
}

fn screenshot(tab: &Tab, req: &BrowseRequest) -> Result<Vec<u8>, String> {
    let (w, h) = (req.viewport.0 as f64, req.viewport.1 as f64);
    let clip = if req.full_page {
        let full = eval(tab, "Math.ceil(document.documentElement.scrollHeight)", false).ok().and_then(|v| v.as_f64()).unwrap_or(h);
        Some(Page::Viewport { x: 0.0, y: 0.0, width: w, height: full.clamp(h, 2400.0), scale: 1.0 })
    } else {
        None
    };
    let r = tab
        .call_method(Page::CaptureScreenshot {
            format: Some(Page::CaptureScreenshotFormatOption::Png),
            quality: None,
            clip,
            from_surface: Some(true),
            capture_beyond_viewport: Some(req.full_page),
            optimize_for_speed: None,
        })
        .map_err(|e| short(&e))?;
    unbase64(&r.data).ok_or_else(|| "the screenshot was not valid base64".to_string())
}

/// Standard base64 (padding optional), as CDP sends images.
fn unbase64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for b in s.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\r' | b'\n' => continue,
            _ => return None,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// The page as text: title, headings, controls with selectors a step can use, links, and the
/// visible text. One expression, returned by value.
const SNAPSHOT_JS: &str = r#"(function(){
 const vis=e=>{const r=e.getBoundingClientRect();const s=getComputedStyle(e);return r.width>0&&r.height>0&&s.visibility!=='hidden'&&s.display!=='none';};
 const t=(s,n)=>(s||'').replace(/\s+/g,' ').trim().slice(0,n);
 const sel=e=>{if(e.id)return '#'+CSS.escape(e.id);const n=e.getAttribute('name');if(n)return e.tagName.toLowerCase()+'[name="'+n+'"]';
  const c=[...e.classList].find(x=>x);return e.tagName.toLowerCase()+(c?'.'+CSS.escape(c):'');};
 const label=e=>{if(e.id){const l=document.querySelector('label[for="'+CSS.escape(e.id)+'"]');if(l)return t(l.innerText,50);}
  return t(e.getAttribute('aria-label')||e.placeholder||e.name||'',50);};
 const out=[];
 out.push('title: '+t(document.title,120));
 const hs=[...document.querySelectorAll('h1,h2,h3')].filter(vis).slice(0,15).map(h=>h.tagName.toLowerCase()+': '+t(h.innerText,100));
 if(hs.length)out.push('headings:\n  '+hs.join('\n  '));
 const bs=[...document.querySelectorAll('button,[role=button],input[type=submit],input[type=button]')].filter(vis).slice(0,25)
  .map(b=>'['+t(b.innerText||b.value||b.getAttribute('aria-label'),50)+'] '+sel(b)+(b.disabled?' (disabled)':''));
 if(bs.length)out.push('buttons:\n  '+bs.join('\n  '));
 const ins=[...document.querySelectorAll('input:not([type=submit]):not([type=button]):not([type=hidden]),textarea,select')].filter(vis).slice(0,25)
  .map(i=>{const ty=i.tagName==='SELECT'?'select':(i.type||'text');const v=ty==='password'?'':t(i.value,40);return ty+' '+sel(i)+(label(i)?' "'+label(i)+'"':'')+(v?' = '+v:'');});
 if(ins.length)out.push('inputs:\n  '+ins.join('\n  '));
 const ls=[...document.querySelectorAll('a[href]')].filter(vis).slice(0,25).map(a=>t(a.innerText,50)+' -> '+a.getAttribute('href'));
 if(ls.length)out.push('links:\n  '+ls.join('\n  '));
 const noalt=[...document.images].filter(i=>!i.hasAttribute('alt')).length;
 if(noalt)out.push('images without alt text: '+noalt);
 out.push('text:\n  '+t(document.body?document.body.innerText:'',3500));
 return out.join('\n').slice(0,8000);
})()"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_are_read_from_the_current_page() {
        assert_eq!(origin_of("http://localhost:5173/a/b").as_deref(), Some("http://localhost:5173"));
        assert_eq!(origin_of("file:///x/index.html"), None);
    }

    #[test]
    fn base64_decodes_what_cdp_sends() {
        assert_eq!(unbase64("aGVsbG8gd29ybGQ=").unwrap(), b"hello world");
        assert_eq!(unbase64("aGk").unwrap(), b"hi");
        assert!(unbase64("a$b").is_none());
    }

    #[test]
    fn favicon_noise_is_ignored() {
        assert!(noise("http://127.0.0.1:1/favicon.ico"));
        assert!(noise("http://127.0.0.1:1/favicon.ico?v=2"));
        assert!(!noise("http://127.0.0.1:1/app.js"));
    }
}
