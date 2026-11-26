//! The client commands of a script the editor runs (ADR-078): `@set`, `@unset`, `@echo` and
//! `@include`, resolved after the script is split and before anything reads it. What this
//! returns is the text that runs, and what the guard, the connection type's confirmations and
//! the read-only check read: they never see a command, and see every included statement.
//!
//! The tab's own text is kept as written, without its command lines. An included file is a
//! script of its own, split alone in the dialect's starting state, and each of its statements
//! is written into the run's text on its own, closed so that it ends where it ended in its
//! file: a string it leaves open or a `DELIMITER` it sets never reaches the text after its
//! `@include` line. The whole text is then split again, and must give exactly the statements
//! that were written, in order, or nothing runs.
//!
//! Which files an `@include` may read, and how many, is `include.rs`'s.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use spelaion_driver_api::Dialect;

use crate::include::{
    IncludeError, IncludedFile, MAX_INCLUDE_DEPTH, MAX_INCLUDED_BYTES, MAX_INCLUDED_FILES,
    ScriptTree,
};
use crate::variables::{
    VariableError, is_variable_name, is_variable_value, names_of, script_variables,
    substitute_statement, substitute_variables, variables,
};

/// What the Messages tab shows when the run reaches it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EchoLine {
    /// An `@echo` line's text, as the script wrote it (ADR-078, point 6).
    Echo(String),
    /// An included file starts: its name, relative to the script's tree (ADR-078, point 7).
    Included(String),
}

/// A line for the Messages tab, by the statement it comes before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptEcho {
    /// The statement of the resolved script it comes before, counted from 0; the number of
    /// statements when it comes after the last one.
    pub before: usize,
    pub line: EchoLine,
}

/// Where a statement of the resolved script was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementOrigin {
    /// The included file, relative to the script's tree with `/`; `None` for the text the tab
    /// sent.
    pub file: Option<String>,
    /// Its first line in that text, from 1.
    pub line: usize,
}

/// A script with its client commands resolved (ADR-078): what the editor's run sends and what
/// its guard and read-only check read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedScript {
    /// The statements, with each `${name}` written in as the values stood at that statement,
    /// every included file's statements in place of its `@include` line, and no command line.
    pub text: String,
    /// The `@echo` lines and the included files' names, by the statement each comes before.
    pub echoes: Vec<ScriptEcho>,
    /// Where each statement of `text` comes from, in order.
    pub origins: Vec<StatementOrigin>,
    /// The files read, once each, with the hash of what was read: a run whose files changed
    /// since is refused (`check_included_files`).
    pub files: Vec<IncludedFile>,
}

/// What a client command line of the editor's run does.
enum Command<'a> {
    Set(&'a str, &'a str),
    Unset(&'a str),
    Echo(&'a str),
    Include(&'a str),
}

/// Reads a `@set`, `@unset`, `@echo` or `@include` line; `None` for any other statement (an
/// `@export`, an unknown `@word` or SQL), which the run leaves as it is: the session refuses a
/// command line that reaches it.
fn command(statement: &str) -> Option<Result<Command<'_>, VariableError>> {
    let word = spelaion_dialect::client_command(statement)?;
    let rest = statement.trim_start()[1 + word.len()..].trim();
    match word.as_str() {
        "set" => Some(match rest.split_once('=') {
            Some((name, value)) if is_variable_name(name.trim()) => {
                let value = value.trim();
                if is_variable_value(value) {
                    Ok(Command::Set(name.trim(), value))
                } else {
                    Err(VariableError::InvalidValue(name.trim().to_string()))
                }
            }
            _ => Err(VariableError::Command(
                "@set is written @set name = value, with a name of letters, digits and _".into(),
            )),
        }),
        "unset" => Some(if is_variable_name(rest) {
            Ok(Command::Unset(rest))
        } else {
            Err(VariableError::Command(
                "@unset is written @unset name, with a name of letters, digits and _".into(),
            ))
        }),
        "echo" => Some(Ok(Command::Echo(rest))),
        "include" => Some(Ok(Command::Include(rest))),
        _ => None,
    }
}

/// Resolves the client commands of a script the editor runs (ADR-078, points 4 to 7): walks its
/// statements in order, sends nothing, and writes each statement's `${name}`s as the values
/// stand there. The run starts from `values` (the tab's over its connection's); `@set` replaces
/// a value from its line on and `@unset` removes it, for this run only: `values` itself is never
/// changed. A value is used exactly as [`substitute_variables`] uses one, with its refusals; a
/// `@set` value is the rest of its line, trimmed, taken as it is. `@include` reads from `tree`,
/// the tab's file's (`None` for a console, which refuses it), every file before anything runs.
/// A dialect without command lines (one that is not SQL) gets [`substitute_variables`] alone.
///
/// Blocks on the file system when the script includes a file.
pub fn resolve_script(
    dialect: &dyn Dialect,
    script: &str,
    values: &HashMap<String, String>,
    tree: Option<&ScriptTree>,
) -> Result<ResolvedScript, VariableError> {
    if !dialect.is_sql() {
        let text = substitute_variables(dialect, script, values)?;
        let origins = dialect
            .split_statements(&text)
            .iter()
            .map(|span| StatementOrigin {
                file: None,
                line: line_at(&text, span.start),
            })
            .collect();
        return Ok(ResolvedScript {
            text,
            echoes: Vec::new(),
            origins,
            files: Vec::new(),
        });
    }
    let mut walk = Walk::new(dialect, tree, values, Mode::Resolve);
    walk.tab(script)?;
    let Walk {
        out,
        expected,
        origins,
        echoes,
        files,
        ..
    } = walk;
    // The whole text gives exactly the statements written, in order.
    let spans = dialect.split_statements(&out);
    let first_wrong =
        (0..expected.len().max(spans.len())).find(|&i| match (spans.get(i), expected.get(i)) {
            (Some(span), Some(written)) => span.text(&out).trim() != written.trim(),
            _ => true,
        });
    if let Some(i) = first_wrong {
        // Next to an included statement, it is how that file's statement was closed.
        let included = [i, i.saturating_sub(1)]
            .into_iter()
            .find_map(|j| origins.get(j).and_then(|o| o.file.clone()));
        return Err(match included {
            Some(file) => IncludeError::Unjoinable(file).into(),
            None => VariableError::SplitsStatement(names_of(
                &script_variables(dialect, script)
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
            )),
        });
    }
    Ok(ResolvedScript {
        text: out,
        echoes,
        origins,
        files,
    })
}

/// The variables of `script` that need a value before it can run: used by a statement, its own
/// or an included file's, while neither `values` nor an earlier `@set` gives one. A name an
/// earlier `@unset` removed is not asked for: the run is refused there, as the script says
/// (ADR-078, point 5). Whatever [`resolve_script`] would refuse (a path, a file) is left for it
/// to say: the names found before it are given.
///
/// Blocks on the file system when the script includes a file.
pub fn missing_variables(
    dialect: &dyn Dialect,
    script: &str,
    values: &HashMap<String, String>,
    tree: Option<&ScriptTree>,
) -> Vec<String> {
    if !dialect.is_sql() {
        let mut missing: Vec<String> = Vec::new();
        for name in script_variables(dialect, script) {
            if !values.contains_key(&name) {
                missing.push(name);
            }
        }
        return missing;
    }
    let mut walk = Walk::new(dialect, tree, values, Mode::Collect);
    // An error stops the walk: what it found so far is asked for.
    let _ = walk.tab(script);
    walk.missing
}

/// Whether the walk writes the run's text or only notes the names that need a value.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Resolve,
    Collect,
}

/// One resolution: the files it read are read once, and are not read again.
struct Walk<'a> {
    dialect: &'a dyn Dialect,
    tree: Option<&'a ScriptTree>,
    mode: Mode,
    values: HashMap<String, String>,
    /// Names an `@unset` removed (collect mode: not asked for).
    unset: HashSet<String>,
    missing: Vec<String>,
    out: String,
    /// Each statement as written into `out`, in order.
    expected: Vec<String>,
    origins: Vec<StatementOrigin>,
    echoes: Vec<ScriptEcho>,
    files: Vec<IncludedFile>,
    /// What each file read held, by its canonical path.
    texts: HashMap<PathBuf, Arc<str>>,
    /// The files being walked, the tab's own first: one of them again is a cycle.
    stack: Vec<PathBuf>,
    inclusions: usize,
    included_bytes: usize,
}

/// How an included statement can be closed, tried in order: the one whose statement splits
/// back alone and unchanged is used. `;` for most dialects, a `/` line for a PL/SQL block, a
/// `GO` line for a T-SQL batch, and MySQL's own delimiter for a body with `;` inside.
const CLOSINGS: &[(&str, &str)] = &[
    ("", ";\n"),
    ("", "\n;\n"),
    ("", "\n/\n"),
    ("", "\nGO\n"),
    ("DELIMITER $$\n", "\n$$\nDELIMITER ;\n"),
];

impl<'a> Walk<'a> {
    fn new(
        dialect: &'a dyn Dialect,
        tree: Option<&'a ScriptTree>,
        values: &HashMap<String, String>,
        mode: Mode,
    ) -> Self {
        Self {
            dialect,
            tree,
            mode,
            values: values.clone(),
            unset: HashSet::new(),
            missing: Vec::new(),
            out: String::new(),
            expected: Vec::new(),
            origins: Vec::new(),
            echoes: Vec::new(),
            files: Vec::new(),
            texts: HashMap::new(),
            stack: tree
                .map(|t| vec![t.file().to_path_buf()])
                .unwrap_or_default(),
            inclusions: 0,
            included_bytes: 0,
        }
    }

    /// The text the tab sent: kept as written, without its command lines.
    fn tab(&mut self, script: &str) -> Result<(), VariableError> {
        self.out.reserve(script.len());
        let mut copied = 0;
        let mut lines = Lines::default();
        for span in self.dialect.split_statements(script) {
            let Some(statement) = script.get(span.start..span.end) else {
                continue;
            };
            if span.start < copied {
                continue;
            }
            match command(statement).transpose()? {
                Some(command) => {
                    // The line is the run's, not the server's: it leaves the text.
                    self.out.push_str(&script[copied..span.start]);
                    copied = span.end;
                    let dir = self.tree.map(|t| t.dir().to_path_buf());
                    self.run(command, dir.as_deref())?;
                }
                None => {
                    self.origins.push(StatementOrigin {
                        file: None,
                        line: lines.at(script, span.start),
                    });
                    let written = self.statement(statement)?;
                    if let Some(written) = &written {
                        self.out.push_str(&script[copied..span.start]);
                        self.out.push_str(written);
                        copied = span.end;
                    }
                    self.expected
                        .push(written.unwrap_or_else(|| statement.to_string()));
                }
            }
        }
        self.out.push_str(&script[copied..]);
        Ok(())
    }

    /// An included file's text: each statement written on its own and closed.
    fn file(&mut self, text: &str, name: &str, dir: &Path) -> Result<(), VariableError> {
        let mut lines = Lines::default();
        for span in self.dialect.split_statements(text) {
            let Some(statement) = text.get(span.start..span.end) else {
                continue;
            };
            let line = lines.at(text, span.start);
            let at = |error: VariableError| match error {
                // Placed already: in a file this one includes, or naming its own file.
                VariableError::InFile { .. }
                | VariableError::Include(IncludeError::Unjoinable(_)) => error,
                other => VariableError::InFile {
                    file: name.to_string(),
                    line,
                    error: Box::new(other),
                },
            };
            match command(statement).transpose().map_err(at)? {
                Some(command) => self.run(command, Some(dir)).map_err(at)?,
                None => {
                    self.origins.push(StatementOrigin {
                        file: Some(name.to_string()),
                        line,
                    });
                    let written = self
                        .statement(statement)
                        .map_err(at)?
                        .unwrap_or_else(|| statement.to_string());
                    if self.mode == Mode::Resolve {
                        let closed = closed(self.dialect, &written)
                            .ok_or_else(|| IncludeError::Unjoinable(name.to_string()))?;
                        self.out.push_str(&closed);
                    }
                    self.expected.push(written);
                }
            }
        }
        Ok(())
    }

    /// A statement with its variables written in; `None` when it has none. In collect mode,
    /// notes the names without a value and writes nothing.
    fn statement(&mut self, statement: &str) -> Result<Option<String>, VariableError> {
        let found = variables(self.dialect, statement);
        if found.is_empty() {
            return Ok(None);
        }
        match self.mode {
            Mode::Resolve => {
                substitute_statement(self.dialect, statement, &found, &self.values).map(Some)
            }
            Mode::Collect => {
                for (_, name) in found {
                    if !self.values.contains_key(name)
                        && !self.unset.contains(name)
                        && !self.missing.iter().any(|m| m == name)
                    {
                        self.missing.push(name.to_string());
                    }
                }
                Ok(None)
            }
        }
    }

    /// What a command line does, from a file in `dir` (`None`: a console's text).
    fn run(&mut self, command: Command<'_>, dir: Option<&Path>) -> Result<(), VariableError> {
        match command {
            Command::Set(name, value) => {
                self.values.insert(name.to_string(), value.to_string());
                self.unset.remove(name);
            }
            Command::Unset(name) => {
                self.values.remove(name);
                self.unset.insert(name.to_string());
            }
            Command::Echo(text) => self.echoes.push(ScriptEcho {
                before: self.origins.len(),
                line: EchoLine::Echo(text.to_string()),
            }),
            Command::Include(written) => self.include(written, dir)?,
        }
        Ok(())
    }

    /// Puts the file `written` names, from a file in `dir`, in the run (ADR-078, point 7).
    fn include(&mut self, written: &str, dir: Option<&Path>) -> Result<(), VariableError> {
        let (Some(tree), Some(dir)) = (self.tree, dir) else {
            return Err(IncludeError::Console.into());
        };
        // The tab's script is level 0; the file it includes, level 1.
        if self.stack.len() > MAX_INCLUDE_DEPTH {
            return Err(IncludeError::TooDeep(written.to_string()).into());
        }
        let path = tree.locate(dir, written)?;
        let name = tree.name_of(&path);
        if self.stack.contains(&path) {
            return Err(IncludeError::Cycle(name).into());
        }
        self.inclusions += 1;
        if self.inclusions > MAX_INCLUDED_FILES {
            return Err(IncludeError::TooMany(name).into());
        }
        let text = match self.texts.get(&path) {
            Some(text) => Arc::clone(text),
            None => {
                let file = tree.read(&path)?;
                let text: Arc<str> = Arc::from(file.text);
                self.files.push(IncludedFile {
                    path: path.clone(),
                    name: name.clone(),
                    hash: file.hash,
                });
                self.texts.insert(path.clone(), Arc::clone(&text));
                text
            }
        };
        self.included_bytes += text.len();
        if self.included_bytes > MAX_INCLUDED_BYTES {
            return Err(IncludeError::TooMuchText(name).into());
        }
        self.echoes.push(ScriptEcho {
            before: self.origins.len(),
            line: EchoLine::Included(name.clone()),
        });
        let dir = path.parent().unwrap_or(tree.root()).to_path_buf();
        self.stack.push(path);
        self.file(&text, &name, &dir)?;
        self.stack.pop();
        Ok(())
    }
}

/// `statement` closed so that, read alone, it splits back as itself and nothing more; `None`
/// when no closing does that.
fn closed(dialect: &dyn Dialect, statement: &str) -> Option<String> {
    CLOSINGS.iter().find_map(|(before, after)| {
        if before.contains("$$") && statement.contains("$$") {
            return None;
        }
        let text = format!("{before}{statement}{after}");
        let spans = dialect.split_statements(&text);
        (spans.len() == 1 && spans[0].text(&text).trim() == statement.trim()).then_some(text)
    })
}

/// The line of a byte offset, counted forwards through one text.
#[derive(Default)]
struct Lines {
    offset: usize,
    line: usize,
}

impl Lines {
    fn at(&mut self, text: &str, offset: usize) -> usize {
        if offset < self.offset {
            *self = Self::default();
        }
        self.line += text
            .get(self.offset..offset)
            .map_or(0, |between| between.matches('\n').count());
        self.offset = offset;
        self.line + 1
    }
}

/// The line of `offset` in `text`, from 1.
fn line_at(text: &str, offset: usize) -> usize {
    Lines::default().at(text, offset)
}
