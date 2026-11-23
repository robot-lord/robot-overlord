//! The code-folder form: where the user keeps repos (`[session] code_dir`).
//! The repo finder scans it and new repos go there.
//!
//! Describe, then confirm: an optional description for the overlord, and
//! the folder and what the finder scans as hints with rlord's suggestion
//! filled in. Saving with no description (or after the overlord reopened
//! the form, `confirm_setup`) saves by plain code (`setup.apply`); a
//! description goes to the overlord (`overlord.setup_request`), which
//! reopens the form with its proposal.
//!
//! Opened on first run (after the overlord's own dialog), by `:setup`, and
//! by the overlord.
//!
//! While `[limits] auto_resume` is unset it also asks that, once (the
//! Auto-resume row: on or off must be picked before saving), saved with
//! the folder (`setup.apply`'s `auto_resume`), or at once
//! (`limits.auto_resume`) when the form goes to the overlord.

use super::*;
use ratatui::widgets::Clear;
use rlord_core::config::{contract_tilde, expand_tilde};

/// What the repo finder scans, as the form will save it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Roots {
    /// Only the code folder (`repo_roots` removed).
    Follow,
    /// The user's configured roots, as they are.
    Keep(Vec<String>),
    /// These (from the overlord).
    Set(Vec<String>),
}

/// A suggested folder: its path (with `~`) and what it rests on.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Choice {
    path: String,
    projects: usize,
    of: usize,
    from_roots: bool,
    prompts: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Describe,
    Folder,
    Roots,
    AutoResume,
    Save,
}

pub(crate) struct SetupForm {
    /// The describe field. None: no overlord exists, the row is hidden.
    describe: Option<String>,
    /// The folder, shown with `~`.
    code_dir: String,
    /// The user edited or picked it (or the overlord set it).
    code_dir_chosen: bool,
    /// The suggestion, then alternatives.
    choices: Vec<Choice>,
    /// (source, projects) behind the suggestion.
    sources: Vec<(String, usize)>,
    roots: Roots,
    /// `repo_roots` as configured.
    configured: Vec<String>,
    /// The Roots row is shown: explicit roots other than the code folder,
    /// or roots the overlord set.
    roots_row: bool,
    /// From `confirm_setup`: the hints it changed, and why.
    changed: Vec<String>,
    note: Option<String>,
    /// Reopened by the overlord, description untouched since: saving
    /// saves what is shown.
    confirmed: bool,
    /// Opened by the first run: Esc is remembered (`setup.apply skip`).
    pub(crate) auto: bool,
    /// Open the overlord's overlay when this closes (first run).
    pub(crate) then_overlord: bool,
    row: usize,
    error: Option<String>,
    /// rlord's suggestion is still being worked out (off the UI thread;
    /// `fill` lands it).
    pub(crate) loading: bool,
    /// The user moved or typed since it opened: a late suggestion keeps
    /// their place.
    touched: bool,
    /// The folder the user was told doesn't exist; saving it again
    /// creates it.
    confirm_create: Option<String>,
    /// Reopened with an overlord proposal (`prefill`): a late suggestion
    /// doesn't replace it.
    proposal: bool,
    /// The roots edit is rlord's default (`roots_default`), not a pick.
    roots_from_default: bool,
    /// The overlord's proposed roots, kept so ←/→ can come back to them.
    proposed_roots: Option<Vec<String>>,
    /// The folder `$HOME` was confirmed for ("scans your whole home
    /// folder"): saving it again goes ahead.
    confirm_home: Option<String>,
    /// The code folder as configured now (with `~`; "" when unset): shown
    /// as the current setting, not as the user's pick.
    current: String,
    /// `$HOME` as given and resolved: only folders under it are looked at
    /// from this (UI) thread.
    homes: Vec<std::path::PathBuf>,
    /// Which folders macOS would ask about (built once).
    guard: rlord_core::privacy::Guard,
    /// The line under the folder (`folder_hint`), worked out once per
    /// edit, not per frame (it may look at the disk).
    hint: String,
    /// `[limits] auto_resume` isn't chosen yet: the Auto-resume row asks.
    auto_resume_row: bool,
    /// The user's pick on that row (None: not picked yet).
    pub(crate) auto_resume: Option<bool>,
}

/// A setup request on its way to the overlord: its result, and the form
/// it came from (`App::poll_setup_send`).
pub(crate) type SetupSend = (mpsc::Receiver<Result<(), String>>, Box<SetupForm>);

/// What Save does.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SaveCall {
    /// `setup.apply` with these params.
    Apply(Value),
    /// `overlord.setup_request` with these params.
    Request(Value),
    /// The folder doesn't exist: ask before creating it.
    AskCreate(String),
    /// The folder is `$HOME`: say the finder scans all of it, ask again.
    AskHome(String),
}

impl SetupForm {
    /// The form for a `setup.suggest` answer. `overlord`: one exists (the
    /// describe row is shown).
    pub(crate) fn from_suggest(sugg: &Value, overlord: bool, auto: bool) -> SetupForm {
        let choice = |c: &Value| Choice {
            path: contract_tilde(c["path"].as_str().unwrap_or("")),
            projects: c["projects"].as_u64().unwrap_or(0) as usize,
            of: c["of"].as_u64().unwrap_or(0) as usize,
            from_roots: c["from"] == "repo_roots",
            prompts: c["prompts"] == true,
        };
        let choices: Vec<Choice> = std::iter::once(&sugg["suggestion"])
            .filter(|s| s.is_object())
            .chain(sugg["alternatives"].as_array().into_iter().flatten())
            .map(choice)
            .filter(|c| !c.path.is_empty())
            .collect();
        let sources = rlord_core::code_dir::sources_of(&sugg["sources"]);
        let configured: Vec<String> = sugg["roots"]["configured"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let current = sugg["code_dir"].as_str().unwrap_or("");
        // (`code_dir_chosen` is only the user's own change in this form.)
        let (code_dir, chosen) = if current.is_empty() {
            (
                choices.first().map(|c| c.path.clone()).unwrap_or_default(),
                false,
            )
        } else {
            (contract_tilde(current), false)
        };
        // What saving rlord's suggestion does with the roots is the
        // daemon's call (`roots_default`), used as given.
        let roots_from_default = !configured.is_empty() && sugg["roots_default"] == "follow";
        let roots = if configured.is_empty() || sugg["roots_default"] == "follow" {
            Roots::Follow
        } else {
            Roots::Keep(configured.clone())
        };
        let mut f = SetupForm {
            describe: overlord.then(String::new),
            code_dir,
            code_dir_chosen: chosen,
            choices,
            sources,
            roots,
            configured,
            roots_row: false,
            changed: Vec::new(),
            note: None,
            confirmed: false,
            auto,
            then_overlord: false,
            row: 0,
            error: None,
            loading: false,
            touched: false,
            confirm_create: None,
            confirm_home: None,
            roots_from_default,
            current: if current.is_empty() {
                String::new()
            } else {
                contract_tilde(current)
            },
            homes: rlord_core::privacy::home_paths(),
            proposal: false,
            proposed_roots: None,
            guard: rlord_core::privacy::Guard::new(),
            hint: String::new(),
            auto_resume_row: false,
            auto_resume: None,
        };
        f.roots_row = f.roots_differ();
        // With a folder filled in, Enter saves it; with none, type one.
        let start = if f.code_dir.is_empty() {
            Row::Folder
        } else {
            Row::Save
        };
        f.row = f.rows().iter().position(|r| *r == start).unwrap_or(0);
        f.refresh_hint();
        f
    }

    /// Work out the line under the folder again (after an edit).
    pub(crate) fn refresh_hint(&mut self) {
        self.hint = folder_hint(self);
    }

    /// A form waiting on rlord's suggestion ("finding…").
    /// `status` (`setup.status`, cheap) gives the current code folder and
    /// roots, so saving before the suggestion lands can't drop roots the
    /// form hasn't seen.
    pub(crate) fn loading(overlord: bool, auto: bool, status: &Value) -> SetupForm {
        let known = json!({
            "code_dir": status["code_dir"],
            "roots": {"configured": status["repo_roots"]},
            "roots_default": status["roots_default"],
        });
        let mut f = SetupForm::from_suggest(&known, overlord, auto);
        // (A key present and null: not chosen yet.)
        f.auto_resume_row = status.get("auto_resume").is_some_and(Value::is_null);
        f.loading = true;
        f.refresh_hint();
        f
    }

    /// The suggestion arrived (Null: none could be made). What the user
    /// already typed or picked stays.
    pub(crate) fn fill(&mut self, sugg: &Value) {
        // Only the suggestion's own fields: the choices, what they rest
        // on, and the folder when none is shown yet. What is configured
        // (and the roots edit) came from `setup.status` and stays, so a
        // failed suggestion can't turn Save into dropping `repo_roots`.
        let new = SetupForm::from_suggest(sugg, false, self.auto);
        self.choices = new.choices;
        self.sources = new.sources;
        let at = self.current();
        if self.code_dir.trim().is_empty() && !self.code_dir_chosen {
            self.code_dir = new.code_dir;
        }
        self.loading = false;
        self.roots_row = self.roots_row || self.roots_differ();
        self.row = if self.touched || self.proposal {
            self.rows().iter().position(|r| *r == at).unwrap_or(0)
        } else {
            // As a fresh form starts: on Save with a folder, else on it.
            let start = if self.code_dir.is_empty() {
                Row::Folder
            } else {
                Row::Save
            };
            self.rows().iter().position(|r| *r == start).unwrap_or(0)
        };
        self.refresh_hint();
    }

    /// The configured roots are explicit and aren't just the code folder.
    fn roots_differ(&self) -> bool {
        rlord_core::config::roots_beyond_code_dir(&self.configured, &self.code_dir)
    }

    /// Reopened by the overlord (`confirm_setup`): its proposal, the
    /// user's description, what it changed and why.
    pub(crate) fn prefill(&mut self, p: &Value) {
        if let Some(d) = p["code_dir"].as_str().filter(|d| !d.is_empty()) {
            self.code_dir = contract_tilde(d);
        }
        if let Some(r) = p["repo_roots"].as_array() {
            let r: Vec<String> = r
                .iter()
                .filter_map(Value::as_str)
                .map(contract_tilde)
                .collect();
            // `[]`: only the code folder.
            self.roots = if r.is_empty() {
                Roots::Follow
            } else {
                self.proposed_roots = Some(r.clone());
                Roots::Set(r)
            };
            self.roots_from_default = false;
            self.roots_row = true;
        } else {
            // No roots proposed: the configured ones stay, whatever the
            // first-run default would do (`["~"]` included).
            self.roots = if self.configured.is_empty() {
                Roots::Follow
            } else {
                Roots::Keep(self.configured.clone())
            };
            self.roots_row = self.roots_differ();
        }
        self.proposal = true;
        if let Some(d) = p["description"].as_str() {
            self.describe = Some(d.to_string());
        }
        self.changed = p["changed"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        self.note = p["note"]
            .as_str()
            .filter(|n| !n.is_empty())
            .map(str::to_string);
        self.refresh_hint();
        self.confirmed = true;
        self.error = None;
        self.row = self
            .rows()
            .iter()
            .position(|r| *r == Row::Save)
            .unwrap_or(0);
    }

    fn rows(&self) -> Vec<Row> {
        let mut v = Vec::new();
        if self.describe.is_some() {
            v.push(Row::Describe);
        }
        v.push(Row::Folder);
        if self.roots_row {
            v.push(Row::Roots);
        }
        if self.auto_resume_row {
            v.push(Row::AutoResume);
        }
        v.push(Row::Save);
        v
    }

    fn current(&self) -> Row {
        self.rows()[self.row.min(self.rows().len() - 1)]
    }

    /// The description changed after the overlord reopened the form: it
    /// is a new request again.
    fn description_changed(&mut self) {
        self.confirmed = false;
        self.changed.clear();
        self.note = None;
    }

    fn describing(&self) -> bool {
        self.describe
            .as_deref()
            .is_some_and(|d| !d.trim().is_empty())
    }

    /// ← / → on the folder row: the next or previous suggestion.
    fn cycle(&mut self, forward: bool) {
        let n = self.choices.len();
        if n == 0 {
            return;
        }
        let at = self.choices.iter().position(|c| c.path == self.code_dir);
        let to = match (at, forward) {
            (None, true) => 0,
            (None, false) => n - 1,
            (Some(i), true) => (i + 1) % n,
            (Some(i), false) => (i + n - 1) % n,
        };
        self.code_dir = self.choices[to].path.clone();
        self.code_dir_chosen = true;
        self.refresh_hint();
        if !self.roots_row {
            self.roots_row = self.roots_differ();
        }
    }

    /// ← / → on the roots row: the code folder or the configured roots.
    /// The roots row's choices, in order: the overlord's proposal (if
    /// any), only the code folder, the configured roots (if any).
    fn roots_options(&self) -> Vec<Roots> {
        let mut v = Vec::new();
        if let Some(p) = &self.proposed_roots {
            v.push(Roots::Set(p.clone()));
        }
        v.push(Roots::Follow);
        if !self.configured.is_empty() {
            v.push(Roots::Keep(self.configured.clone()));
        }
        v
    }

    fn toggle_roots(&mut self) {
        let o = self.roots_options();
        let next = o
            .iter()
            .position(|r| *r == self.roots)
            .map_or(0, |i| (i + 1) % o.len());
        self.roots = o[next].clone();
        self.roots_from_default = false;
    }

    /// The suggestion the folder field shows, if it is one.
    fn shown_choice(&self) -> Option<&Choice> {
        self.choices.iter().find(|c| c.path == self.code_dir)
    }

    /// The Auto-resume row is shown and nothing is picked on it yet.
    pub(crate) fn auto_resume_missing(&self) -> bool {
        self.auto_resume_row && self.auto_resume.is_none()
    }

    /// ←/→/space on the Auto-resume row: on, then off, then on.
    fn toggle_auto_resume(&mut self) {
        self.auto_resume = Some(self.auto_resume != Some(true));
    }

    /// What Save does, or why it can't. `exists`: whether a folder is
    /// there (None: can't tell without risking a macOS prompt).
    pub(crate) fn save_call(
        &self,
        exists: &dyn Fn(&str) -> Option<bool>,
    ) -> std::result::Result<SaveCall, String> {
        let dir = self.code_dir.trim();
        if self.describing() && !self.confirmed {
            let source = if dir.is_empty() {
                "not set"
            } else if self.code_dir_chosen {
                "picked by the user"
            } else if dir == self.current {
                "current setting"
            } else if self.proposal {
                "proposed by the overlord"
            } else {
                "suggested by rlord"
            };
            let (roots, roots_source) = match &self.roots {
                Roots::Follow if self.roots_from_default => (json!("follow"), "default"),
                Roots::Follow => (json!("follow"), "user"),
                Roots::Keep(r) => (json!(r), "config"),
                Roots::Set(r) => (json!(r), "user"),
            };
            return Ok(SaveCall::Request(json!({
                "description": self.describe.as_deref().unwrap_or("").trim(),
                "code_dir": if dir.is_empty() { String::new() } else { expand_tilde(dir) },
                "code_dir_source": source,
                "roots": roots,
                "roots_source": roots_source,
            })));
        }
        if self.auto_resume_missing() && !dir.is_empty() {
            return Err("pick auto-resume on or off (←/→ on its row)".into());
        }
        if dir.is_empty() {
            return Err(if self.loading {
                "still finding a suggestion: type a folder, or wait a moment".into()
            } else {
                "type the folder you keep your repos in".into()
            });
        }
        let path = clean_dir(dir);
        // `$HOME` itself: the finder would scan all of it; asked first.
        let home = self.is_home(&path);
        if home && self.confirm_home.as_deref() != Some(path.as_str()) {
            return Ok(SaveCall::AskHome(path));
        }
        // A new folder is created only once the user said so (as the CLI
        // asks). One not looked at here (`exists` None) is checked by the
        // daemon; if it isn't there, the user is asked then.
        let confirmed = self.confirm_create.as_deref() == Some(path.as_str());
        let missing = exists(&path) == Some(false);
        if missing && !confirmed {
            return Ok(SaveCall::AskCreate(path));
        }
        let create = missing || confirmed;
        let roots = match &self.roots {
            Roots::Follow => json!("follow"),
            Roots::Keep(_) => json!("leave"),
            Roots::Set(r) => json!(r),
        };
        let mut p = json!({
            "code_dir": path,
            "roots": roots,
            "create": create,
            "allow_home": home,
        });
        if let Some(on) = self.auto_resume.filter(|_| self.auto_resume_row) {
            p["auto_resume"] = json!(on);
        }
        Ok(SaveCall::Apply(p))
    }

    /// `path` is `$HOME` (as given or resolved).
    fn is_home(&self, path: &str) -> bool {
        rlord_core::privacy::is_home_in(
            std::path::Path::new(path),
            &self.homes,
            self.guard.folds_case(),
        )
    }

    /// Whether this (UI) thread may look at `path` on disk: only under
    /// `$HOME`, and only where no macOS prompt guards. Anything else is
    /// left to the daemon's check on save.
    fn may_stat(&self, path: &std::path::Path) -> bool {
        self.homes.iter().any(|h| self.guard.is_within(path, h))
            && self.guard.prompts(path).is_none()
    }

    /// The user has typed or moved in it.
    pub(crate) fn is_touched(&self) -> bool {
        self.touched
    }

    /// A click on row `i`: select it. True when it already was.
    pub(crate) fn click_row(&mut self, i: usize) -> bool {
        if self.row == i {
            return true;
        }
        self.row = i.min(self.rows().len() - 1);
        false
    }

    /// The Save row is focused (a click on it saves).
    pub(crate) fn on_save(&self) -> bool {
        self.current() == Row::Save
    }
}

impl App {
    /// First run: open the code-folder form when setup is needed. True
    /// when it opened.
    pub(crate) fn check_setup(&mut self) -> bool {
        // Put off with Esc this run: not again until the next start (a
        // parked overlord proposal still opens).
        if self.setup_put_off && self.pending_setup.is_none() {
            return false;
        }
        // (A cheap look-up; the suggestion is worked out only if needed,
        // and off this thread.)
        match self.call("setup.status", Value::Null) {
            Some(st) => self.check_setup_from(st),
            None => false,
        }
    }

    /// `check_setup` with `setup.status` in hand.
    fn check_setup_from(&mut self, st: Value) -> bool {
        let needed = st["needed"] == true;
        // The overlord proposed one while something else was open: that
        // one, not rlord's own suggestion (even if the form was put off).
        if let Some(cmd) = self.pending_setup.take() {
            self.open_parked(cmd, Some(st));
            if let Mode::Setup(f) = &mut self.mode {
                f.auto = needed;
            }
            return true;
        }
        needed && self.open_setup_with(&st, true)
    }

    /// The startup sequence (the overlord's dialogs, a pending command)
    /// is over once nothing is open: then the code folder, once. Never
    /// mid-session. Called on each pass of the event loop.
    pub(crate) fn continue_startup(&mut self) {
        if !self.startup_chain || !matches!(self.mode, Mode::Normal) {
            return;
        }
        self.startup_chain = false;
        let opened = self.check_setup();
        // Setup is behind the user (answered, or a code folder set): the
        // import offer, if anything is found. Otherwise after the form.
        if !opened && !self.setup_put_off {
            self.start_import_offer();
        }
        if std::mem::take(&mut self.overlay_after_startup) {
            match &mut self.mode {
                Mode::Setup(f) if opened => f.then_overlord = true,
                _ => self.show_overlord_after_offer(),
            }
        }
    }

    /// Open a proposal the overlord parked while something else was open,
    /// with what is configured now (`status`, or asked for here), not what
    /// was configured when it was queued. The proposal's own fields stay.
    pub(crate) fn open_parked(&mut self, mut cmd: Value, status: Option<Value>) {
        if let Some(st) = status.or_else(|| self.call("setup.status", Value::Null)) {
            cmd["status"] = st;
        }
        self.open_setup_from(&cmd);
    }

    /// Open the code-folder form now, showing "finding…" while rlord's
    /// suggestion is worked out on another thread (it reads the agents'
    /// files). True when it opened.
    pub(crate) fn open_setup(&mut self, auto: bool) -> bool {
        match self.call("setup.status", Value::Null) {
            Some(st) => self.open_setup_with(&st, auto),
            None => false,
        }
    }

    /// `open_setup`, with `setup.status` in hand.
    fn open_setup_with(&mut self, status: &Value, auto: bool) -> bool {
        let Some(o) = self.call("overlord.get", Value::Null) else {
            return false;
        };
        self.drop_stale_suggestion(status);
        self.mode = Mode::Setup(Box::new(SetupForm::loading(!o.is_null(), auto, status)));
        self.load_suggestion();
        true
    }

    /// The kept suggestion was made for another config (the code folder or
    /// `repo_roots` changed since): drop it, so the next open asks again.
    fn drop_stale_suggestion(&mut self, status: &Value) {
        let stale = self.last_suggestion.as_ref().is_some_and(|v| {
            v["code_dir"] != status["code_dir"] || v["roots"]["configured"] != status["repo_roots"]
        });
        if stale {
            self.last_suggestion = None;
        }
    }

    /// Work out rlord's suggestion on another thread (it reads the agents'
    /// files); `poll_setup` fills the open form with it.
    /// Only the first open in a run asks the daemon: the answer is kept for
    /// the run and reused (reopening, an overlord round trip).
    fn load_suggestion(&mut self) {
        if let Some(v) = self.last_suggestion.clone() {
            if let Mode::Setup(f) = &mut self.mode {
                f.fill(&v);
            }
            return;
        }
        // (Tests never reach a real daemon, nor the agents' files it reads.)
        if cfg!(test) {
            return;
        }
        let launch = std::env::current_dir()
            .ok()
            .and_then(|d| rlord_core::git::main_repo_root(&d.to_string_lossy()));
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let v = client_call(
                &paths::socket_path(),
                "setup.suggest",
                json!({"launch_dir": launch}),
            )
            .unwrap_or(Value::Null);
            let _ = tx.send(v);
        });
        self.setup_rx = Some(rx);
    }

    /// The suggestion for an open form, if it has arrived (the event loop
    /// asks on each pass). True when the screen changed.
    pub(crate) fn poll_setup(&mut self) -> bool {
        let Some(rx) = &self.setup_rx else {
            return false;
        };
        let v = match rx.try_recv() {
            Ok(v) => v,
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => Value::Null,
        };
        self.setup_rx = None;
        if !v.is_null() {
            self.last_suggestion = Some(v.clone());
        }
        match &mut self.mode {
            Mode::Setup(f) if f.loading => {
                f.fill(&v);
                true
            }
            _ => false,
        }
    }

    /// The setup request's delivery: said once it's done; on failure the
    /// form comes back with the user's text, once nothing else is open.
    /// True when the screen changed.
    pub(crate) fn poll_setup_send(&mut self) -> bool {
        if let Some((rx, _)) = &self.setup_send {
            let r = match rx.try_recv() {
                Ok(r) => r,
                Err(mpsc::TryRecvError::Empty) => return false,
                Err(mpsc::TryRecvError::Disconnected) => Err("the request was lost".into()),
            };
            let (_, mut f) = self.setup_send.take().expect("checked");
            match r {
                Ok(()) => self.status_line = "sent to the overlord".into(),
                Err(e) => {
                    self.setup_reply_flags = None;
                    self.status_line = format!("couldn't reach the overlord: {e}");
                    f.error = Some(format!("not sent: {e}"));
                    self.setup_retry = Some(f);
                }
            }
        }
        if self.setup_retry.is_some() && matches!(self.mode, Mode::Normal) {
            self.mode = Mode::Setup(self.setup_retry.take().expect("checked"));
            return true;
        }
        false
    }

    /// The overlord's `confirm_setup`: the form, reopened with its proposal.
    /// (It replaces any proposal parked for `:setup`.)
    pub(crate) fn open_setup_from(&mut self, cmd: &Value) {
        self.pending_setup = None;
        // Replacing the first-run form: it stays the first-run form (Esc,
        // then the overlay, as before).
        let (auto, then_overlord) = match &self.mode {
            Mode::Setup(f) => (f.auto, f.then_overlord),
            // (The form that asked the overlord was sent away: its own.)
            _ => self.setup_reply_flags.take().unwrap_or((false, false)),
        };
        // What is configured now came with the command (cheap); rlord's own
        // suggestion, for ←/→, loads in the background.
        self.drop_stale_suggestion(&cmd["status"]);
        let mut f = SetupForm::loading(true, auto, &cmd["status"]);
        f.then_overlord = then_overlord;
        f.prefill(&cmd["prefill"]);
        self.mode = Mode::Setup(Box::new(f));
        self.load_suggestion();
    }

    /// The form closed (saved, sent or put off).
    fn close_setup(&mut self, f: &SetupForm) {
        // The import offer comes next (the daemon says whether it
        // applies). Put off with Esc: both wait for the next start.
        if !self.setup_put_off {
            self.start_import_offer();
        }
        if f.then_overlord {
            self.show_overlord_after_offer();
        }
    }

    pub(crate) fn handle_setup_form(&mut self, k: KeyEvent, mut f: Box<SetupForm>) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let row = f.current();
        let n = f.rows().len();
        if ctrl && k.code == KeyCode::Char('s') {
            return self.save_setup_form(f);
        }
        // Don't ask again (first run): recorded, so the form stops
        // opening; with no code folder the finder then lists only repos
        // already used (unless `repo_roots` is set).
        if ctrl && k.code == KeyCode::Char('d') && f.auto {
            let _ = self.call("setup.apply", json!({"skip": true}));
            self.status_line =
                "won't ask again · the finder lists only repos you've used · :setup any time"
                    .into();
            self.close_setup(&f);
            return;
        }
        if !matches!(k.code, KeyCode::Esc | KeyCode::Enter) {
            f.touched = true;
        }
        match k.code {
            // Later: nothing is recorded, so it asks again next start.
            KeyCode::Esc => {
                if f.auto {
                    self.setup_put_off = true;
                    self.status_line =
                        "no code folder yet · asks again next start · :setup any time".into();
                }
                self.close_setup(&f);
                return;
            }
            KeyCode::Tab | KeyCode::Down => f.row = (f.row + 1) % n,
            KeyCode::BackTab | KeyCode::Up => f.row = (f.row + n - 1) % n,
            KeyCode::Enter if row == Row::Describe => {
                if let Some(d) = &mut f.describe {
                    d.push('\n');
                }
                f.description_changed();
            }
            KeyCode::Enter => return self.save_setup_form(f),
            KeyCode::Left | KeyCode::Right if row == Row::Folder => {
                f.cycle(k.code == KeyCode::Right);
                f.error = None;
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if row == Row::Roots => {
                f.toggle_roots();
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if row == Row::AutoResume => {
                f.toggle_auto_resume();
                f.error = None;
            }
            KeyCode::Char('y') if row == Row::AutoResume => {
                f.auto_resume = Some(true);
                f.error = None;
            }
            KeyCode::Char('n') if row == Row::AutoResume => {
                f.auto_resume = Some(false);
                f.error = None;
            }
            KeyCode::Backspace => match row {
                Row::Describe => {
                    if let Some(d) = &mut f.describe {
                        d.pop();
                    }
                    f.description_changed();
                }
                Row::Folder => {
                    f.code_dir.pop();
                    f.code_dir_chosen = true;
                    f.error = None;
                    f.refresh_hint();
                }
                _ => {}
            },
            KeyCode::Char(c) if !ctrl => match row {
                Row::Describe => {
                    if let Some(d) = &mut f.describe {
                        d.push(c);
                    }
                    f.description_changed();
                }
                Row::Folder => {
                    f.code_dir.push(c);
                    f.code_dir_chosen = true;
                    f.error = None;
                    if !f.roots_row {
                        f.roots_row = f.roots_differ();
                    }
                    f.refresh_hint();
                }
                _ => {}
            },
            _ => {}
        }
        self.mode = Mode::Setup(f);
    }

    fn save_setup_form(&mut self, mut f: Box<SetupForm>) {
        let exists = |p: &str| {
            let p = std::path::Path::new(p);
            f.may_stat(p).then(|| p.is_dir())
        };
        let call = f.save_call(&exists);
        match call {
            Ok(SaveCall::AskHome(path)) => {
                f.error = Some(format!(
                    "{} is your whole home folder: the repo finder scans all of it. \
                     Enter again to use it anyway",
                    contract_tilde(&path)
                ));
                f.confirm_home = Some(path);
                f.row = f.rows().iter().position(|r| *r == Row::Save).unwrap_or(0);
                self.mode = Mode::Setup(f);
            }
            Ok(SaveCall::AskCreate(path)) => {
                f.error = Some(format!(
                    "{} doesn't exist: enter again to create it",
                    contract_tilde(&path)
                ));
                f.confirm_create = Some(path);
                f.row = f.rows().iter().position(|r| *r == Row::Save).unwrap_or(0);
                self.mode = Mode::Setup(f);
            }
            Err(e) => {
                f.error = Some(e);
                let at = if f.auto_resume_missing() && !f.code_dir.trim().is_empty() {
                    Row::AutoResume
                } else {
                    Row::Folder
                };
                f.row = f.rows().iter().position(|r| *r == at).unwrap_or(0);
                self.mode = Mode::Setup(f);
            }
            Ok(SaveCall::Apply(p)) => match client_call(&paths::socket_path(), "setup.apply", p) {
                Ok(r) => {
                    if let Ok(c) = Config::load() {
                        self.config = c;
                    }
                    let roots: Vec<String> = r["repo_roots"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .map(contract_tilde)
                                .collect()
                        })
                        .unwrap_or_default();
                    let dir = contract_tilde(r["code_dir"].as_str().unwrap_or(""));
                    // Saved: a proposal parked for `:setup`, and the kept
                    // suggestion, are stale now.
                    self.pending_setup = None;
                    self.last_suggestion = None;
                    self.status_line = if roots == [dir.clone()] {
                        format!("code folder: {dir} · the repo finder scans it")
                    } else {
                        format!(
                            "code folder: {dir} · the repo finder scans {}",
                            roots.join(", ")
                        )
                    };
                    if r["checked"] == false {
                        self.status_line.push_str(
                            " · not checked: macOS guards this folder; create it yourself if it doesn't exist",
                        );
                    }
                    self.close_setup(&f);
                }
                Err(e) => {
                    // Not looked at here, and the daemon found it missing:
                    // ask, as for one seen missing here.
                    let path = clean_dir(&f.code_dir);
                    if format!("{e:#}").contains("doesn't exist") {
                        f.error = Some(format!(
                            "{} doesn't exist: enter again to create it",
                            contract_tilde(&path)
                        ));
                        f.confirm_create = Some(path);
                    } else {
                        f.error = Some(format!("{e:#}"));
                    }
                    self.mode = Mode::Setup(f);
                }
            },
            Ok(SaveCall::Request(p)) => {
                // The overlord's reopened form doesn't carry the
                // auto-resume pick: saved now.
                if let Some(on) = f.auto_resume.filter(|_| f.auto_resume_row) {
                    if self.call("limits.auto_resume", json!({"on": on})).is_some() {
                        self.config.limits.auto_resume = Some(on);
                        f.auto_resume_row = false;
                    }
                }
                let exists = self
                    .call("overlord.status", Value::Null)
                    .is_some_and(|st| st["exists"] == true);
                if !exists {
                    f.error =
                        Some("no overlord yet: clear the description to save as shown".into());
                    self.mode = Mode::Setup(f);
                    return;
                }
                // In the background: a stopped overlord can take a while.
                // The form is kept until it's delivered (`poll_setup_send`).
                let (tx, rx) = mpsc::channel();
                std::thread::spawn(move || {
                    let r = client_call(&paths::socket_path(), "overlord.setup_request", p)
                        .map(|_| ())
                        .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(r);
                });
                // The overlord's answer reopens it as this form was (first
                // run, overlay after).
                self.setup_reply_flags = Some((f.auto, f.then_overlord));
                self.setup_send = Some((rx, f));
                // Its answer reopens the form; nothing parked is current.
                self.pending_setup = None;
                self.status_line = "sending to the overlord…".into();
                self.show_overlord();
            }
        }
    }

    pub(crate) fn draw_setup_form(&self, f: &mut Frame, s: &SetupForm) {
        let accent = Style::default().fg(theme::accent());
        let muted = theme::muted();
        let width = f.area().width.min(84).saturating_sub(8).max(10) as usize;
        let mut lines: Vec<Line> = vec![Line::from("")];
        if let Some(n) = &s.note {
            lines.push(Line::from(vec![
                Span::styled("  ✦ ", accent.bold()),
                Span::styled(n.clone(), accent),
            ]));
            lines.push(Line::from(Span::styled(
                "    Check it, then save: rlord saves it as shown.",
                muted,
            )));
        } else {
            lines.push(Line::from(Span::styled(
                "  The folder you keep your repos in. The repo finder scans it,",
                Style::default().fg(theme::text()),
            )));
            lines.push(Line::from(Span::styled(
                "  and new repos go there.",
                Style::default().fg(theme::text()),
            )));
        }
        lines.push(Line::from(""));
        let rows = s.rows();
        let mut starts = Vec::new();
        let label = |focused: bool, name: &str| {
            vec![
                Span::styled(if focused { "▌ " } else { "  " }, accent),
                Span::styled(
                    format!("{name:<19}"),
                    if focused {
                        Style::default().fg(theme::yellow()).bold()
                    } else {
                        Style::default().bold()
                    },
                ),
            ]
        };
        let marker = |key: &str| s.changed.iter().any(|c| c == key);
        for (i, row) in rows.iter().enumerate() {
            let focused = s.row == i;
            starts.push(lines.len());
            match row {
                Row::Describe => {
                    let d = s.describe.as_deref().unwrap_or("");
                    let mut spans = label(focused, "Tell the overlord");
                    let mut body: Vec<String> = d
                        .lines()
                        .flat_map(|l| {
                            let chars: Vec<char> = l.chars().collect();
                            if chars.is_empty() {
                                return vec![String::new()];
                            }
                            chars
                                .chunks(width.saturating_sub(21).max(10))
                                .map(|c| c.iter().collect())
                                .collect::<Vec<String>>()
                        })
                        .collect();
                    if d.ends_with('\n') {
                        body.push(String::new());
                    }
                    if body.is_empty() {
                        spans.push(Span::styled(
                            if focused { "▏" } else { "" }.to_string(),
                            accent,
                        ));
                        spans.push(Span::styled(
                            "(optional) e.g. \"work in ~/ko, personal in ~/proj\"",
                            theme::faint(),
                        ));
                        lines.push(Line::from(spans));
                    } else {
                        let last = body.len() - 1;
                        for (j, b) in body.into_iter().enumerate() {
                            let mut sp = if j == 0 {
                                spans.clone()
                            } else {
                                vec![Span::raw(" ".repeat(21))]
                            };
                            let cursor = if focused && j == last { "▏" } else { "" };
                            sp.push(Span::raw(format!("{b}{cursor}")));
                            lines.push(Line::from(sp));
                        }
                    }
                }
                Row::Folder => {
                    let mut spans = label(focused, "Code folder");
                    let style = if marker("code_dir") {
                        accent
                    } else {
                        Style::default().fg(theme::text())
                    };
                    let shown = if focused && s.choices.len() > 1 {
                        format!("◀ {}▏ ▶", s.code_dir)
                    } else if focused {
                        format!("{}▏", s.code_dir)
                    } else {
                        s.code_dir.clone()
                    };
                    spans.push(Span::styled(shown, style));
                    if marker("code_dir") {
                        spans.push(Span::styled("  ✦ updated", accent));
                    }
                    lines.push(Line::from(spans));
                    let hint = s.hint.clone();
                    // (A long hint starts further left, to fit.)
                    let indent = if hint.chars().count() + 21 > width {
                        4
                    } else {
                        21
                    };
                    lines.push(Line::from(Span::styled(
                        format!("{:indent$}{hint}", ""),
                        muted,
                    )));
                }
                Row::Roots => {
                    let mut spans = label(focused, "Repo finder scans");
                    let (value, was) = match &s.roots {
                        Roots::Follow => (
                            "the code folder".to_string(),
                            (!s.configured.is_empty())
                                .then(|| format!("  (was {})", s.configured.join(", "))),
                        ),
                        Roots::Keep(r) => (
                            format!("the code folder and {} (as configured)", r.join(", ")),
                            None,
                        ),
                        Roots::Set(r) => (format!("the code folder and {}", r.join(", ")), None),
                    };
                    let style = if marker("repo_roots") {
                        accent
                    } else {
                        Style::default().fg(theme::text())
                    };
                    spans.push(Span::styled(
                        if focused {
                            format!("◀ {value} ▶")
                        } else {
                            value
                        },
                        style,
                    ));
                    if let Some(w) = was {
                        spans.push(Span::styled(w, theme::faint()));
                    }
                    if marker("repo_roots") {
                        spans.push(Span::styled("  ✦ updated", accent));
                    }
                    lines.push(Line::from(spans));
                }
                Row::AutoResume => {
                    let mut spans = label(focused, "Auto-resume");
                    let value = match s.auto_resume {
                        None => "pick on or off",
                        Some(true) => "on: limited sessions carry on when the limit resets",
                        Some(false) => "off: resume one from its menu",
                    };
                    let style = if s.auto_resume.is_none() {
                        Style::default().fg(theme::yellow())
                    } else {
                        Style::default().fg(theme::text())
                    };
                    spans.push(Span::styled(
                        if focused {
                            format!("◀ {value} ▶")
                        } else {
                            value.to_string()
                        },
                        style,
                    ));
                    lines.push(Line::from(spans));
                    lines.push(Line::from(Span::styled(
                        format!(
                            "{:21}after a usage limit (not the overlord's) · [limits] auto_resume",
                            ""
                        ),
                        muted,
                    )));
                }
                Row::Save => {
                    lines.push(Line::from(""));
                    starts.pop();
                    starts.push(lines.len());
                    let text = if s.describing() && !s.confirmed {
                        "[ Send to the overlord ]"
                    } else {
                        "[ Save ]"
                    };
                    lines.push(Line::from(vec![
                        Span::styled(if focused { "▌ " } else { "  " }, accent),
                        Span::styled(
                            text,
                            if focused {
                                accent.bold()
                            } else {
                                Style::default().bold()
                            },
                        ),
                    ]));
                }
            }
            lines.push(Line::from(""));
        }
        lines.push(keys_line(&[
            ("enter", "save", Cap::Context),
            ("tab", "move", Cap::Global),
            ("←/→", "other suggestions", Cap::Global),
            if s.auto {
                ("esc", "ask me next start", Cap::Global)
            } else {
                ("esc", "close", Cap::Global)
            },
        ]));
        if s.auto {
            lines.push(keys_line(&[("ctrl-d", "don't ask again", Cap::Global)]));
            lines.push(Line::from(Span::styled(
                "    then the finder lists only repos you've used, unless repo_roots is set",
                theme::faint(),
            )));
        }
        if let Some(e) = &s.error {
            lines.push(Line::from(Span::styled(
                format!("  {e}"),
                Style::default().fg(theme::red()),
            )));
        }
        // As tall as what it shows.
        let area = newform::centered(f.area(), 84, lines.len() as u16 + 3);
        f.render_widget(Clear, area);
        self.modal_hits(f.area(), area, &lines);
        for (i, at) in starts.iter().enumerate() {
            self.hits
                .row(area, area.y + 1 + *at as u16, mouse::Hit::Pick(i));
        }
        f.render_widget(
            Paragraph::new(lines).block(theme::modal("◆ Where do you keep your code?")),
            area,
        );
    }
}

/// A typed folder as rlord keeps it (`absolute_dir`: `~` expanded, `.`
/// and `..` resolved lexically), before any guard check or stat: so
/// `~/src/../Documents/x` is seen as the guarded folder it is. A relative
/// path stays as typed (and is refused).
fn clean_dir(dir: &str) -> String {
    rlord_core::config::absolute_dir(dir).unwrap_or_else(|_| expand_tilde(dir.trim()))
}

/// The line under the folder: what the suggestion rests on, or what
/// saving will do.
fn folder_hint(s: &SetupForm) -> String {
    let dir = s.code_dir.trim();
    if dir.is_empty() && s.loading {
        return "finding where your projects are…".into();
    }
    if dir.is_empty() {
        return "rlord found no projects yet: type your repos' folder (created if missing)".into();
    }
    if let Some(c) = s.shown_choice() {
        if c.prompts {
            return "macOS will ask permission for this folder once".into();
        }
        let mut h = rlord_core::code_dir::basis(c.projects, c.of, &s.sources);
        if c.from_roots {
            h.push_str(" · in your repo_roots");
        }
        return h;
    }
    let path = clean_dir(dir);
    let p = std::path::Path::new(&path);
    if !p.is_absolute() {
        return "a full path, like ~/code".into();
    }
    if s.is_home(&path) {
        return "your whole home folder: the repo finder scans all of it".into();
    }
    if s.guard.prompts(p).is_some() {
        return "macOS will ask permission for this folder once".into();
    }
    if dir == s.current {
        return "your current code folder".into();
    }
    // (Only under `$HOME` is the disk looked at from here.)
    if !s.may_stat(p) {
        return "not checked here: rlord checks it when you save".into();
    }
    if !p.is_dir() {
        return "will be created".into();
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sugg(best: Option<&str>, configured: &[&str]) -> Value {
        json!({
            "code_dir": "",
            "suggestion": best.map(|b| json!({"path": b, "projects": 3, "of": 4, "from": "vote", "prompts": false})),
            "alternatives": [{"path": "/elsewhere/work", "projects": 1, "of": 4, "from": "vote", "prompts": false}],
            "sources": {"claude": 3, "rlord": 1},
            "roots": {"configured": configured, "effective": []},
            // (As the daemon says before setup was done.)
            "roots_default": if configured == ["~"] { "follow" } else { "leave" },
        })
    }

    #[test]
    fn focus_starts_on_save_with_a_suggestion() {
        let f = SetupForm::from_suggest(&sugg(Some("/elsewhere/ko"), &[]), true, true);
        assert_eq!(f.current(), Row::Save);
        assert_eq!(f.rows(), [Row::Describe, Row::Folder, Row::Save]);
        assert_eq!(f.code_dir, "/elsewhere/ko");
        // No suggestion, no overlord: the folder field, no describe row.
        let mut s = sugg(None, &[]);
        s["alternatives"] = json!([]);
        let f = SetupForm::from_suggest(&s, false, true);
        assert_eq!(f.current(), Row::Folder);
        assert_eq!(f.rows(), [Row::Folder, Row::Save]);
        assert!(f.hint.starts_with("rlord found no projects yet"));
        assert!(f.save_call(&|_| Some(true)).is_err());
    }

    /// `[limits] auto_resume` unset: the form asks, and won't save until
    /// on or off is picked; set, the row isn't there.
    #[test]
    fn auto_resume_must_be_picked_while_unset() {
        let status = json!({"code_dir": "/elsewhere/ko", "repo_roots": [],
                            "roots_default": "leave", "auto_resume": null});
        let mut f = SetupForm::loading(false, true, &status);
        assert_eq!(f.rows(), [Row::Folder, Row::AutoResume, Row::Save]);
        let err = f.save_call(&|_| Some(true)).unwrap_err();
        assert!(err.contains("auto-resume"), "{err}");
        f.toggle_auto_resume();
        assert_eq!(f.auto_resume, Some(true));
        let SaveCall::Apply(p) = f.save_call(&|_| Some(true)).unwrap() else {
            panic!("expected a save");
        };
        assert_eq!(p["auto_resume"], true);
        f.toggle_auto_resume();
        let SaveCall::Apply(p) = f.save_call(&|_| Some(true)).unwrap() else {
            panic!("expected a save");
        };
        assert_eq!(p["auto_resume"], false);
        for set in [json!(true), json!(false)] {
            let mut st = status.clone();
            st["auto_resume"] = set;
            let f = SetupForm::loading(false, true, &st);
            assert_eq!(f.rows(), [Row::Folder, Row::Save]);
            let SaveCall::Apply(p) = f.save_call(&|_| Some(true)).unwrap() else {
                panic!("expected a save");
            };
            assert!(p.get("auto_resume").is_none());
        }
    }

    #[test]
    fn arrows_cycle_choices_and_roots_row_shows_when_it_should() {
        let mut f = SetupForm::from_suggest(&sugg(Some("/elsewhere/ko"), &["~"]), true, true);
        // `["~"]`: the row shows, preselecting the code folder.
        assert!(f.roots_row && f.roots == Roots::Follow);
        f.row = 1;
        f.cycle(true);
        assert_eq!(
            (f.code_dir.as_str(), f.code_dir_chosen),
            ("/elsewhere/work", true)
        );
        f.cycle(true);
        assert_eq!(f.code_dir, "/elsewhere/ko");
        // Other explicit roots: kept by default.
        let f = SetupForm::from_suggest(&sugg(Some("/elsewhere/ko"), &["/x"]), true, true);
        assert_eq!(f.roots, Roots::Keep(vec!["/x".into()]));
        // Roots that are the code folder: no row.
        let f =
            SetupForm::from_suggest(&sugg(Some("/elsewhere/ko"), &["/elsewhere/ko"]), true, true);
        assert!(!f.roots_row);
    }

    #[test]
    fn prefill_from_the_overlord_and_editing_clears_confirmed() {
        let mut f = SetupForm::from_suggest(&sugg(Some("/elsewhere/ko"), &[]), true, false);
        f.prefill(
            &json!({"code_dir": "/elsewhere/ko", "repo_roots": ["/elsewhere/ko", "/p"],
                          "description": "both", "changed": ["repo_roots"], "note": "Both"}),
        );
        assert!(f.confirmed && f.roots_row);
        assert_eq!(f.note.as_deref(), Some("Both"));
        assert_eq!(f.changed, ["repo_roots"]);
        assert_eq!(f.current(), Row::Save);
        // Confirmed: Save saves what is shown.
        assert_eq!(
            f.save_call(&|_| Some(true)).unwrap(),
            SaveCall::Apply(json!({"code_dir": "/elsewhere/ko",
                                   "roots": ["/elsewhere/ko", "/p"], "create": false, "allow_home": false}))
        );
        f.description_changed();
        assert!(!f.confirmed && f.changed.is_empty());
        assert!(matches!(
            f.save_call(&|_| Some(true)).unwrap(),
            SaveCall::Request(_)
        ));
    }

    #[test]
    fn save_applies_or_asks_the_overlord() {
        let mut f = SetupForm::from_suggest(&sugg(Some("/elsewhere/ko"), &["~"]), true, true);
        assert_eq!(
            f.save_call(&|_| Some(true)).unwrap(),
            SaveCall::Apply(
                json!({"code_dir": "/elsewhere/ko", "roots": "follow", "create": false, "allow_home": false})
            )
        );
        // A folder that isn't there: asked first, created on the second save.
        assert_eq!(
            f.save_call(&|_| Some(false)).unwrap(),
            SaveCall::AskCreate("/elsewhere/ko".into())
        );
        f.confirm_create = Some("/elsewhere/ko".into());
        assert_eq!(
            f.save_call(&|_| Some(false)).unwrap(),
            SaveCall::Apply(
                json!({"code_dir": "/elsewhere/ko", "roots": "follow", "create": true, "allow_home": false})
            )
        );
        // Another folder typed since: asked again.
        f.code_dir = "/elsewhere/kode".into();
        assert!(matches!(
            f.save_call(&|_| Some(false)).unwrap(),
            SaveCall::AskCreate(_)
        ));
        // Can't tell (macOS would ask): saved as given, not created.
        assert_eq!(
            f.save_call(&|_| None).unwrap(),
            SaveCall::Apply(
                json!({"code_dir": "/elsewhere/kode", "roots": "follow", "create": false, "allow_home": false})
            )
        );
        f.code_dir = "/elsewhere/ko".into();
        f.describe = Some("mine are in ~/proj".into());
        let SaveCall::Request(p) = f.save_call(&|_| Some(true)).unwrap() else {
            panic!("not a request")
        };
        assert_eq!(p["code_dir_source"], "suggested by rlord");
        assert_eq!(p["roots"], "follow");
        assert_eq!(p["description"], "mine are in ~/proj");
    }

    /// The form opens at once; the suggestion lands later, keeping what
    /// the user typed meanwhile.
    #[test]
    fn a_late_suggestion_fills_the_form() {
        let mut f = SetupForm::loading(true, true, &Value::Null);
        assert!(f.loading && f.code_dir.is_empty());
        assert_eq!(f.hint, "finding where your projects are…");
        assert!(f
            .save_call(&|_| Some(true))
            .unwrap_err()
            .contains("still finding"));
        f.fill(&sugg(Some("/elsewhere/ko"), &[]));
        assert!(!f.loading);
        assert_eq!(
            (f.code_dir.as_str(), f.current()),
            ("/elsewhere/ko", Row::Save)
        );
        // Typed before it arrived: kept, and the focus stays.
        let mut f = SetupForm::loading(true, true, &Value::Null);
        f.row = 1;
        f.touched = true;
        f.code_dir = "/mine".into();
        f.code_dir_chosen = true;
        f.describe = Some("d".into());
        f.fill(&sugg(Some("/elsewhere/ko"), &[]));
        assert_eq!(f.code_dir, "/mine");
        assert_eq!(f.current(), Row::Folder);
        assert_eq!(f.describe.as_deref(), Some("d"));
        assert_eq!(f.choices.len(), 2);
        // No answer: an empty form, no longer loading.
        let mut f = SetupForm::loading(false, true, &Value::Null);
        f.fill(&Value::Null);
        assert!(!f.loading);
        assert!(f.hint.starts_with("rlord found no projects yet"));
    }

    /// Saved before the suggestion arrives: the configured roots (from
    /// the cheap status) are kept, never replaced by "follow".
    #[test]
    fn an_early_save_keeps_configured_roots() {
        let st =
            json!({"needed": true, "code_dir": "", "repo_roots": ["/x"], "roots_default": "leave"});
        let mut f = SetupForm::loading(true, true, &st);
        assert_eq!(f.roots, Roots::Keep(vec!["/x".into()]));
        f.code_dir = "/c".into();
        let SaveCall::Apply(p) = f.save_call(&|_| Some(true)).unwrap() else {
            panic!("not saved")
        };
        assert_eq!(p["roots"], "leave");
        // The current code folder is shown at once too.
        let st = json!({"code_dir": "/c", "repo_roots": [], "done": true});
        assert_eq!(SetupForm::loading(false, false, &st).code_dir, "/c");
    }

    /// The daemon's `roots_default` is used as given.
    #[test]
    fn the_roots_default_comes_from_the_daemon() {
        let mut s = sugg(Some("/elsewhere/ko"), &["~"]);
        s["roots_default"] = json!("follow");
        assert_eq!(
            SetupForm::from_suggest(&s, false, false).roots,
            Roots::Follow
        );
        s["roots_default"] = json!("leave");
        assert_eq!(
            SetupForm::from_suggest(&s, false, false).roots,
            Roots::Keep(vec!["~".into()])
        );
    }

    /// The suggestion failed (Null): the configured roots stay, and Save
    /// leaves them.
    #[test]
    fn a_failed_suggestion_keeps_the_roots() {
        let st = json!({"code_dir": "", "repo_roots": ["/x"], "roots_default": "leave"});
        let mut f = SetupForm::loading(false, true, &st);
        f.fill(&Value::Null);
        assert!(!f.loading);
        assert_eq!(f.roots, Roots::Keep(vec!["/x".into()]));
        f.code_dir = "/c".into();
        let SaveCall::Apply(p) = f.save_call(&|_| Some(true)).unwrap() else {
            panic!("not saved")
        };
        assert_eq!(p["roots"], "leave");
    }

    /// Put off with Esc: not opened again this run by `check_setup` (a
    /// dialog closing); `:setup` and the overlord still open it.
    #[test]
    fn a_form_put_off_stays_off_this_run() {
        let (_tx, rx) = mpsc::channel();
        let keys = KeyMap::from_config(&rlord_core::config::Keys::default()).unwrap();
        let mut app = App::new(rx, Config::default(), keys);
        app.setup_put_off = true;
        // (No daemon is asked: it returns before any call.)
        assert!(!app.check_setup());
        assert!(matches!(app.mode, Mode::Normal));
        app.open_setup_from(&json!({"cmd": "setup", "prefill": {"code_dir": "/p"},
                                    "status": {"code_dir": "", "repo_roots": []}}));
        assert!(matches!(app.mode, Mode::Setup(_)));
    }

    /// An overlord proposal that names no roots leaves `["~"]` alone, even
    /// on first run where rlord's own suggestion would drop it; a late
    /// suggestion doesn't change that.
    #[test]
    fn a_proposal_without_roots_leaves_home_roots() {
        let st = json!({"code_dir": "", "repo_roots": ["~"], "roots_default": "follow"});
        let mut f = SetupForm::loading(true, true, &st);
        assert_eq!(f.roots, Roots::Follow);
        f.prefill(&json!({"code_dir": "/elsewhere/ko", "note": "n"}));
        assert_eq!(f.roots, Roots::Keep(vec!["~".into()]));
        let mut s = sugg(Some("/elsewhere/work"), &["~"]);
        s["roots_default"] = json!("follow");
        f.fill(&s);
        assert_eq!(f.roots, Roots::Keep(vec!["~".into()]));
        assert_eq!(f.code_dir, "/elsewhere/ko");
        assert!(f.confirmed && f.note.as_deref() == Some("n"));
        assert_eq!(f.current(), Row::Save);
        let SaveCall::Apply(p) = f.save_call(&|_| Some(true)).unwrap() else {
            panic!("not saved")
        };
        assert_eq!(p["roots"], "leave");
    }

    /// The overlord's `repo_roots: []`: only the code folder.
    #[test]
    fn an_empty_roots_proposal_scans_only_the_code_folder() {
        let mut f = SetupForm::from_suggest(&sugg(Some("/elsewhere/ko"), &["/x"]), true, false);
        f.prefill(
            &json!({"code_dir": "/elsewhere/ko", "repo_roots": [], "changed": ["repo_roots"]}),
        );
        assert!(f.roots_row && f.roots == Roots::Follow);
        let SaveCall::Apply(p) = f.save_call(&|_| Some(true)).unwrap() else {
            panic!("not saved")
        };
        assert_eq!(p["roots"], "follow");
    }

    /// The hint is worked out on edits, not on every frame.
    #[test]
    fn the_hint_follows_edits() {
        let mut f = SetupForm::from_suggest(&sugg(Some("/elsewhere/ko"), &[]), false, true);
        assert!(f.hint.starts_with("3 of 4 projects"), "{}", f.hint);
        f.cycle(true);
        assert!(f.hint.starts_with("1 of 4 projects"), "{}", f.hint);
        f.code_dir = "rel".into();
        f.refresh_hint();
        assert_eq!(f.hint, "a full path, like ~/code");
    }

    /// The overlord replacing the first-run form keeps it the first-run
    /// form.
    #[test]
    fn a_proposal_over_the_first_run_form_keeps_its_place() {
        let (_tx, rx) = mpsc::channel();
        let keys = KeyMap::from_config(&rlord_core::config::Keys::default()).unwrap();
        let mut app = App::new(rx, Config::default(), keys);
        let mut first = SetupForm::from_suggest(&sugg(None, &[]), true, true);
        first.then_overlord = true;
        app.mode = Mode::Setup(Box::new(first));
        app.open_setup_from(&json!({"cmd": "setup", "prefill": {"code_dir": "/new"},
                                    "status": {"code_dir": "", "repo_roots": []}}));
        let Mode::Setup(f) = &app.mode else {
            panic!("form not open")
        };
        assert!(f.auto && f.then_overlord);
    }

    /// The suggestion is asked for once a run: a reopened form (and an
    /// overlord round trip) reuses it.
    #[test]
    fn the_suggestion_is_reused_within_a_run() {
        let (_tx, rx) = mpsc::channel();
        let keys = KeyMap::from_config(&rlord_core::config::Keys::default()).unwrap();
        let mut app = App::new(rx, Config::default(), keys);
        app.last_suggestion = Some(sugg(Some("/elsewhere/ko"), &[]));
        app.open_setup_from(&json!({"cmd": "setup", "prefill": {"note": "n"},
                                    "status": {"code_dir": "", "repo_roots": []}}));
        let Mode::Setup(f) = &app.mode else {
            panic!("form not open")
        };
        assert!(!f.loading && app.setup_rx.is_none());
        assert_eq!((f.code_dir.as_str(), f.choices.len()), ("/elsewhere/ko", 2));
    }

    /// A setup request that couldn't be sent brings the form back with the
    /// user's text, and says why.
    #[test]
    fn a_failed_request_reopens_the_form() {
        let (_tx, rx) = mpsc::channel();
        let keys = KeyMap::from_config(&rlord_core::config::Keys::default()).unwrap();
        let mut app = App::new(rx, Config::default(), keys);
        let mut f = SetupForm::from_suggest(&sugg(None, &[]), true, false);
        f.describe = Some("mine are in ~/ko".into());
        let (tx, srx) = mpsc::channel();
        app.setup_send = Some((srx, Box::new(f)));
        assert!(!app.poll_setup_send());
        tx.send(Err("boom".to_string())).unwrap();
        assert!(app.poll_setup_send());
        assert!(app.status_line.contains("boom"));
        let Mode::Setup(f) = &app.mode else {
            panic!("form not back")
        };
        assert_eq!(f.describe.as_deref(), Some("mine are in ~/ko"));
        // Delivered: said so, nothing reopened.
        app.mode = Mode::Normal;
        let (tx, srx) = mpsc::channel();
        let f = SetupForm::from_suggest(&sugg(None, &[]), true, false);
        app.setup_send = Some((srx, Box::new(f)));
        tx.send(Ok(())).unwrap();
        assert!(!app.poll_setup_send());
        assert_eq!(app.status_line, "sent to the overlord");
    }

    /// ←/→ on the roots row cycles the overlord's proposal, only the code
    /// folder, and the configured roots; a default Follow is sent as such.
    #[test]
    fn roots_cycle_keeps_the_proposal() {
        let mut f = SetupForm::from_suggest(&sugg(Some("/elsewhere/ko"), &["~"]), true, false);
        assert!(f.roots_from_default);
        f.describe = Some("x".into());
        let SaveCall::Request(p) = f.save_call(&|_| Some(true)).unwrap() else {
            panic!("not a request")
        };
        assert_eq!(p["roots_source"], "default");
        f.prefill(&json!({"code_dir": "/elsewhere/ko", "repo_roots": ["/p"]}));
        assert_eq!(f.roots, Roots::Set(vec!["/p".into()]));
        f.toggle_roots();
        assert_eq!(f.roots, Roots::Follow);
        f.toggle_roots();
        assert_eq!(f.roots, Roots::Keep(vec!["~".into()]));
        f.toggle_roots();
        assert_eq!(f.roots, Roots::Set(vec!["/p".into()]));
    }

    /// A setup command at startup is the first-run form, and ends the
    /// startup sequence (no second form after it).
    #[test]
    fn a_setup_command_at_startup_is_the_first_run_form() {
        let (_tx, rx) = mpsc::channel();
        let keys = KeyMap::from_config(&rlord_core::config::Keys::default()).unwrap();
        let mut app = App::new(rx, Config::default(), keys);
        app.startup_chain = true;
        app.handle_ui_cmd(json!({"cmd": "setup", "prefill": {"code_dir": "/p"},
                                 "status": {"needed": true, "code_dir": "", "repo_roots": []}}));
        let Mode::Setup(f) = &app.mode else {
            panic!("form not open")
        };
        assert!(f.auto);
        assert!(!app.startup_chain);
    }

    /// The overlord's answer to a first-run form's request opens as the
    /// first-run form.
    #[test]
    fn the_answer_to_a_first_run_request_is_first_run() {
        let (_tx, rx) = mpsc::channel();
        let keys = KeyMap::from_config(&rlord_core::config::Keys::default()).unwrap();
        let mut app = App::new(rx, Config::default(), keys);
        app.setup_reply_flags = Some((true, true));
        app.open_setup_from(&json!({"cmd": "setup", "prefill": {"code_dir": "/p"},
                                    "status": {"code_dir": "", "repo_roots": []}}));
        let Mode::Setup(f) = &app.mode else {
            panic!("form not open")
        };
        assert!(f.auto && f.then_overlord);
        assert!(app.setup_reply_flags.is_none());
    }

    /// A path with `..` is resolved before the guard and the home rule
    /// see it.
    #[test]
    fn typed_paths_are_resolved_before_any_check() {
        let mut f = SetupForm::from_suggest(&sugg(None, &[]), false, false);
        f.homes = vec![std::path::PathBuf::from("/h")];
        f.guard = rlord_core::privacy::Guard::with(Some(std::path::Path::new("/h")), false);
        f.code_dir = "/h/src/../Documents/code".into();
        f.refresh_hint();
        assert!(f.hint.starts_with("macOS will ask"), "{}", f.hint);
        // Saved as resolved, never stat-ed (guarded).
        let SaveCall::Apply(p) = f
            .save_call(&|p: &str| (!p.contains("..")).then_some(true))
            .unwrap()
        else {
            panic!("not saved")
        };
        assert_eq!(p["code_dir"], "/h/Documents/code");
        f.code_dir = "/h/x/..".into();
        assert_eq!(
            f.save_call(&|_| Some(true)).unwrap(),
            SaveCall::AskHome("/h".into())
        );
    }

    /// A kept suggestion made for another config is dropped on open.
    #[test]
    fn a_stale_suggestion_is_dropped() {
        let (_tx, rx) = mpsc::channel();
        let keys = KeyMap::from_config(&rlord_core::config::Keys::default()).unwrap();
        let mut app = App::new(rx, Config::default(), keys);
        app.last_suggestion = Some(sugg(Some("/elsewhere/ko"), &[]));
        app.drop_stale_suggestion(&json!({"code_dir": "", "repo_roots": []}));
        assert!(app.last_suggestion.is_some());
        app.drop_stale_suggestion(&json!({"code_dir": "/now", "repo_roots": []}));
        assert!(app.last_suggestion.is_none());
    }

    /// Outside `$HOME` the form doesn't look at the disk: "not checked
    /// here", and Save leaves the check to the daemon. `$HOME` itself is
    /// asked about twice.
    #[test]
    fn only_home_folders_are_looked_at_here() {
        let mut f = SetupForm::from_suggest(&sugg(None, &[]), false, false);
        f.homes = vec![std::path::PathBuf::from("/h")];
        f.code_dir = "/srv/code".into();
        f.refresh_hint();
        assert!(f.hint.starts_with("not checked here"), "{}", f.hint);
        assert!(!f.may_stat(std::path::Path::new("/srv/code")));
        // Save: unknown (None), so no create unless the daemon says so.
        let SaveCall::Apply(p) = f.save_call(&|_| None).unwrap() else {
            panic!("not saved")
        };
        assert_eq!(
            (p["create"].as_bool(), p["allow_home"].as_bool()),
            (Some(false), Some(false))
        );
        // Home: a warning, then confirmed.
        f.code_dir = "/h".into();
        f.refresh_hint();
        assert!(f.hint.contains("whole home folder"), "{}", f.hint);
        assert_eq!(
            f.save_call(&|_| Some(true)).unwrap(),
            SaveCall::AskHome("/h".into())
        );
        f.confirm_home = Some("/h".into());
        let SaveCall::Apply(p) = f.save_call(&|_| Some(true)).unwrap() else {
            panic!("not saved")
        };
        assert_eq!(p["allow_home"], true);
    }

    /// A configured folder is the current setting, not the user's pick.
    #[test]
    fn the_configured_folder_is_the_current_setting() {
        let mut s = sugg(None, &[]);
        s["code_dir"] = json!("/c");
        let mut f = SetupForm::from_suggest(&s, true, false);
        assert!(!f.code_dir_chosen);
        f.describe = Some("x".into());
        let SaveCall::Request(p) = f.save_call(&|_| Some(true)).unwrap() else {
            panic!("not a request")
        };
        assert_eq!(p["code_dir_source"], "current setting");
    }

    /// A form the user is working in isn't replaced by a proposal: it is
    /// parked for `:setup`.
    #[test]
    fn a_proposal_waits_while_the_user_works_in_the_form() {
        let (_tx, rx) = mpsc::channel();
        let keys = KeyMap::from_config(&rlord_core::config::Keys::default()).unwrap();
        let mut app = App::new(rx, Config::default(), keys);
        let mut f = SetupForm::from_suggest(&sugg(Some("/elsewhere/ko"), &[]), false, false);
        f.touched = true;
        f.code_dir = "/mine".into();
        app.mode = Mode::Setup(Box::new(f));
        app.handle_ui_cmd(json!({"cmd": "setup", "prefill": {"code_dir": "/p"},
                                 "status": {"code_dir": "", "repo_roots": []}}));
        let Mode::Setup(f) = &app.mode else {
            panic!("form gone")
        };
        assert_eq!(f.code_dir, "/mine");
        assert!(app.pending_setup.is_some());
        assert!(app.status_line.contains(":setup"));
    }

    /// Startup: a pending command's screen first; the code folder once
    /// nothing is open; nothing mid-session.
    #[test]
    fn setup_follows_the_startup_sequence_only() {
        let (_tx, rx) = mpsc::channel();
        let keys = KeyMap::from_config(&rlord_core::config::Keys::default()).unwrap();
        let mut app = App::new(rx, Config::default(), keys);
        app.startup_chain = true;
        // A pending command's screen is open: wait.
        app.mode = Mode::Help;
        app.continue_startup();
        assert!(app.startup_chain);
        // Its form, when it was a parked proposal, opens as the first-run
        // form when setup is needed.
        app.mode = Mode::Normal;
        app.pending_setup = Some(json!({"cmd": "setup", "prefill": {"code_dir": "/p"}}));
        let st = json!({"needed": true, "code_dir": "", "repo_roots": []});
        assert!(app.check_setup_from(st));
        let Mode::Setup(f) = &app.mode else {
            panic!("form not open")
        };
        assert!(f.auto);
        // Closed, the sequence ends (put off: no daemon call is made).
        app.mode = Mode::Normal;
        app.setup_put_off = true;
        app.continue_startup();
        assert!(!app.startup_chain);
        // Mid-session: nothing.
        app.setup_put_off = false;
        app.continue_startup();
        assert!(matches!(app.mode, Mode::Normal));
    }

    /// A parked proposal opens with what is configured now, keeping its
    /// own fields.
    #[test]
    fn a_parked_proposal_opens_with_fresh_status() {
        let (_tx, rx) = mpsc::channel();
        let keys = KeyMap::from_config(&rlord_core::config::Keys::default()).unwrap();
        let mut app = App::new(rx, Config::default(), keys);
        let cmd = json!({"cmd": "setup", "prefill": {"code_dir": "/p", "note": "n"},
                         "status": {"code_dir": "", "repo_roots": []}});
        let fresh = json!({"code_dir": "/now", "repo_roots": ["/x"], "roots_default": "leave"});
        app.open_parked(cmd, Some(fresh));
        let Mode::Setup(f) = &app.mode else {
            panic!("form not open")
        };
        assert_eq!(f.configured, ["/x"]);
        assert_eq!(f.roots, Roots::Keep(vec!["/x".into()]));
        assert_eq!((f.code_dir.as_str(), f.note.as_deref()), ("/p", Some("n")));
    }

    /// A proposal parked for `:setup` is dropped once a newer one opens.
    #[test]
    fn a_newer_proposal_clears_the_parked_one() {
        let (_tx, rx) = mpsc::channel();
        let keys = KeyMap::from_config(&rlord_core::config::Keys::default()).unwrap();
        let mut app = App::new(rx, Config::default(), keys);
        app.pending_setup = Some(json!({"cmd": "setup", "prefill": {"code_dir": "/old"}}));
        app.open_setup_from(&json!({"cmd": "setup", "prefill": {"code_dir": "/new"},
                                    "status": {"code_dir": "", "repo_roots": []}}));
        assert!(app.pending_setup.is_none());
        let Mode::Setup(f) = &app.mode else {
            panic!("form not open")
        };
        assert_eq!(f.code_dir, "/new");
    }

    #[test]
    fn draws_the_form() {
        use ratatui::backend::TestBackend;
        let (_tx, rx) = mpsc::channel();
        let keys = KeyMap::from_config(&rlord_core::config::Keys::default()).unwrap();
        let app = App::new(rx, Config::default(), keys);
        let mut s = sugg(None, &[]);
        s["alternatives"] = json!([]);
        let f = SetupForm::from_suggest(&s, true, true);
        let mut term = ratatui::Terminal::new(TestBackend::new(100, 30)).unwrap();
        term.draw(|fr| app.draw_setup_form(fr, &f)).unwrap();
        let buf = term.backend().buffer();
        let out: String = (0..30)
            .map(|y| {
                (0..100)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    + "\n"
            })
            .collect();
        for want in [
            "Where do you keep your code?",
            "Tell the overlord",
            "Code folder",
            "rlord found no projects yet",
        ] {
            assert!(out.contains(want), "{want:?} missing:\n{out}");
        }
    }
}
