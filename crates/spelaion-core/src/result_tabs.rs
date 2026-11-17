//! The result tabs of each editor tab (SDB-378): which stored results an editor tab keeps,
//! in the order it shows them, which are pinned and what each is called.
//!
//! Results live in the core (ADR-010), so what a run replaces is decided here and not in the
//! UI: a run that replaces frees the results that are not pinned, a run into a new tab frees
//! none, and a re-run of one result puts its new rows in that result's place. A pinned result
//! is an entry of the [`ResultStore`] that no run frees; closing it frees it. Nothing is
//! copied anywhere.
//!
//! Nothing here runs a statement: a re-run is a new run the user asked for, checked by the
//! guard like any other (ADR-028), and this only says where its results go.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use spelaion_driver_api::{Dialect, StatementSpan};

use crate::results::{ResultId, ResultStore};
use crate::session::CoreEvent;

/// The result tabs an editor tab keeps unless the user chooses another number.
pub const DEFAULT_RESULT_TABS: usize = spelaion_store::DEFAULT_RESULT_TABS as usize;
/// The most result tabs an editor tab may be set to keep.
pub const MAX_RESULT_TABS: usize = spelaion_store::MAX_RESULT_TABS as usize;
/// The longest name of a result tab, in characters.
pub const MAX_RESULT_NAME: usize = 60;

/// Where the results of a run go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// The run's results take the place of every result that is not pinned.
    Replace,
    /// The run's results are added next to the ones already there.
    NewTab,
    /// The run's first result takes the place of this one, with its name and its pin; the
    /// old rows stay until the new result starts, so a run that fails leaves them.
    Into(ResultId),
}

/// One result tab, as the UI draws it. The rows stay in the [`ResultStore`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultTab {
    pub result: ResultId,
    /// The name the user gave it, else the one its statement's first comment line gave it.
    pub name: Option<String>,
    /// `name` is the one the user gave it (SDB-407): renaming it to nothing gives back the
    /// comment's name, or none.
    pub renamed: bool,
    pub pinned: bool,
}

/// Where a result that just started went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placed {
    pub tab: ResultTab,
    /// The result whose place it took (a re-run), now freed.
    pub replaced: Option<ResultId>,
    /// Results closed to stay within the limit, now freed.
    pub closed: Vec<ResultId>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResultTabsError {
    #[error("there is no such result tab")]
    NoSuchResult,
    #[error(
        "a result this run would replace has changes that are not saved: save or discard them first, or pin the result"
    )]
    PendingEdits,
}

#[derive(Debug)]
struct Kept {
    result: ResultId,
    /// From the statement's first comment line.
    title: Option<String>,
    /// Given by the user; wins over `title`.
    renamed: Option<String>,
    pinned: bool,
    /// The run that produced it.
    run: u64,
    /// Order of creation, across re-runs in place: the limit closes the lowest first.
    seq: u64,
}

impl Kept {
    fn tab(&self) -> ResultTab {
        ResultTab {
            result: self.result,
            name: self.renamed.clone().or_else(|| self.title.clone()),
            renamed: self.renamed.is_some(),
            pinned: self.pinned,
        }
    }
}

#[derive(Debug, Default)]
struct EditorResults {
    /// In the order the tabs are shown.
    kept: Vec<Kept>,
    /// The latest run started.
    run: u64,
    /// The result the latest run's first result replaces.
    target: Option<ResultId>,
    /// Where the latest run's next result goes, when it does not go last (a re-run in place
    /// that returns several results keeps them together).
    insert_at: Option<usize>,
}

#[derive(Debug, Default)]
struct State {
    editors: HashMap<String, EditorResults>,
    next_seq: u64,
}

/// The result tabs of every editor tab, over one [`ResultStore`].
#[derive(Debug)]
pub struct ResultTabs {
    store: Arc<ResultStore>,
    state: Mutex<State>,
}

impl ResultTabs {
    pub fn new(store: Arc<ResultStore>) -> Self {
        Self {
            store,
            state: Mutex::new(State::default()),
        }
    }

    /// Starts a run of `script` in the editor tab `editor`: frees what `placement` replaces
    /// and returns what follows the run's events to place its results. `original` is the
    /// statement as the user wrote it when `script` is a page of it written by the core.
    /// `limit` is how many result tabs the editor tab keeps.
    ///
    /// Refused, with nothing freed, when a result it would free has unsaved edits, or when
    /// the result to run into is not one of this editor tab's.
    pub fn begin_run(
        self: &Arc<Self>,
        editor: &str,
        placement: Placement,
        dialect: Arc<dyn Dialect>,
        script: &str,
        original: Option<&str>,
        limit: usize,
    ) -> Result<RunResults, ResultTabsError> {
        let mut state = self.lock();
        let results = state.editors.entry(editor.to_owned()).or_default();
        let mut closed = Vec::new();
        match placement {
            Placement::Replace => {
                if results
                    .kept
                    .iter()
                    .any(|k| !k.pinned && self.has_pending_edits(k.result))
                {
                    return Err(ResultTabsError::PendingEdits);
                }
                results.kept.retain(|k| {
                    if !k.pinned {
                        closed.push(k.result);
                    }
                    k.pinned
                });
                results.target = None;
            }
            Placement::NewTab => results.target = None,
            Placement::Into(target) => {
                if !results.kept.iter().any(|k| k.result == target) {
                    return Err(ResultTabsError::NoSuchResult);
                }
                if self.has_pending_edits(target) {
                    return Err(ResultTabsError::PendingEdits);
                }
                results.target = Some(target);
            }
        }
        results.insert_at = None;
        results.run += 1;
        let run = results.run;
        drop(state);
        for result in &closed {
            self.store.remove(*result);
        }
        Ok(RunResults {
            tabs: self.clone(),
            editor: editor.to_owned(),
            run,
            dialect,
            script: script.to_owned(),
            original: original.map(str::to_owned),
            limit: limit.clamp(1, MAX_RESULT_TABS),
            closed,
            spans: HashMap::new(),
            previous_end: 0,
        })
    }

    /// The editor tab's result tabs, in order.
    pub fn list(&self, editor: &str) -> Vec<ResultTab> {
        self.lock()
            .editors
            .get(editor)
            .map(|results| results.kept.iter().map(Kept::tab).collect())
            .unwrap_or_default()
    }

    /// Pins a result tab, so no run replaces it and the limit never closes it, or unpins it.
    pub fn pin(
        &self,
        editor: &str,
        result: ResultId,
        pinned: bool,
    ) -> Result<ResultTab, ResultTabsError> {
        self.change(editor, result, |kept| kept.pinned = pinned)
    }

    /// Names a result tab. An empty name gives it back the name its statement's comment gave
    /// it, if any.
    pub fn rename(
        &self,
        editor: &str,
        result: ResultId,
        name: &str,
    ) -> Result<ResultTab, ResultTabsError> {
        let name = clean_name(name);
        self.change(editor, result, |kept| kept.renamed = name)
    }

    /// Closes a result tab, whichever editor tab has it, and frees its rows. Also frees a
    /// stored result no editor tab has. `false` when there was nothing to free.
    pub fn close(&self, result: ResultId) -> bool {
        {
            let mut state = self.lock();
            for results in state.editors.values_mut() {
                results.kept.retain(|k| k.result != result);
                if results.target == Some(result) {
                    results.target = None;
                }
            }
        }
        self.store.remove(result)
    }

    /// Closes every result tab of an editor tab (the editor tab was closed) and frees them.
    /// A result that a run still going there starts later is freed as it starts.
    pub fn close_editor(&self, editor: &str) -> usize {
        let removed = self.lock().editors.remove(editor);
        let kept = removed.map(|results| results.kept).unwrap_or_default();
        for k in &kept {
            self.store.remove(k.result);
        }
        kept.len()
    }

    fn change(
        &self,
        editor: &str,
        result: ResultId,
        change: impl FnOnce(&mut Kept),
    ) -> Result<ResultTab, ResultTabsError> {
        let mut state = self.lock();
        let kept = state
            .editors
            .get_mut(editor)
            .and_then(|results| results.kept.iter_mut().find(|k| k.result == result))
            .ok_or(ResultTabsError::NoSuchResult)?;
        change(kept);
        Ok(kept.tab())
    }

    /// Places a result that run `run` of `editor` just started.
    fn place(
        &self,
        editor: &str,
        run: u64,
        result: ResultId,
        title: Option<String>,
        limit: usize,
    ) -> Option<Placed> {
        let mut state = self.lock();
        let seq = state.next_seq;
        state.next_seq += 1;
        let Some(results) = state.editors.get_mut(editor).filter(|r| r.run == run) else {
            // The editor tab was closed, or a newer run started there: nobody shows this
            // result, so it is not kept.
            drop(state);
            self.store.remove(result);
            return None;
        };
        let mut new = Kept {
            result,
            title,
            renamed: None,
            pinned: false,
            run,
            seq,
        };
        let mut replaced = None;
        let target = results
            .target
            .take()
            .and_then(|target| results.kept.iter().position(|k| k.result == target));
        if let Some(at) = target {
            let old = &results.kept[at];
            // It keeps the tab's pin and name; a comment in the text run again names it anew.
            new.pinned = old.pinned;
            new.renamed = old.renamed.clone();
            new.title = new.title.or_else(|| old.title.clone());
            replaced = Some(old.result);
            results.kept[at] = new;
            results.insert_at = Some(at + 1);
        } else if let Some(at) = results.insert_at.filter(|at| *at <= results.kept.len()) {
            results.kept.insert(at, new);
            results.insert_at = Some(at + 1);
        } else {
            results.kept.push(new);
        }
        // The limit: the oldest result that is not pinned goes first. Never one of the run
        // in progress (a script shows every result it returns), and never unsaved edits.
        let mut closed = Vec::new();
        while results.kept.len() > limit {
            let oldest = results
                .kept
                .iter()
                .enumerate()
                .filter(|(_, k)| !k.pinned && k.run != run && !self.has_pending_edits(k.result))
                .min_by_key(|(_, k)| k.seq)
                .map(|(at, _)| at);
            let Some(at) = oldest else { break };
            closed.push(results.kept.remove(at).result);
            if let Some(insert_at) = results.insert_at.as_mut()
                && at < *insert_at
            {
                *insert_at -= 1;
            }
        }
        let tab = results
            .kept
            .iter()
            .find(|k| k.result == result)
            .map(Kept::tab)?;
        drop(state);
        for old in replaced.iter().chain(&closed) {
            self.store.remove(*old);
        }
        Some(Placed {
            tab,
            replaced,
            closed,
        })
    }

    fn has_pending_edits(&self, result: ResultId) -> bool {
        self.store.get(result).is_some_and(|stored| {
            stored
                .read()
                .map(|stored| !stored.pending().is_empty())
                .unwrap_or(false)
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("result tabs lock poisoned")
    }
}

/// Follows one run's events and places each result it starts
/// ([`ResultTabs::begin_run`]).
pub struct RunResults {
    tabs: Arc<ResultTabs>,
    editor: String,
    run: u64,
    dialect: Arc<dyn Dialect>,
    script: String,
    original: Option<String>,
    limit: usize,
    closed: Vec<ResultId>,
    spans: HashMap<usize, StatementSpan>,
    /// Where the statement before the latest one ended.
    previous_end: usize,
}

impl RunResults {
    /// The results the run replaced as it started, already freed.
    pub fn closed(&self) -> &[ResultId] {
        &self.closed
    }

    /// To be called with every event of the run, in order. For a result that starts, says
    /// where it went; `None` for any other event, and for a result nobody shows any more
    /// (its editor tab was closed), which is freed.
    pub fn observe(&mut self, event: &CoreEvent) -> Option<Placed> {
        match event {
            CoreEvent::StatementStarted { index, span } => {
                if let Some(previous) = index.checked_sub(1).and_then(|i| self.spans.get(&i)) {
                    self.previous_end = previous.end;
                }
                self.spans.insert(*index, *span);
                None
            }
            CoreEvent::ResultStarted { index, result, .. } => {
                let title = self.title(*index);
                self.tabs
                    .place(&self.editor, self.run, *result, title, self.limit)
            }
            CoreEvent::Rows { .. }
            | CoreEvent::Notice { .. }
            | CoreEvent::StatementFinished { .. }
            | CoreEvent::StatementFailed { .. }
            | CoreEvent::ErrorPaused { .. }
            | CoreEvent::ScriptFinished { .. }
            | CoreEvent::HistoryWarning { .. } => None,
        }
    }

    /// The name statement `index` gives its results.
    fn title(&self, index: usize) -> Option<String> {
        if let Some(original) = &self.original {
            return statement_title(self.dialect.as_ref(), original);
        }
        let span = self.spans.get(&index)?;
        // Where the statement's text starts: some splitters give a statement the comments
        // before it and others leave them out, so the comment is looked for from the end of
        // the statement before.
        let from = self.previous_end.min(span.start);
        let before = self.script.get(from..span.end)?;
        let text = before.trim_start_matches(|c: char| c.is_whitespace() || c == ';');
        let at = from + (before.len() - text.len());
        // A comment that ends the line of the statement before is about that statement.
        let line = &self.script[..at];
        let line = &line[line.rfind('\n').map_or(0, |i| i + 1)..];
        if !line.trim().is_empty() {
            return None;
        }
        statement_title(self.dialect.as_ref(), text)
    }
}

/// The name a statement gives its result tab: the text of the `--` comment line it starts
/// with, when the engine's dialect reads that line as a comment (MySQL needs a space after
/// the dashes; a language without such comments names nothing).
pub fn statement_title(dialect: &dyn Dialect, statement: &str) -> Option<String> {
    let text = statement.trim_start();
    if !text.starts_with("--") {
        return None;
    }
    // Code at the very start means the dialect does not read the dashes as a comment.
    if dialect
        .code_ranges(text)
        .first()
        .is_some_and(|code| code.start == 0)
    {
        return None;
    }
    let line = text.lines().next().unwrap_or_default();
    clean_name(line.trim_matches(|c: char| c == '-' || c.is_whitespace()))
}

/// A name as it is kept: one line, no control or invisible characters, at most
/// [`MAX_RESULT_NAME`] characters. `None` when nothing is left.
fn clean_name(name: &str) -> Option<String> {
    let mut clean = String::new();
    let mut space = false;
    for c in name.chars() {
        if c.is_whitespace() {
            space = !clean.is_empty();
            continue;
        }
        if c.is_control() || is_invisible(c) {
            continue;
        }
        if space {
            clean.push(' ');
            space = false;
        }
        clean.push(c);
    }
    if clean.is_empty() {
        return None;
    }
    if clean.chars().count() > MAX_RESULT_NAME {
        clean = clean.chars().take(MAX_RESULT_NAME - 1).collect();
        clean = clean.trim_end().to_owned();
        clean.push('…');
    }
    Some(clean)
}

/// Characters that take no room or change the direction of the text around them: a name
/// must read as what it is.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pending::PendingEdit;
    use spelaion_driver_api::{ColumnMeta, LogicalType, ResultSchema};

    fn dialect() -> Arc<dyn Dialect> {
        crate::dialect_of(spelaion_config::Engine::Postgres).expect("a dialect")
    }

    fn schema() -> Arc<ResultSchema> {
        Arc::new(ResultSchema::new(vec![ColumnMeta {
            name: "n".into(),
            logical: LogicalType::Int,
            native_type: "int8".into(),
            native_id: None,
            origin: None,
        }]))
    }

    struct Fixture {
        store: Arc<ResultStore>,
        tabs: Arc<ResultTabs>,
    }

    impl Fixture {
        fn new() -> Self {
            let store = Arc::new(ResultStore::new());
            let tabs = Arc::new(ResultTabs::new(store.clone()));
            Self { store, tabs }
        }

        /// Runs `script` as the session would: one result per statement.
        fn run(
            &self,
            editor: &str,
            placement: Placement,
            script: &str,
            limit: usize,
        ) -> Vec<Placed> {
            let mut run = self
                .tabs
                .begin_run(editor, placement, dialect(), script, None, limit)
                .expect("the run starts");
            self.results(&mut run, script)
        }

        fn results(&self, run: &mut RunResults, script: &str) -> Vec<Placed> {
            let mut placed = Vec::new();
            for (index, span) in dialect().split_statements(script).into_iter().enumerate() {
                run.observe(&CoreEvent::StatementStarted { index, span });
                let result = self.store.create(schema());
                placed.extend(run.observe(&CoreEvent::ResultStarted {
                    index,
                    result,
                    schema: schema(),
                }));
            }
            placed
        }

        fn ids(&self, editor: &str) -> Vec<ResultId> {
            self.tabs.list(editor).iter().map(|t| t.result).collect()
        }
    }

    #[test]
    fn a_run_replaces_what_is_not_pinned() {
        let f = Fixture::new();
        let first = f.run("a", Placement::Replace, "select 1; select 2", 20);
        let (one, two) = (first[0].tab.result, first[1].tab.result);
        f.tabs.pin("a", one, true).unwrap();

        let mut run = f
            .tabs
            .begin_run("a", Placement::Replace, dialect(), "select 3", None, 20)
            .unwrap();
        assert_eq!(run.closed(), [two]);
        let three = f.results(&mut run, "select 3")[0].tab.result;

        assert_eq!(f.ids("a"), [one, three]);
        assert!(f.store.get(one).is_some(), "the pinned result is kept");
        assert!(f.store.get(two).is_none(), "the other one is freed");

        // Unpinned, the next run lets it go.
        f.tabs.pin("a", one, false).unwrap();
        f.run("a", Placement::Replace, "select 4", 20);
        assert!(f.store.get(one).is_none());
        assert_eq!(f.store.len(), 1);
    }

    #[test]
    fn a_run_into_a_new_tab_frees_nothing() {
        let f = Fixture::new();
        let one = f.run("a", Placement::Replace, "select 1", 20)[0].tab.result;
        let two = f.run("a", Placement::NewTab, "select 2", 20)[0].tab.result;
        assert_eq!(f.ids("a"), [one, two]);
        assert_eq!(f.store.len(), 2);
        // Another editor tab's run does not touch them.
        f.run("b", Placement::Replace, "select 3", 20);
        assert_eq!(f.ids("a"), [one, two]);
    }

    #[test]
    fn a_re_run_takes_the_results_place_name_and_pin() {
        let f = Fixture::new();
        let first = f.run("a", Placement::Replace, "select 1; select 2; select 3", 20);
        let (one, two, three) = (
            first[0].tab.result,
            first[1].tab.result,
            first[2].tab.result,
        );
        f.tabs.rename("a", two, "Totals").unwrap();
        f.tabs.pin("a", two, true).unwrap();

        let mut run = f
            .tabs
            .begin_run("a", Placement::Into(two), dialect(), "select 2", None, 20)
            .unwrap();
        assert!(run.closed().is_empty());
        assert!(f.store.get(two).is_some(), "kept until the new rows start");
        let placed = f.results(&mut run, "select 2; select 22");
        assert_eq!(placed[0].replaced, Some(two));
        assert!(f.store.get(two).is_none());
        let (again, extra) = (placed[0].tab.result, placed[1].tab.result);
        assert_eq!(f.ids("a"), [one, again, extra, three]);
        assert_eq!(placed[0].tab.name.as_deref(), Some("Totals"));
        assert!(placed[0].tab.pinned);
        assert!(!placed[1].tab.pinned);
    }

    #[test]
    fn a_re_run_keeps_the_comments_name_unless_its_text_gives_another() {
        let f = Fixture::new();
        let one = f.run("a", Placement::Replace, "-- Totals\nselect 1", 20)[0]
            .tab
            .result;
        let mut run = f
            .tabs
            .begin_run("a", Placement::Into(one), dialect(), "select 1", None, 20)
            .unwrap();
        let again = f.results(&mut run, "select 1").remove(0).tab;
        assert_eq!(again.name.as_deref(), Some("Totals"));
        let mut run = f
            .tabs
            .begin_run(
                "a",
                Placement::Into(again.result),
                dialect(),
                "-- Sums\nselect 1",
                None,
                20,
            )
            .unwrap();
        let renamed = f.results(&mut run, "-- Sums\nselect 1").remove(0).tab;
        assert_eq!(renamed.name.as_deref(), Some("Sums"));
    }

    #[test]
    fn a_re_run_that_returns_nothing_leaves_the_result() {
        let f = Fixture::new();
        let one = f.run("a", Placement::Replace, "select 1", 20)[0].tab.result;
        let run = f
            .tabs
            .begin_run("a", Placement::Into(one), dialect(), "select 1", None, 20)
            .unwrap();
        drop(run);
        assert_eq!(f.ids("a"), [one]);
        assert!(f.store.get(one).is_some());
        // The next run places its results as usual, not into the old target.
        let two = f.run("a", Placement::NewTab, "select 2", 20)[0].tab.result;
        assert_eq!(f.ids("a"), [one, two]);
    }

    #[test]
    fn a_re_run_needs_a_result_of_that_editor_tab() {
        let f = Fixture::new();
        let one = f.run("a", Placement::Replace, "select 1", 20)[0].tab.result;
        let other = f
            .tabs
            .begin_run("b", Placement::Into(one), dialect(), "select 1", None, 20);
        assert_eq!(other.err(), Some(ResultTabsError::NoSuchResult));
        assert_eq!(
            f.tabs.pin("b", one, true),
            Err(ResultTabsError::NoSuchResult)
        );
        assert_eq!(
            f.tabs.rename("b", one, "x"),
            Err(ResultTabsError::NoSuchResult)
        );
        assert!(f.store.get(one).is_some());
    }

    #[test]
    fn the_limit_closes_the_oldest_unpinned_result_first() {
        let f = Fixture::new();
        let one = f.run("a", Placement::Replace, "select 1", 3)[0].tab.result;
        let two = f.run("a", Placement::NewTab, "select 2", 3)[0].tab.result;
        let three = f.run("a", Placement::NewTab, "select 3", 3)[0].tab.result;
        f.tabs.pin("a", one, true).unwrap();

        let placed = f.run("a", Placement::NewTab, "select 4", 3);
        assert_eq!(
            placed[0].closed,
            [two],
            "the pinned one is older, and stays"
        );
        assert!(f.store.get(two).is_none());
        assert_eq!(f.ids("a"), [one, three, placed[0].tab.result]);

        // A run shows every result it returns, whatever the limit.
        let many = f.run(
            "a",
            Placement::NewTab,
            "select 5; select 6; select 7; select 8",
            3,
        );
        assert_eq!(f.ids("a").len(), 5, "the pinned one and the run's four");
        assert_eq!(many.iter().map(|p| p.closed.len()).sum::<usize>(), 2);
        assert!(f.store.get(one).is_some());
        assert_eq!(f.store.len(), 5);
    }

    #[test]
    fn closing_frees_a_pinned_result() {
        let f = Fixture::new();
        let one = f.run("a", Placement::Replace, "select 1", 20)[0].tab.result;
        f.tabs.pin("a", one, true).unwrap();
        assert!(f.tabs.close(one));
        assert!(f.store.is_empty());
        assert!(f.ids("a").is_empty());
        assert!(!f.tabs.close(one));
    }

    #[test]
    fn closing_the_editor_tab_frees_everything_even_a_result_that_starts_later() {
        let f = Fixture::new();
        let one = f.run("a", Placement::Replace, "select 1", 20)[0].tab.result;
        f.tabs.pin("a", one, true).unwrap();
        let mut run = f
            .tabs
            .begin_run("a", Placement::NewTab, dialect(), "select 2", None, 20)
            .unwrap();
        assert_eq!(f.tabs.close_editor("a"), 1);
        assert!(f.results(&mut run, "select 2").is_empty());
        assert!(f.store.is_empty());
    }

    #[test]
    fn unsaved_edits_are_never_freed_by_a_run() {
        let f = Fixture::new();
        let one = f.run("a", Placement::Replace, "select 1", 1)[0].tab.result;
        f.store.append(
            one,
            arrow::array::RecordBatch::try_new(
                schema().arrow.clone(),
                vec![Arc::new(arrow::array::Int64Array::from(vec![1]))],
            )
            .unwrap(),
        );
        f.store
            .get(one)
            .unwrap()
            .write()
            .unwrap()
            .edit_pending(PendingEdit::Delete(vec![0]))
            .unwrap();

        let replace = f
            .tabs
            .begin_run("a", Placement::Replace, dialect(), "select 2", None, 1);
        assert_eq!(replace.err(), Some(ResultTabsError::PendingEdits));
        let into = f
            .tabs
            .begin_run("a", Placement::Into(one), dialect(), "select 1", None, 1);
        assert_eq!(into.err(), Some(ResultTabsError::PendingEdits));
        // Over the limit, it still stays.
        let placed = f.run("a", Placement::NewTab, "select 2", 1);
        assert!(placed[0].closed.is_empty());
        assert!(f.store.get(one).is_some());
    }

    #[test]
    fn a_leading_comment_line_names_the_result() {
        let d = dialect();
        let title = |sql: &str| statement_title(d.as_ref(), sql);
        assert_eq!(
            title("-- Totals by month\nselect 1").as_deref(),
            Some("Totals by month")
        );
        assert_eq!(title("  \n--Totals\r\nselect 1").as_deref(), Some("Totals"));
        assert_eq!(
            title("---- Totals ----\nselect 1").as_deref(),
            Some("Totals")
        );
        assert_eq!(
            title("-- first\n-- second\nselect 1").as_deref(),
            Some("first")
        );
        assert_eq!(title("select 1 -- not at the start"), None);
        assert_eq!(title("/* block */ select 1"), None);
        assert_eq!(title("--\nselect 1"), None);
        assert_eq!(title("select '-- text'"), None);
        let long = title(&format!("-- {}\nselect 1", "x".repeat(200))).unwrap();
        assert_eq!(long.chars().count(), MAX_RESULT_NAME);
        assert!(long.ends_with('…'));
        // Hidden characters and direction overrides never reach a tab's name.
        assert_eq!(
            title("-- a\u{202E}b\u{200B}c\tand  d\nselect 1").as_deref(),
            Some("abc and d")
        );
    }

    #[test]
    fn mysql_reads_two_dashes_as_a_comment_only_before_a_space() {
        let mysql = crate::dialect_of(spelaion_config::Engine::Mysql).expect("a dialect");
        assert_eq!(
            statement_title(mysql.as_ref(), "-- Totals\nselect 1").as_deref(),
            Some("Totals")
        );
        assert_eq!(statement_title(mysql.as_ref(), "--1\n"), None);
    }

    #[test]
    fn each_statement_of_a_script_is_named_by_its_own_comment() {
        let f = Fixture::new();
        let script =
            "-- one\nselect 1;\n-- two\nselect 2; -- about two\nselect 3;\n\n-- four\n\nselect 4";
        let placed = f.run("a", Placement::Replace, script, 20);
        let names: Vec<_> = placed.iter().map(|p| p.tab.name.as_deref()).collect();
        assert_eq!(names, [Some("one"), Some("two"), None, Some("four")]);
    }

    #[test]
    fn a_page_of_a_query_is_named_by_the_query_as_written() {
        let f = Fixture::new();
        let mut run = f
            .tabs
            .begin_run(
                "a",
                Placement::Replace,
                dialect(),
                "select * from (select 1) q limit 100",
                Some("-- First page\nselect 1"),
                20,
            )
            .unwrap();
        let placed = f.results(&mut run, "select * from (select 1) q limit 100");
        assert_eq!(placed[0].tab.name.as_deref(), Some("First page"));
    }

    #[test]
    fn the_users_name_wins_and_an_empty_one_gives_the_comments_back() {
        let f = Fixture::new();
        let one = f.run("a", Placement::Replace, "-- Totals\nselect 1", 20)[0]
            .tab
            .result;
        assert_eq!(
            f.tabs
                .rename("a", one, "  Mine \n")
                .unwrap()
                .name
                .as_deref(),
            Some("Mine")
        );
        assert!(f.tabs.list("a")[0].renamed);
        let back = f.tabs.rename("a", one, " ").unwrap();
        assert_eq!(back.name.as_deref(), Some("Totals"));
        // The comment's name is not one the user gave (SDB-407).
        assert!(!back.renamed);
        assert!(!f.tabs.list("a")[0].renamed);
    }
}
