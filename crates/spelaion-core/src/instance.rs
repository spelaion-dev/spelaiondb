//! One desktop app per personal folder (SDB-397): a second launch hands its command line to
//! the window already open and ends, so opening an `.sql` file from the file manager or a
//! terminal lands in the app the user is working in.
//!
//! The first app listens on a local port of this computer (127.0.0.1 only) and writes the port
//! and a random token to `desktop-instance.json` in the personal folder, which only its owner
//! can read. A second launch reads it, connects and sends the token, its folder and its
//! arguments, as one line of JSON; the first answers `ok` and reads the arguments with
//! [`crate::launch::parse`], as it read its own. Without the token nothing is accepted. A launch
//! that cannot hand over (no file, nobody listening, a wrong answer) becomes the first itself.
//!
//! It is per personal folder, not per app: a second profile (`SPELAION_USER_DIR`, the smoke
//! test's folder) is its own app and never sends anything to the user's.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Where the running app says how to reach it, in the personal folder.
pub const INSTANCE_FILE: &str = "desktop-instance.json";
/// The longest handover accepted: arguments are paths and a connection id.
const MAX_MESSAGE: u64 = 256 * 1024;
/// How long either side waits for the other.
const TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Serialize, Deserialize)]
struct InstanceFile {
    port: u16,
    token: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Handover {
    token: String,
    cwd: PathBuf,
    args: Vec<String>,
}

/// What this launch is.
pub enum Claim {
    /// The first: it serves later launches.
    First(InstanceServer),
    /// Its arguments went to the app already running: this launch ends.
    HandedOver,
    /// It could not listen (`why`): it runs alone, and a later launch opens another window.
    Alone(String),
}

/// Hands `args` (given in `cwd`) to the app running for `user_dir`, or becomes it.
pub fn claim(user_dir: &Path, args: &[String], cwd: &Path) -> Claim {
    if hand_over(user_dir, args, cwd) {
        return Claim::HandedOver;
    }
    match InstanceServer::listen(user_dir) {
        Ok(server) => Claim::First(server),
        Err(e) => Claim::Alone(e.to_string()),
    }
}

/// Sends `args` to the app running for `user_dir`; whether it took them.
pub fn hand_over(user_dir: &Path, args: &[String], cwd: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(user_dir.join(INSTANCE_FILE)) else {
        return false;
    };
    let Ok(file) = serde_json::from_str::<InstanceFile>(&text) else {
        return false;
    };
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, file.port));
    let Ok(mut stream) = TcpStream::connect_timeout(&address, TIMEOUT) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(TIMEOUT));
    let _ = stream.set_write_timeout(Some(TIMEOUT));
    let handover = Handover {
        token: file.token,
        cwd: cwd.to_path_buf(),
        args: args.to_vec(),
    };
    let Ok(mut line) = serde_json::to_string(&handover) else {
        return false;
    };
    line.push('\n');
    if stream.write_all(line.as_bytes()).is_err() {
        return false;
    }
    let mut answer = String::new();
    let _ = BufReader::new(stream.take(16)).read_line(&mut answer);
    answer.trim() == "ok"
}

/// The first app's side: it listens until the app ends.
pub struct InstanceServer {
    listener: TcpListener,
    token: String,
    file: PathBuf,
}

impl InstanceServer {
    /// Listens on a free local port and writes how to reach it, owner-only, to `user_dir`.
    pub fn listen(user_dir: &Path) -> std::io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let file = user_dir.join(INSTANCE_FILE);
        let text = serde_json::to_string(&InstanceFile {
            port,
            token: token.clone(),
        })
        .map_err(std::io::Error::other)?;
        std::fs::create_dir_all(user_dir)?;
        spelaion_config::atomic_file::replace(
            &file,
            text.as_bytes(),
            spelaion_config::atomic_file::NewFileMode::OwnerOnly,
        )?;
        Ok(Self {
            listener,
            token,
            file,
        })
    }

    /// Serves later launches on a thread of its own: `on_launch` gets each one's arguments and
    /// folder, once its token was checked.
    pub fn serve(
        self,
        on_launch: impl Fn(Vec<String>, PathBuf) + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            for stream in self.listener.incoming() {
                let Ok(stream) = stream else { continue };
                if let Some(handover) = read_handover(stream, &self.token) {
                    on_launch(handover.args, handover.cwd);
                }
            }
        })
    }

    /// The file it wrote (tests and the app's exit remove it).
    pub fn file(&self) -> &Path {
        &self.file
    }
}

/// One launch's message, if it carries the token; the launch is answered either way.
fn read_handover(stream: TcpStream, token: &str) -> Option<Handover> {
    let _ = stream.set_read_timeout(Some(TIMEOUT));
    let _ = stream.set_write_timeout(Some(TIMEOUT));
    let mut writer = stream.try_clone().ok()?;
    let mut line = String::new();
    BufReader::new(stream.take(MAX_MESSAGE))
        .read_line(&mut line)
        .ok()?;
    let accepted = serde_json::from_str::<Handover>(&line)
        .ok()
        .filter(|handover| same_token(&handover.token, token));
    let _ = writer.write_all(if accepted.is_some() { b"ok\n" } else { b"no\n" });
    accepted
}

/// Compares tokens in a time that does not depend on where they differ.
fn same_token(given: &str, expected: &str) -> bool {
    given.len() == expected.len()
        && given
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("spelaion-instance-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_second_launch_hands_its_files_to_the_first_and_ends() {
        let user_dir = dir("handover");
        let Claim::First(server) = claim(&user_dir, &[], Path::new("/")) else {
            panic!("the first launch serves");
        };
        let (tx, rx) = mpsc::channel();
        server.serve(move |args, cwd| tx.send((args, cwd)).unwrap());
        let args = vec![
            "orders.sql".to_string(),
            "--connection".into(),
            "shop".into(),
        ];
        let cwd = PathBuf::from("C:/work/reports");
        assert!(matches!(claim(&user_dir, &args, &cwd), Claim::HandedOver));
        let (got, got_cwd) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(got, args);
        assert_eq!(got_cwd, cwd);
        // And the next one too.
        assert!(hand_over(&user_dir, &["b.sql".into()], &cwd));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap().0,
            vec!["b.sql".to_string()]
        );
        let _ = std::fs::remove_dir_all(&user_dir);
    }

    #[test]
    fn without_the_token_nothing_is_taken() {
        let user_dir = dir("token");
        let server = InstanceServer::listen(&user_dir).unwrap();
        let (tx, rx) = mpsc::channel::<Vec<String>>();
        let text = std::fs::read_to_string(server.file()).unwrap();
        server.serve(move |args, _| tx.send(args).unwrap());
        let file: InstanceFile = serde_json::from_str(&text).unwrap();
        for message in [
            format!(
                "{{\"token\":\"{}x\",\"cwd\":\"/\",\"args\":[\"evil.sql\"]}}\n",
                file.token
            ),
            "{\"token\":\"\",\"cwd\":\"/\",\"args\":[\"evil.sql\"]}\n".to_string(),
            "not json\n".to_string(),
        ] {
            let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, file.port)).unwrap();
            stream.write_all(message.as_bytes()).unwrap();
            let mut answer = String::new();
            BufReader::new(stream).read_line(&mut answer).unwrap();
            assert_eq!(answer.trim(), "no", "{message}");
        }
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "nothing was taken"
        );
        let _ = std::fs::remove_dir_all(&user_dir);
    }

    #[test]
    fn a_stale_file_or_nobody_listening_makes_this_launch_the_first() {
        let user_dir = dir("stale");
        // A port nobody listens on any more.
        let gone = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = gone.local_addr().unwrap().port();
        drop(gone);
        std::fs::write(
            user_dir.join(INSTANCE_FILE),
            format!("{{\"port\":{port},\"token\":\"t\"}}"),
        )
        .unwrap();
        assert!(!hand_over(&user_dir, &["a.sql".into()], Path::new("/")));
        std::fs::write(user_dir.join(INSTANCE_FILE), "garbage").unwrap();
        assert!(matches!(
            claim(&user_dir, &[], Path::new("/")),
            Claim::First(_)
        ));
        // A second personal folder is another app: nothing is handed across.
        let other = dir("other");
        assert!(!hand_over(&other, &["a.sql".into()], Path::new("/")));
        let _ = std::fs::remove_dir_all(&user_dir);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn a_message_bigger_than_the_limit_is_cut_and_refused() {
        let user_dir = dir("big");
        let server = InstanceServer::listen(&user_dir).unwrap();
        let file: InstanceFile =
            serde_json::from_str(&std::fs::read_to_string(server.file()).unwrap()).unwrap();
        let (tx, rx) = mpsc::channel::<Vec<String>>();
        server.serve(move |args, _| tx.send(args).unwrap());
        let huge = "a".repeat(MAX_MESSAGE as usize + 10);
        let message = format!(
            "{{\"token\":\"{}\",\"cwd\":\"/\",\"args\":[\"{huge}\"]}}\n",
            file.token
        );
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, file.port)).unwrap();
        let _ = stream.write_all(message.as_bytes());
        let mut answer = String::new();
        let _ = BufReader::new(stream).read_line(&mut answer);
        assert_ne!(answer.trim(), "ok");
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
        let _ = std::fs::remove_dir_all(&user_dir);
    }
}
