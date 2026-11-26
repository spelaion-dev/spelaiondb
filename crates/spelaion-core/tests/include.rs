//! `@include` (ADR-078, point 7, SDB-468): a file tab's script puts another `.sql` file of its
//! own folder tree in itself. These try to leave the tree (`..`, links, junctions, UNC and
//! `\\?\` paths, drive-relative paths, alternate data streams, device names), break each limit,
//! include in a console, include a missing file or one that changed after the guard's question,
//! and carry an included file's open string or `DELIMITER` past its line. Each is refused
//! before anything runs. Nothing external: only the dialects, the guard and temporary folders.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use spelaion_config::Safeguards;
use spelaion_core::{
    EchoLine, IncludeError, MAX_INCLUDE_DEPTH, MAX_INCLUDED_FILES, PathRefusal, ResolvedScript,
    ScriptTree, VariableError, check_included_files, check_read_only, missing_variables,
    resolve_script, script_risks,
};
use spelaion_driver_api::Dialect;

/// Every engine whose dialect is SQL.
const SQL_ENGINES: &[&str] = &[
    "postgres",
    "mysql",
    "mariadb",
    "sqlite",
    "libsql",
    "sqlserver",
    "duckdb",
    "clickhouse",
    "trino",
    "presto",
    "oracle",
    "databend",
    "spanner",
    "hive",
    "spark",
    "kyuubi",
    "impala",
    "flightsql",
    "dremio",
    "gizmosql",
];

fn dialect(engine: &str) -> Arc<dyn Dialect> {
    spelaion_dialect::for_engine(engine).unwrap_or_else(|| panic!("{engine} has no dialect"))
}

/// A temporary folder with an open folder `root` in it and the tab's file `root/main.sql`.
struct Folder {
    base: PathBuf,
}

impl Folder {
    fn new(name: &str) -> Self {
        let base =
            std::env::temp_dir().join(format!("spelaion-include-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("root")).unwrap();
        let folder = Self { base };
        folder.write("root/main.sql", "select 0");
        folder
    }

    fn root(&self) -> PathBuf {
        self.base.join("root")
    }

    /// Writes `text` at `relative` (from the base), making its folders.
    fn write(&self, relative: &str, text: &str) -> PathBuf {
        let path = self.base.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }

    /// The tree of the tab whose file is `root/main.sql`, with `root` open.
    fn tree(&self) -> ScriptTree {
        ScriptTree::for_file(&self.root().join("main.sql"), &[self.root()]).unwrap()
    }
}

impl Drop for Folder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn resolve(engine: &str, tree: &ScriptTree, script: &str) -> Result<ResolvedScript, VariableError> {
    resolve_script(
        dialect(engine).as_ref(),
        script,
        &HashMap::new(),
        Some(tree),
    )
}

/// The statements of a resolved script, as the dialect splits its text.
fn statements(engine: &str, resolved: &ResolvedScript) -> Vec<String> {
    dialect(engine)
        .split_statements(&resolved.text)
        .iter()
        .map(|s| s.text(&resolved.text).trim().to_string())
        .collect()
}

/// The reason a path was refused, wherever the `@include` line was.
fn refusal(result: Result<ResolvedScript, VariableError>) -> PathRefusal {
    match result {
        Err(VariableError::Include(IncludeError::Path { reason, .. })) => reason,
        Err(VariableError::InFile { error, .. }) => refusal(Err(*error)),
        other => panic!("not a path refusal: {other:?}"),
    }
}

#[test]
fn an_include_puts_the_files_statements_at_its_line() {
    let _turn = crate::serial::file_turn(module_path!());
    let folder = Folder::new("in-place");
    folder.write("root/sub/a.sql", "select 2;\n\nselect 3");
    let tree = folder.tree();
    let resolved = resolve("postgres", &tree, "select 1;\n@include sub/a.sql\nselect 4").unwrap();
    assert_eq!(
        statements("postgres", &resolved),
        ["select 1", "select 2", "select 3", "select 4"]
    );
    let origins: Vec<(Option<&str>, usize)> = resolved
        .origins
        .iter()
        .map(|o| (o.file.as_deref(), o.line))
        .collect();
    assert_eq!(
        origins,
        [
            (None, 1),
            (Some("sub/a.sql"), 1),
            (Some("sub/a.sql"), 3),
            (None, 3)
        ]
    );
    assert_eq!(resolved.echoes.len(), 1);
    assert_eq!(resolved.echoes[0].before, 1);
    assert_eq!(
        resolved.echoes[0].line,
        EchoLine::Included("sub/a.sql".into())
    );
    assert_eq!(resolved.files.len(), 1);
    assert_eq!(resolved.files[0].name, "sub/a.sql");
    assert_eq!(resolved.files[0].hash.len(), 64);
}

#[test]
fn every_sql_dialect_includes_a_file() {
    let _turn = crate::serial::file_turn(module_path!());
    let folder = Folder::new("dialects");
    folder.write("root/two.sql", "select 2;\nselect 3;\n");
    let tree = folder.tree();
    for &engine in SQL_ENGINES {
        let resolved = resolve(engine, &tree, "select 1;\n@include two.sql\nselect 4")
            .unwrap_or_else(|e| panic!("{engine}: {e}"));
        assert_eq!(
            statements(engine, &resolved),
            ["select 1", "select 2", "select 3", "select 4"],
            "{engine}"
        );
    }
}

#[test]
fn paths_in_an_included_file_are_relative_to_it_inside_the_same_tree() {
    let _turn = crate::serial::file_turn(module_path!());
    let folder = Folder::new("relative");
    folder.write("root/sub/a.sql", "@include ../b.sql\n@include c.sql");
    folder.write("root/b.sql", "select 'b'");
    folder.write("root/sub/c.sql", "select 'c'");
    folder.write("outside.sql", "select 'outside'");
    let tree = folder.tree();
    let resolved = resolve("postgres", &tree, "@include sub/a.sql").unwrap();
    assert_eq!(
        statements("postgres", &resolved),
        ["select 'b'", "select 'c'"]
    );
    // `..` out of the tree, from the tab and from an included file.
    assert_eq!(
        refusal(resolve("postgres", &tree, "@include ../outside.sql")),
        PathRefusal::Outside
    );
    folder.write("root/sub/up.sql", "@include ../../outside.sql");
    let nested = resolve("postgres", &tree, "@include sub/up.sql");
    match &nested {
        Err(VariableError::InFile { file, line, .. }) => {
            assert_eq!((file.as_str(), *line), ("sub/up.sql", 1));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(refusal(nested), PathRefusal::Outside);
}

#[test]
fn the_tree_is_the_open_folder_holding_the_file_or_else_its_own_folder() {
    let _turn = crate::serial::file_turn(module_path!());
    let folder = Folder::new("tree");
    folder.write("root/shared.sql", "select 'shared'");
    folder.write("root/reports/q.sql", "select 0");
    let file = folder.root().join("reports/q.sql");
    // Inside the open folder: its whole tree.
    let open = ScriptTree::for_file(&file, &[folder.root()]).unwrap();
    assert!(resolve("postgres", &open, "@include ../shared.sql").is_ok());
    // No open folder holds it: only its own folder.
    let alone = ScriptTree::for_file(&file, &[folder.base.join("elsewhere")]).unwrap();
    assert_eq!(
        refusal(resolve("postgres", &alone, "@include ../shared.sql")),
        PathRefusal::Outside
    );
}

#[test]
fn a_path_written_to_leave_the_tree_is_refused_before_anything_is_read() {
    let _turn = crate::serial::file_turn(module_path!());
    let folder = Folder::new("written");
    let tree = folder.tree();
    let inside = folder.write("root/in.sql", "select 1");
    let verbatim = format!("\\\\?\\{}", inside.display());
    let forward = inside.display().to_string().replace('\\', "/");
    for (written, reason) in [
        ("/etc/passwd.sql", PathRefusal::Absolute),
        (
            forward.as_str(),
            if cfg!(windows) {
                PathRefusal::Forbidden("a :")
            } else {
                PathRefusal::Absolute
            },
        ),
        ("//localhost/C$/in.sql", PathRefusal::Absolute),
        ("\\\\localhost\\C$\\in.sql", PathRefusal::Forbidden("a \\")),
        (verbatim.as_str(), PathRefusal::Forbidden("a \\")),
        ("C:in.sql", PathRefusal::Forbidden("a :")),
        ("in.sql:hidden", PathRefusal::Forbidden("a :")),
        ("in.sql::$DATA", PathRefusal::Forbidden("a :")),
        ("~/in.sql", PathRefusal::Forbidden("a ~")),
        ("${dir}/in.sql", PathRefusal::Forbidden("${")),
        ("con.sql", PathRefusal::DeviceName("CON".into())),
        ("sub/NUL.sql", PathRefusal::DeviceName("NUL".into())),
        ("in.sql.", PathRefusal::TrailingDot),
        ("in.txt", PathRefusal::NotSql),
        ("", PathRefusal::Empty),
    ] {
        assert_eq!(
            refusal(resolve("postgres", &tree, &format!("@include {written}"))),
            reason,
            "{written:?}"
        );
    }
}

#[test]
fn a_missing_file_or_a_folder_stops_the_run_before_its_first_statement() {
    let _turn = crate::serial::file_turn(module_path!());
    let folder = Folder::new("missing");
    std::fs::create_dir_all(folder.root().join("dir.sql")).unwrap();
    let tree = folder.tree();
    assert_eq!(
        refusal(resolve("postgres", &tree, "select 1;\n@include nope.sql")),
        PathRefusal::NotFound
    );
    assert_eq!(
        refusal(resolve("postgres", &tree, "select 1;\n@include dir.sql")),
        PathRefusal::NotAFile
    );
    // A file that is not text, as `read_sql_file` refuses it.
    std::fs::write(folder.root().join("bin.sql"), b"select\0").unwrap();
    assert!(matches!(
        resolve("postgres", &tree, "@include bin.sql"),
        Err(VariableError::Include(IncludeError::Read(..)))
    ));
}

#[test]
fn a_console_has_no_folder_to_include_from() {
    let _turn = crate::serial::file_turn(module_path!());
    let d = dialect("postgres");
    assert_eq!(
        resolve_script(
            d.as_ref(),
            "select 1;\n@include a.sql",
            &HashMap::new(),
            None
        ),
        Err(VariableError::Include(IncludeError::Console))
    );
    // A console's script without one still runs its other commands.
    assert!(resolve_script(d.as_ref(), "@set x = 1\nselect ${x}", &HashMap::new(), None).is_ok());
}

#[test]
fn a_file_that_includes_itself_is_refused_and_the_same_file_twice_in_a_row_is_not() {
    let _turn = crate::serial::file_turn(module_path!());
    let folder = Folder::new("cycle");
    folder.write("root/a.sql", "select 'a';\n@include b.sql");
    folder.write("root/b.sql", "@include a.sql");
    folder.write("root/once.sql", "select 'once'");
    folder.write("root/back.sql", "@include main.sql");
    let tree = folder.tree();
    let cycle = |file: &str, again: &str| VariableError::InFile {
        file: file.into(),
        line: 1,
        error: Box::new(VariableError::Include(IncludeError::Cycle(again.into()))),
    };
    assert_eq!(
        resolve("postgres", &tree, "@include a.sql"),
        Err(cycle("b.sql", "a.sql"))
    );
    // The tab's own file, from a file it includes.
    assert_eq!(
        resolve("postgres", &tree, "@include back.sql"),
        Err(cycle("back.sql", "main.sql"))
    );
    let twice = resolve("postgres", &tree, "@include once.sql\n@include once.sql").unwrap();
    assert_eq!(
        statements("postgres", &twice),
        ["select 'once'", "select 'once'"]
    );
    // Read once, for both.
    assert_eq!(twice.files.len(), 1);
}

#[test]
fn each_limit_is_refused_one_past_it() {
    let _turn = crate::serial::file_turn(module_path!());
    let folder = Folder::new("limits");
    // Depth: level n includes level n + 1, the last level selects.
    let chain = |levels: usize| {
        for level in 1..=levels {
            let text = if level < levels {
                format!("select {level};\n@include l{}.sql", level + 1)
            } else {
                "select 'end'".to_string()
            };
            folder.write(&format!("root/l{level}.sql"), &text);
        }
    };
    let tree = folder.tree();
    chain(MAX_INCLUDE_DEPTH);
    let deep = resolve("postgres", &tree, "@include l1.sql").unwrap();
    assert_eq!(deep.origins.len(), MAX_INCLUDE_DEPTH);
    chain(MAX_INCLUDE_DEPTH + 1);
    let error = resolve("postgres", &tree, "@include l1.sql").unwrap_err();
    assert!(error.to_string().contains("levels of @include"), "{error}");

    // Files: each `@include` read counts.
    folder.write("root/one.sql", "select 1");
    let many = |n: usize| {
        (0..n)
            .map(|_| "@include one.sql")
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(resolve("postgres", &tree, &many(MAX_INCLUDED_FILES)).is_ok());
    assert!(matches!(
        resolve("postgres", &tree, &many(MAX_INCLUDED_FILES + 1)),
        Err(VariableError::Include(IncludeError::TooMany(_)))
    ));

    // Text: 15 MiB of comment, five times, is past 64 MiB.
    let big = format!("-- {}\n", "x".repeat(15 * 1024 * 1024));
    folder.write("root/big.sql", &big);
    let four = (0..4)
        .map(|_| "@include big.sql")
        .collect::<Vec<_>>()
        .join("\n");
    assert!(resolve("postgres", &tree, &four).is_ok());
    let five = format!("{four}\n@include big.sql");
    assert!(matches!(
        resolve("postgres", &tree, &five),
        Err(VariableError::Include(IncludeError::TooMuchText(_)))
    ));
}

#[test]
fn a_file_changed_after_the_guard_asked_refuses_the_run() {
    let _turn = crate::serial::file_turn(module_path!());
    let folder = Folder::new("changed");
    let path = folder.write("root/del.sql", "delete from t where id = 1");
    let tree = folder.tree();
    let resolved = resolve("postgres", &tree, "@include del.sql").unwrap();
    assert_eq!(check_included_files(&resolved.files), Ok(()));
    std::fs::write(&path, "delete from t").unwrap();
    assert_eq!(
        check_included_files(&resolved.files),
        Err(IncludeError::Changed("del.sql".into()))
    );
    std::fs::remove_file(&path).unwrap();
    assert_eq!(
        check_included_files(&resolved.files),
        Err(IncludeError::Changed("del.sql".into()))
    );
}

#[test]
fn an_included_files_open_string_or_delimiter_never_reaches_past_its_line() {
    let _turn = crate::serial::file_turn(module_path!());
    let folder = Folder::new("contained");
    folder.write("root/open.sql", "select 'never closed");
    folder.write(
        "root/proc.sql",
        "DELIMITER $$\ncreate procedure p() begin select 1; select 2; end$$\n",
    );
    folder.write("root/block.sql", "begin\n  null;\nend;\n/\n");
    let tree = folder.tree();
    // The string would swallow `drop table t`: nothing runs.
    assert!(matches!(
        resolve("postgres", &tree, "@include open.sql\ndrop table t"),
        Err(VariableError::Include(IncludeError::Unjoinable(_)))
    ));
    // The procedure stays one statement, and the tab's `;` still ends its statements.
    let resolved = resolve("mysql", &tree, "@include proc.sql\nselect 3;\nselect 4").unwrap();
    assert_eq!(
        statements("mysql", &resolved),
        [
            "create procedure p() begin select 1; select 2; end",
            "select 3",
            "select 4"
        ]
    );
    // A PL/SQL block closed by its `/` line.
    let resolved = resolve("oracle", &tree, "@include block.sql\nselect 1 from dual").unwrap();
    assert_eq!(
        statements("oracle", &resolved).len(),
        2,
        "{:?}",
        resolved.text
    );
}

#[test]
fn the_guard_and_the_read_only_check_read_the_included_statements() {
    let _turn = crate::serial::file_turn(module_path!());
    let folder = Folder::new("guard");
    folder.write("root/sub/clean.sql", "select 1;\n\ndelete from orders");
    let tree = folder.tree();
    let resolved = resolve("postgres", &tree, "select 0;\n@include sub/clean.sql").unwrap();
    let d = dialect("postgres");
    let risky = script_risks(d.as_ref(), &resolved.text, &Safeguards::default());
    assert_eq!(risky.len(), 1);
    let origin = &resolved.origins[risky[0].index];
    assert_eq!(
        (origin.file.as_deref(), origin.line),
        (Some("sub/clean.sql"), 3)
    );
    // A write in an included file refuses a read-only connection's script before anything runs.
    let refused = check_read_only(d.as_ref(), &resolved.text).unwrap_err();
    assert_eq!(
        resolved.origins[refused.index].file.as_deref(),
        Some("sub/clean.sql")
    );
}

#[test]
fn a_set_in_an_included_file_stays_in_force_after_it() {
    let _turn = crate::serial::file_turn(module_path!());
    let folder = Folder::new("set");
    folder.write("root/vars.sql", "@set t = users\n@set n = ${never}");
    folder.write("root/ok.sql", "@set t = users");
    let tree = folder.tree();
    let resolved = resolve("postgres", &tree, "@include ok.sql\nselect * from ${t}").unwrap();
    assert_eq!(statements("postgres", &resolved), ["select * from users"]);
    // Names an included file gives are not asked for; those it uses without one are.
    folder.write("root/uses.sql", "select ${asked}");
    let d = dialect("postgres");
    assert_eq!(
        missing_variables(
            d.as_ref(),
            "@include ok.sql\nselect ${t}",
            &HashMap::new(),
            Some(&tree)
        ),
        Vec::<String>::new()
    );
    assert_eq!(
        missing_variables(
            d.as_ref(),
            "@include uses.sql",
            &HashMap::new(),
            Some(&tree)
        ),
        ["asked"]
    );
    // An error in an included file says which file and line.
    folder.write("root/bad.sql", "select 1;\n@unset not a name");
    let error = resolve("postgres", &tree, "@include bad.sql").unwrap_err();
    assert!(
        error.to_string().starts_with("bad.sql, line 2: @unset"),
        "{error}"
    );
}

#[cfg(unix)]
#[test]
fn a_symbolic_link_out_of_the_tree_or_to_a_file_that_is_not_sql_is_refused() {
    let _turn = crate::serial::file_turn(module_path!());
    use std::os::unix::fs::symlink;
    let folder = Folder::new("unix-links");
    let outside = folder.write("secret.sql", "select 'secret'");
    let notes = folder.write("root/notes.txt", "not sql");
    std::fs::create_dir_all(folder.base.join("away")).unwrap();
    folder.write("away/x.sql", "select 'away'");
    symlink(&outside, folder.root().join("in.sql")).unwrap();
    symlink(&notes, folder.root().join("x.sql")).unwrap();
    symlink(folder.base.join("away"), folder.root().join("linked")).unwrap();
    let tree = folder.tree();
    assert_eq!(
        refusal(resolve("postgres", &tree, "@include in.sql")),
        PathRefusal::Outside
    );
    assert_eq!(
        refusal(resolve("postgres", &tree, "@include x.sql")),
        PathRefusal::NotSql
    );
    assert_eq!(
        refusal(resolve("postgres", &tree, "@include linked/x.sql")),
        PathRefusal::Outside
    );
    // A link that stays inside is followed.
    folder.write("root/real.sql", "select 'real'");
    symlink(
        folder.root().join("real.sql"),
        folder.root().join("alias.sql"),
    )
    .unwrap();
    assert!(resolve("postgres", &tree, "@include alias.sql").is_ok());
}

#[cfg(windows)]
#[test]
fn a_junction_or_a_link_out_of_the_tree_or_to_a_file_that_is_not_sql_is_refused() {
    let _turn = crate::serial::file_turn(module_path!());
    let folder = Folder::new("windows-links");
    std::fs::create_dir_all(folder.base.join("away")).unwrap();
    folder.write("away/x.sql", "select 'away'");
    // A junction needs no privilege.
    let made = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(folder.root().join("linked"))
        .arg(folder.base.join("away"))
        .output()
        .unwrap();
    assert!(made.status.success(), "{made:?}");
    let tree = folder.tree();
    assert_eq!(
        refusal(resolve("postgres", &tree, "@include linked/x.sql")),
        PathRefusal::Outside
    );
    // A symbolic link needs developer mode or an administrator (CI's runners are): tried, and
    // said when it cannot be made.
    let outside = folder.write("secret.sql", "select 'secret'");
    let notes = folder.write("root/notes.txt", "not sql");
    let link_out = std::os::windows::fs::symlink_file(&outside, folder.root().join("in.sql"));
    let link_txt = std::os::windows::fs::symlink_file(&notes, folder.root().join("x.sql"));
    match (link_out, link_txt) {
        (Ok(()), Ok(())) => {
            assert_eq!(
                refusal(resolve("postgres", &tree, "@include in.sql")),
                PathRefusal::Outside
            );
            assert_eq!(
                refusal(resolve("postgres", &tree, "@include x.sql")),
                PathRefusal::NotSql
            );
        }
        (out, txt) => eprintln!(
            "symbolic links could not be made here ({out:?}, {txt:?}): only the junction was tried"
        ),
    }
}
