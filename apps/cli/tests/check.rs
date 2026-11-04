//! `spelaiondb check` (ADR-022): the real binary against a repository with every kind of
//! problem, as a team's CI or pre-commit hook would run it.

use std::path::{Path, PathBuf};
use std::process::Command;

struct Repo(PathBuf);

impl Repo {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("spelaion-check-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".spelaion/connections")).unwrap();
        Self(dir)
    }

    fn write(&self, relative: &str, content: &str) {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    /// Runs `spelaiondb check` with no personal directory at all: CI has none.
    fn check(&self, args: &[&str]) -> (bool, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_spelaiondb"))
            .arg("check")
            .arg(&self.0)
            .args(args)
            .env("SPELAION_USER_DIR", self.0.join("no-such-user-dir"))
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text.replace('\\', "/"))
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const GOOD: &str = "schema_version = 1\nengine = \"postgres\"\nenv = \"prod\"\nhost = \"db.internal\"\ndatabase = \"orders\"\nuser = \"${user.db_user}\"\npassword = { secret = \"keychain\" }\n\n[tls]\nmode = \"verify-full\"\ncert = \"certs/${user.db_user}.crt\"\nkey = \"~/.ssl/${user.db_user}.key\"\n";

#[test]
fn a_clean_repository_passes() {
    let repo = Repo::new("clean");
    repo.write(".spelaion/connections/orders.toml", GOOD);
    repo.write(
        ".spelaion/tunnels/bastion.toml",
        "schema_version = 1\n\n[[hops]]\nhost = \"bastion.internal\"\nuser = \"${user.ssh_user}\"\nauth = [\"agent\"]\nidentity_file = \"~/.ssh/id_ed25519\"\n",
    );
    repo.write(
        ".spelaion/known_hosts",
        "# reviewed\nbastion.internal ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIC0Cx5aAdFPmIIPx6ZxdIYFKbARZbBbB5NV0QIO3Qdp2\n",
    );
    let (ok, out) = repo.check(&[]);
    assert!(ok, "{out}");
    assert!(out.contains("0 error(s), 0 warning(s)"), "{out}");
}

#[test]
fn errors_fail_and_say_where_and_why() {
    let repo = Repo::new("errors");
    let conn = |name: &str, body: &str| {
        repo.write(
            &format!(".spelaion/connections/{name}.toml"),
            &format!("schema_version = 1\nengine = \"postgres\"\nhost = \"db\"\ndatabase = \"d\"\nuser = \"u\"\n{body}"),
        )
    };
    conn("typo", "databse = \"x\"\n");
    conn(
        "inline",
        "password = { secret = \"cmd:PGPASSWORD=hunter2 psql -c 'select 1'\" }\n",
    );
    conn(
        "url",
        "password = { secret = \"cmd:curl https://ops:hunter2@vault.internal/db\" }\n",
    );
    conn(
        "flag",
        "password = { secret = \"cmd:mysql --password=hunter2\" }\n",
    );
    conn("nosuch", "tunnel = \"missing\"\n");
    conn("badvar", "port = 5432\n[tls]\nca = \"${home}/ca.pem\"\n");
    conn(
        "keyinside",
        "[tls]\ncert = \"certs/me.crt\"\nkey = \"certs/me.key\"\n",
    );
    conn("badref", "password = { secret = \"vault://x\" }\n");
    repo.write(
        ".spelaion/certs/me.key",
        "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIA==\n-----END PRIVATE KEY-----\n",
    );

    let (ok, out) = repo.check(&[]);
    assert!(!ok, "errors must fail the check: {out}");
    for (file, why) in [
        ("connections/typo.toml", "databse"),
        ("connections/inline.toml", "PGPASSWORD="),
        ("connections/url.toml", "user:password@"),
        ("connections/flag.toml", "--password"),
        (
            "connections/nosuch.toml",
            "tunnel \"missing\" does not exist",
        ),
        ("connections/badvar.toml", "${home} is not a variable"),
        ("connections/keyinside.toml", "inside the repository"),
        ("connections/badref.toml", "password"),
        ("certs/me.key", "holds a private key"),
    ] {
        assert!(
            out.lines()
                .any(|l| l.contains(file) && l.contains("error:") && l.contains(why)),
            "{file}: {why}\n{out}"
        );
    }
    // The secrets themselves are never repeated beyond the file's own text.
    assert!(!out.contains("MC4CAQAwBQYDK2VwBCIEIA=="), "{out}");
}

#[test]
fn warnings_pass_unless_strict() {
    let repo = Repo::new("warnings");
    repo.write(
        ".spelaion/connections/agents.toml",
        "schema_version = 1\nengine = \"postgres\"\nenv = \"prod\"\nhost = \"db\"\ndatabase = \"d\"\nuser = \"u\"\n\n[tls]\nmode = \"require\"\n\n[mcp]\nexposed = true\nwrites = \"allow\"\n",
    );
    repo.write(
        ".spelaion/tunnels/keyed.toml",
        "schema_version = 1\n\n[[hops]]\nhost = \"b\"\nuser = \"u\"\nauth = [\"key\"]\nidentity_file = \"keys/id_ed25519\"\n",
    );
    repo.write(".spelaion/known_hosts", "bastion.internal ssh-ed25519\n");

    let (ok, out) = repo.check(&[]);
    assert!(ok, "warnings alone pass: {out}");
    for why in [
        "lets AI agents write",
        "does not verify the server's certificate",
        "identity_file \"keys/id_ed25519\" is inside the repository",
        "line 1: expected",
    ] {
        assert!(
            out.lines()
                .any(|l| l.contains("warning:") && l.contains(why)),
            "{why}\n{out}"
        );
    }
    let (ok, out) = repo.check(&["--strict"]);
    assert!(!ok, "--strict fails on warnings: {out}");
}

#[test]
fn outside_a_repository_it_says_so() {
    let dir = std::env::temp_dir().join(format!("spelaion-check-none-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_spelaiondb"))
        .args(["check", dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("is not inside a repository"));
    let _ = std::fs::remove_dir_all(Path::new(&dir));
}
