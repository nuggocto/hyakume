//! Journeys through the built `hyakume` binary: real processes, sockets,
//! files, and a pseudo-terminal from `script`, so prompts are answered the
//! way a person answers them. Tests wait for readiness lines and observable
//! state, not for time to pass, except where a deadline is under test.

// Test helpers may stop the test on unexpected failures.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use rand_core::{OsRng, RngCore};
use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_hyakume");
const PASSPHRASE: &str = "correct horse battery staple";
const DEADLINE: Duration = Duration::from_secs(60);
/// The consent question at the start of a line, where only the receiving
/// command writes it; a preview line starts with its gutter instead.
const QUESTION: &str = "\nAccept this offer? [y/N]";

/// A temporary directory for one test, removed when it ends.
struct Home(PathBuf);

impl Home {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "hyakume-cli-{}-{:x}",
            std::process::id(),
            OsRng.next_u64()
        ));
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }

    /// A new private folder inside the home.
    fn folder(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One service process and the directories it owns.
struct Service {
    /// The `hyakume` binary this service and its commands run.
    bin: PathBuf,
    state: PathBuf,
    run: PathBuf,
    log: PathBuf,
    child: Option<Child>,
}

impl Service {
    fn new(home: &Home, name: &str) -> Self {
        Self::with_bin(home, name, Path::new(BIN))
    }

    fn with_bin(home: &Home, name: &str, bin: &Path) -> Self {
        Self {
            bin: bin.to_owned(),
            state: home.0.join(name).join("state"),
            run: home.0.join(name).join("run"),
            log: home.0.join(format!("{name}.log")),
            child: None,
        }
    }

    /// Starts the service and waits for it to listen.
    fn start(&mut self) {
        self.start_with(&[], &[]);
    }

    fn start_with(&mut self, env: &[(&str, &str)], args: &[&str]) {
        let mut cmd = self.command(&["service", "run"]);
        cmd.args(args).envs(env.iter().copied());
        self.start_command(cmd);
    }

    /// Starts the service in a mount namespace of its own, with a small
    /// tmpfs of `size` over each folder: a disk that really fills up, with
    /// no privileges needed. Only the service sees these disks; use
    /// [`Service::inside`] to look at them.
    fn start_on_small_disks(&mut self, disks: &[(&Path, &str)]) {
        let mut script = String::new();
        for (path, size) in disks {
            script.push_str(&format!(
                "mount -t tmpfs -o size={size},mode=0700 tmpfs {} && ",
                quote(path.to_str().unwrap())
            ));
        }
        script.push_str(&format!(
            "exec {} service run",
            quote(self.bin.to_str().unwrap())
        ));
        let mut cmd = Command::new("unshare");
        cmd.args(["-Urm", "sh", "-c", &script])
            .env("HYAKUME_STATE_DIR", &self.state)
            .env("HYAKUME_RUNTIME_DIR", &self.run)
            .env_remove("HYAKUME_LOG");
        self.start_command(cmd);
    }

    /// Where `path` is as the running service sees it.
    fn inside(&self, path: &Path) -> PathBuf {
        let pid = self.child.as_ref().unwrap().id();
        PathBuf::from(format!("/proc/{pid}/root{}", path.display()))
    }

    fn start_command(&mut self, mut cmd: Command) {
        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(
                std::fs::File::options()
                    .create(true)
                    .append(true)
                    .open(&self.log)
                    .unwrap(),
            )
            .spawn()
            .unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut line = Vec::new();
            let mut byte = [0u8; 1];
            while stdout.read(&mut byte).map(|n| n == 1).unwrap_or(false) {
                if byte[0] == b'\n' {
                    break;
                }
                line.push(byte[0]);
            }
            let _ = tx.send(String::from_utf8_lossy(&line).into_owned());
        });
        let line = rx.recv_timeout(DEADLINE).unwrap();
        assert!(line.starts_with("listening "), "{line} {}", self.log_text());
        self.child = Some(child);
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args)
            .env("HYAKUME_STATE_DIR", &self.state)
            .env("HYAKUME_RUNTIME_DIR", &self.run)
            .env_remove("HYAKUME_LOG")
            .env_remove("HYAKUME_CRASH_AT");
        cmd
    }

    /// Runs a command to its end with `input` on standard input.
    fn run(&self, args: &[&str], input: &[u8]) -> Output {
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let input = input.to_vec();
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
        let out = wait_output(child);
        writer.join().unwrap();
        out
    }

    /// Runs a command in the background; its output comes from the handle.
    fn spawn(&self, args: &[&str], input: &[u8]) -> std::thread::JoinHandle<Output> {
        let mut cmd = self.command(args);
        let input = input.to_vec();
        std::thread::spawn(move || {
            let mut child = cmd
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let mut stdin = child.stdin.take().unwrap();
            let _ = stdin.write_all(&input);
            drop(stdin);
            wait_output(child)
        })
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        let out = self.run(&all, b"");
        assert!(out.status.success(), "{args:?}: {}", text(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn init(&self, name: &str) {
        let out = self.run(
            &[
                "init",
                "--store",
                "key-file",
                "--name",
                name,
                "--passphrase-fd",
                "0",
            ],
            format!("{PASSPHRASE}\n").as_bytes(),
        );
        assert!(out.status.success(), "{}", text(&out));
    }

    fn unlock(&self) {
        let out = self.run(
            &["unlock", "--passphrase-fd", "0"],
            format!("{PASSPHRASE}\n").as_bytes(),
        );
        assert!(out.status.success(), "{}", text(&out));
    }

    fn route(&self) -> String {
        self.json(&["status"])["route"].as_str().unwrap().to_owned()
    }

    fn stop(&mut self) {
        let out = self.run(&["service", "stop"], b"");
        assert!(out.status.success(), "{}", text(&out));
        let child = self.child.take().unwrap();
        let out = wait_output(child);
        assert!(out.status.success(), "{}", self.log_text());
    }

    /// Waits for the process to end on its own, as after a crash point.
    fn wait_crash(&mut self) {
        let child = self.child.take().unwrap();
        let out = wait_output(child);
        assert!(
            !out.status.success(),
            "the service was expected to stop abruptly"
        );
    }

    /// Polls `status` until `check` holds.
    fn wait_status(&self, check: impl Fn(&Value) -> bool) -> Value {
        let start = Instant::now();
        loop {
            let status = self.json(&["status"]);
            if check(&status) {
                return status;
            }
            assert!(start.elapsed() < DEADLINE, "status never matched: {status}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn wait_output(mut child: Child) -> Output {
    let start = Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if start.elapsed() > DEADLINE {
            let _ = child.kill();
            let out = child.wait_with_output().unwrap();
            panic!("a command did not end in time: {}", text(&out));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Like [`wait_output`] for a child that leads its own process group: one
/// that overruns is killed with everything it started.
fn wait_group(mut child: Child) -> Output {
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if start.elapsed() > DEADLINE {
            let group = rustix::process::Pid::from_child(&child);
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
            let out = child.wait_with_output().unwrap();
            panic!("a command did not end in time: {}", text(&out));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

fn text(out: &Output) -> String {
    format!(
        "status {:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', r"'\''"))
}

/// A command running on a pseudo-terminal under `script`, as if a person
/// sat at it.
struct Tty {
    child: Child,
    stdin: ChildStdin,
    screen: Arc<(Mutex<Vec<u8>>, Condvar)>,
    /// Disconnects when the reader has taken everything the command wrote.
    read_all: std::sync::mpsc::Receiver<()>,
    /// How far `expect` has read.
    seen: usize,
}

impl Tty {
    fn spawn(service: &Service, args: &[&str]) -> Self {
        let bin = service.bin.to_str().unwrap();
        let command = std::iter::once(bin)
            .chain(args.iter().copied())
            .map(quote)
            .collect::<Vec<_>>()
            .join(" ");
        // Echo stays on, as on a real terminal, so anything a command does
        // not hide is shown.
        let mut child = Command::new("script")
            .args([
                "-q",
                "-e",
                "-f",
                "-E",
                "always",
                "-c",
                &command,
                "/dev/null",
            ])
            .env("HYAKUME_STATE_DIR", &service.state)
            .env("HYAKUME_RUNTIME_DIR", &service.run)
            .env_remove("HYAKUME_LOG")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let screen = Arc::new((Mutex::new(Vec::new()), Condvar::new()));
        let shared = screen.clone();
        let (reading, read_all) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            let _reading = reading;
            let mut buf = [0u8; 4096];
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let (lock, cond) = &*shared;
                lock.lock().unwrap().extend_from_slice(&buf[..n]);
                cond.notify_all();
            }
        });
        Self {
            child,
            stdin,
            screen,
            read_all,
            seen: 0,
        }
    }

    fn screen(&self) -> String {
        String::from_utf8_lossy(&self.screen.0.lock().unwrap()).into_owned()
    }

    /// Waits until `needle` appears after what was already read, and
    /// returns the screen up to and including it.
    fn expect(&mut self, needle: &str) -> String {
        let (lock, cond) = &*self.screen;
        let start = Instant::now();
        let mut screen = lock.lock().unwrap();
        loop {
            let text = String::from_utf8_lossy(&screen[self.seen..]).into_owned();
            if let Some(at) = text.find(needle) {
                let upto = String::from_utf8_lossy(&screen[..self.seen]).len() + at + needle.len();
                let shown = String::from_utf8_lossy(&screen[..upto.min(screen.len())]).into_owned();
                self.seen = upto.min(screen.len());
                return shown;
            }
            let left = DEADLINE
                .checked_sub(start.elapsed())
                .unwrap_or_else(|| panic!("never saw {needle:?}; the screen was:\n{text}"));
            screen = cond.wait_timeout(screen, left).unwrap().0;
        }
    }

    fn type_line(&mut self, line: &str) {
        self.stdin.write_all(line.as_bytes()).unwrap();
        self.stdin.write_all(b"\n").unwrap();
        self.stdin.flush().unwrap();
    }

    /// Waits for the command to end; returns its exit code and screen.
    fn finish(mut self) -> (Option<i32>, String) {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                // The output ends with the command; wait for the reader.
                let _ = self.read_all.recv_timeout(DEADLINE);
                return (status.code(), self.screen());
            }
            assert!(
                start.elapsed() < DEADLINE,
                "the command did not end: {}",
                self.screen()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Ends the command as Ctrl-C would.
    fn interrupt(mut self) -> (Option<i32>, String) {
        let _ = self.stdin.write_all(&[3]);
        let _ = self.stdin.flush();
        self.finish()
    }
}

impl Drop for Tty {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Two initialized, unlocked services, paired through the public CLI.
struct Pair {
    home: Home,
    a: Service,
    b: Service,
}

fn started(home: &Home, name: &str, bin: &Path) -> Service {
    let mut service = Service::with_bin(home, name, bin);
    service.start();
    service.init(name);
    service
}

/// The text after `label` up to the end of its line.
fn after<'a>(screen: &'a str, label: &str) -> &'a str {
    let at = screen.rfind(label).unwrap() + label.len();
    screen[at..].trim_start().lines().next().unwrap().trim()
}

fn pair() -> Pair {
    pair_using(Home::new(), Path::new(BIN))
}

fn pair_using(home: Home, bin: &Path) -> Pair {
    let a = started(&home, "alpha", bin);
    let b = started(&home, "beta", bin);
    pair_services(&a, &b);
    Pair { home, a, b }
}

/// Pairs two running, unlocked services through the CLI: a invites b.
fn pair_services(a: &Service, b: &Service) {
    let mut invite = Tty::spawn(a, &["pair", "--invite", "--alias", "beta"]);
    let shown = invite.expect("Waiting for it to be used");
    let token = after(&shown, "Give this invitation to the other device:").to_owned();
    assert!(token.starts_with("hyakume://invite#"), "{shown}");
    let mut join = Tty::spawn(b, &["pair", "--alias", "alpha"]);
    join.expect("Invitation from the other device:");
    join.type_line(&token);
    // Both people read their words before answering.
    let on_a = invite.expect("Do the words match? [y/N]");
    let on_b = join.expect("Do the words match? [y/N]");
    let words = |screen: &str| after(screen, "Both screens must show the same words:").to_owned();
    assert_eq!(words(&on_a), words(&on_b));
    assert_eq!(words(&on_a).split(' ').count(), 4);
    invite.type_line("y");
    join.type_line("y");
    let (code_a, screen_a) = invite.finish();
    let (code_b, screen_b) = join.finish();
    assert_eq!(code_a, Some(0), "{screen_a}");
    assert_eq!(code_b, Some(0), "{screen_b}");
    assert!(screen_a.contains("Paired with beta"), "{screen_a}");
    assert!(screen_b.contains("Paired with alpha"), "{screen_b}");
    // The invitation itself never reached the joining side's screen.
    assert!(!screen_b.contains(&token[20..]), "{screen_b}");
}

/// Lists a folder's names, partial files included.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// Offers from `a` to `b`, answers on `rx`, and returns the sender's output.
fn offer(p: &Pair, rx: &mut Tty, args: &[&str], input: &[u8], accept: bool) -> (Output, String) {
    let route = p.b.route();
    let mut all = vec!["send", "beta", "--at", &route];
    all.extend_from_slice(args);
    let send = p.a.spawn(&all, input);
    let screen = rx.expect(QUESTION);
    rx.type_line(if accept { "y" } else { "n" });
    let out = send.join().unwrap();
    (out, screen)
}

#[test]
fn two_services_pair_and_exchange_every_kind_of_payload_through_the_cli() {
    let p = pair();
    let inbox = p.home.folder("inbox");
    let outbox = p.home.folder("outbox");
    let mut rx = Tty::spawn(&p.b, &["receive", "--dir", inbox.to_str().unwrap()]);
    rx.expect("Receiving into");

    std::fs::write(outbox.join("notes.txt"), b"hello beta").unwrap();
    let (out, screen) = offer(
        &p,
        &mut rx,
        &[outbox.join("notes.txt").to_str().unwrap()],
        b"",
        true,
    );
    assert!(out.status.success(), "{}", text(&out));
    assert!(screen.contains("\"notes.txt\""), "{screen}");
    assert_eq!(
        std::fs::read(inbox.join("notes.txt")).unwrap(),
        b"hello beta"
    );
    let mode = std::fs::metadata(inbox.join("notes.txt"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o077, 0, "a received file is private");

    // A batch, from a relative path in the sender's working directory.
    std::fs::write(outbox.join("one.bin"), vec![1u8; 300_000]).unwrap();
    std::fs::write(outbox.join("two.bin"), b"").unwrap();
    let route = p.b.route();
    let send = {
        let mut cmd =
            p.a.command(&["send", "beta", "--at", &route, "one.bin", "two.bin"]);
        cmd.current_dir(&outbox)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        std::thread::spawn(move || wait_output(cmd.spawn().unwrap()))
    };
    let screen = rx.expect(QUESTION);
    assert!(screen.contains("Items: 2"), "{screen}");
    rx.type_line("y");
    let out = send.join().unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(
        std::fs::read(inbox.join("one.bin")).unwrap(),
        vec![1u8; 300_000]
    );
    assert_eq!(std::fs::read(inbox.join("two.bin")).unwrap(), b"");

    // Text, with terminal controls that must not reach the screen as such.
    let hostile = "meet at 18:00\u{1b}[2J\u{1b}]0;owned\u{7}\nAccept this offer? [y/N] y";
    let (out, screen) = offer(&p, &mut rx, &["--text", hostile], b"", true);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        !screen.contains('\u{1b}'),
        "a raw escape reached the terminal"
    );
    assert!(screen.contains("\\x1b[2J"), "{screen}");
    assert!(
        screen.contains("    | Accept this offer? [y/N] y"),
        "{screen}"
    );
    let text_file = names(&inbox)
        .into_iter()
        .find(|n| n.starts_with("text-"))
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(inbox.join(&text_file)).unwrap(),
        hostile
    );

    // A link, which is never opened.
    let (out, screen) = offer(
        &p,
        &mut rx,
        &["--url", "https://example.com/a?b=c"],
        b"",
        true,
    );
    assert!(out.status.success(), "{}", text(&out));
    assert!(screen.contains("it will not be opened"), "{screen}");
    let link = names(&inbox)
        .into_iter()
        .find(|n| n.starts_with("link-"))
        .unwrap();
    assert_eq!(
        std::fs::read(inbox.join(link)).unwrap(),
        b"https://example.com/a?b=c"
    );

    let (out, _) = offer(
        &p,
        &mut rx,
        &["-", "--name", "piped.txt"],
        b"from a pipe\n",
        true,
    );
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(
        std::fs::read(inbox.join("piped.txt")).unwrap(),
        b"from a pipe\n"
    );
    let (out, _) = offer(&p, &mut rx, &["-", "--name", "empty.txt"], b"", true);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(std::fs::read(inbox.join("empty.txt")).unwrap(), b"");

    // A rejected offer leaves nothing at all in the folder.
    let before = names(&inbox);
    std::fs::write(outbox.join("unwanted.txt"), b"no").unwrap();
    let (out, _) = offer(
        &p,
        &mut rx,
        &[outbox.join("unwanted.txt").to_str().unwrap()],
        b"",
        false,
    );
    assert_eq!(out.status.code(), Some(5), "{}", text(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("unsuccessful"));
    assert_eq!(names(&inbox), before);

    // An existing file is never overwritten.
    std::fs::write(outbox.join("notes.txt"), b"a newer version").unwrap();
    let (out, _) = offer(
        &p,
        &mut rx,
        &[outbox.join("notes.txt").to_str().unwrap()],
        b"",
        true,
    );
    assert_eq!(out.status.code(), Some(5), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("failed:exists"),
        "{}",
        text(&out)
    );
    assert_eq!(
        std::fs::read(inbox.join("notes.txt")).unwrap(),
        b"hello beta"
    );

    // Inspecting the last transfer shows its item and why it failed.
    let transfers = p.a.json(&["transfers"]);
    let last = transfers["transfers"][0]["transfer"]
        .as_str()
        .unwrap()
        .to_owned();
    let inspected = p.a.json(&["inspect", &last[..8]]);
    assert_eq!(inspected["items"][0]["state"], "failed:exists");

    // Removing the device ends its authority on both sides of a send.
    let (_, _) = rx.interrupt();
    let out = p.b.run(&["remove", "alpha"], b"");
    assert!(out.status.success(), "{}", text(&out));
    let devices = p.b.json(&["devices"]);
    assert_eq!(devices["devices"][0]["trusted"], false);
    let route = p.b.route();
    let out = p.a.run(
        &["send", "beta", "--at", &route, "--text", "still there?"],
        b"",
    );
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    let out =
        p.b.run(&["send", "alpha", "--at", &p.a.route(), "--text", "x"], b"");
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
}

#[test]
fn a_second_service_for_the_same_state_is_refused() {
    let home = Home::new();
    let mut first = Service::new(&home, "alpha");
    first.start();
    let out = first.run(&["service", "run"], b"");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("another Hyakume service already uses"),
        "{}",
        text(&out)
    );
    // The first one is unharmed.
    assert_eq!(first.json(&["status"])["state"], "uninitialized");
    first.stop();
}

#[test]
fn a_locked_service_keeps_its_key_on_disk_encrypted_and_refuses_work_until_unlocked() {
    let home = Home::new();
    let mut a = Service::new(&home, "alpha");
    a.start();
    // Typed at a terminal, the passphrase is never shown.
    let mut init = Tty::spawn(&a, &["init", "--store", "key-file", "--name", "alpha"]);
    init.expect("Passphrase: ");
    init.type_line(PASSPHRASE);
    init.expect("Passphrase again: ");
    init.type_line(PASSPHRASE);
    let (code, screen) = init.finish();
    assert_eq!(code, Some(0), "{screen}");
    assert!(
        !screen.contains("horse"),
        "the passphrase was echoed: {screen}"
    );
    let fingerprint = a.json(&["status"])["fingerprint"]
        .as_str()
        .unwrap()
        .to_owned();

    // Nothing in the state directory holds the key in the clear: the key
    // file is age-encrypted, and the database has no secret column.
    let key = std::fs::read(a.state.join("identity.age")).unwrap();
    assert!(key.starts_with(b"age-encryption.org/v1"));
    let db = std::fs::read(a.state.join("hyakume.db")).unwrap();
    assert!(
        !db.windows(PASSPHRASE.len())
            .any(|w| w == PASSPHRASE.as_bytes())
    );

    // Restarted, the service is locked: no route, and work is refused.
    a.stop();
    a.start();
    let status = a.json(&["status"]);
    assert_eq!(status["state"], "locked");
    assert!(status["route"].is_null());
    let out = a.run(
        &["send", "anyone", "--at", "hyakume-route:x", "--text", "hi"],
        b"",
    );
    assert_eq!(out.status.code(), Some(4), "{}", text(&out));
    let out = a.run(
        &["unlock", "--passphrase-fd", "0"],
        b"wrong horse battery\n",
    );
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert_eq!(a.json(&["status"])["state"], "locked");
    a.unlock();
    let status = a.json(&["status"]);
    assert_eq!(status["state"], "unlocked");
    assert_eq!(status["fingerprint"], fingerprint.as_str());
    a.stop();
}

#[test]
fn an_identity_whose_endpoint_cannot_start_stays_locked_and_is_never_replaced() {
    let home = Home::new();
    let mut a = Service::new(&home, "alpha");
    // Another program holds the port the endpoint needs.
    let taken = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().to_string();
    a.start_with(&[], &["--listen", &port]);
    let init = [
        "init",
        "--store",
        "key-file",
        "--name",
        "alpha",
        "--passphrase-fd",
        "0",
    ];
    let passphrase = format!("{PASSPHRASE}\n");
    let out = a.run(&init, passphrase.as_bytes());
    assert!(!out.status.success(), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("the identity is saved"),
        "{}",
        text(&out)
    );
    let status = a.json(&["status"]);
    assert_eq!(status["state"], "locked", "{status}");
    // A second init keeps the saved identity.
    let again = a.run(&init, passphrase.as_bytes());
    assert!(
        String::from_utf8_lossy(&again.stderr).contains("already has an identity"),
        "{}",
        text(&again)
    );
    drop(taken);
    a.unlock();
    let unlocked = a.json(&["status"]);
    assert_eq!(unlocked["state"], "unlocked");
    assert_eq!(unlocked["fingerprint"], status["fingerprint"]);
    a.stop();
}

#[test]
fn an_unresponsive_secret_service_is_refused_quickly_instead_of_hanging() {
    let home = Home::new();
    // A bus socket that accepts connections and never answers.
    let socket = home.0.join("bus");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for conn in listener.incoming().flatten() {
            held.push(conn);
        }
    });
    let mut a = Service::new(&home, "alpha");
    let bus = format!("unix:path={}", socket.display());
    a.start_with(&[("DBUS_SESSION_BUS_ADDRESS", &bus)], &[]);
    let start = Instant::now();
    let out = a.run(
        &["init", "--store", "secret-service", "--name", "alpha"],
        b"",
    );
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("Secret Service"),
        "{}",
        text(&out)
    );
    assert!(
        start.elapsed() < Duration::from_secs(15),
        "took {:?}",
        start.elapsed()
    );
    assert_eq!(a.json(&["status"])["state"], "uninitialized");
    assert!(!a.state.join("identity.age").exists());
    a.stop();
}

#[test]
fn a_crash_after_the_rename_is_recovered_as_published() {
    let p = pair();
    let inbox = p.home.folder("inbox");
    let mut b = p.b;
    b.stop();
    b.start_with(&[("HYAKUME_CRASH_AT", "published-unrecorded")], &[]);
    b.unlock();
    let mut rx = Tty::spawn(&b, &["receive", "--dir", inbox.to_str().unwrap()]);
    rx.expect("Receiving into");
    let route = b.route();
    let file = p.home.folder("outbox").join("report.pdf");
    std::fs::write(&file, vec![9u8; 100_000]).unwrap();
    let send = p.a.spawn(
        &["send", "beta", "--at", &route, file.to_str().unwrap()],
        b"",
    );
    rx.expect(QUESTION);
    rx.type_line("y");
    // b stops right after the rename, before recording it.
    b.wait_crash();
    let out = send.join().unwrap();
    assert_ne!(out.status.code(), Some(0), "{}", text(&out));
    drop(rx);

    b.start();
    let transfers = b.json(&["transfers"]);
    let id = transfers["transfers"][0]["transfer"]
        .as_str()
        .unwrap()
        .to_owned();
    let inspected = b.json(&["inspect", &id]);
    // The file under its final name is the one Hyakume wrote, so the
    // transfer is recorded as complete, and nothing stays charged.
    assert_eq!(inspected["items"][0]["state"], "published", "{inspected}");
    assert_eq!(inspected["outcome"], "completed");
    assert_eq!(
        std::fs::read(inbox.join("report.pdf")).unwrap(),
        vec![9u8; 100_000]
    );
    assert_eq!(names(&inbox), ["report.pdf"]);
    assert_eq!(b.json(&["status"])["partial_bytes"], 0);
}

#[test]
fn a_crash_before_the_rename_keeps_the_partial_private_until_its_work_is_cancelled() {
    let p = pair();
    let inbox = p.home.folder("inbox");
    // A file with the same content already exists under the final name: it
    // must neither count as the received item nor be touched.
    std::fs::write(inbox.join("same.bin"), vec![4u8; 50_000]).unwrap();
    let mut b = p.b;
    b.stop();
    b.start_with(&[("HYAKUME_CRASH_AT", "before-rename")], &[]);
    b.unlock();
    let mut rx = Tty::spawn(&b, &["receive", "--dir", inbox.to_str().unwrap()]);
    rx.expect("Receiving into");
    let route = b.route();
    let file = p.home.folder("outbox").join("same.bin");
    std::fs::write(&file, vec![4u8; 50_000]).unwrap();
    let send = p.a.spawn(
        &["send", "beta", "--at", &route, file.to_str().unwrap()],
        b"",
    );
    rx.expect(QUESTION);
    rx.type_line("y");
    b.wait_crash();
    send.join().unwrap();
    drop(rx);

    b.start();
    let id = b.json(&["transfers"])["transfers"][0]["transfer"]
        .as_str()
        .unwrap()
        .to_owned();
    let inspected = b.json(&["inspect", &id]);
    // Paused for a later resume; the identical existing file proves nothing.
    assert_eq!(inspected["items"][0]["state"], "paused", "{inspected}");
    assert_eq!(inspected["retained_bytes"], 50_000);
    let partial = names(&inbox)
        .into_iter()
        .find(|n| n.starts_with(".hyakume-"))
        .unwrap();
    let mode = std::fs::metadata(inbox.join(&partial))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);

    // Cancelling ends the paused work; the partial file goes and its bytes
    // are released only once it is gone.
    let out = b.run(&["cancel", &id], b"");
    assert_eq!(out.status.code(), Some(5), "{}", text(&out));
    b.wait_status(|s| s["partial_bytes"] == 0);
    assert_eq!(names(&inbox), ["same.bin"]);
    b.stop();
}

#[test]
fn a_sender_crash_keeps_the_sealed_spool_of_its_paused_transfer_across_restart() {
    let p = pair();
    let inbox = p.home.folder("inbox");
    let mut rx = Tty::spawn(&p.b, &["receive", "--dir", inbox.to_str().unwrap()]);
    rx.expect("Receiving into");
    let mut a = p.a;
    a.stop();
    a.start_with(&[("HYAKUME_CRASH_AT", "sending")], &[]);
    a.unlock();
    let route = p.b.route();
    let piped = vec![7u8; 200_000];
    let send = a.spawn(&["send", "beta", "--at", &route, "-"], &piped);
    rx.expect(QUESTION);
    rx.type_line("y");
    // a stops as it starts sending the first chunk.
    a.wait_crash();
    send.join().unwrap();

    a.start();
    a.unlock();
    let id = a.json(&["transfers"])["transfers"][0]["transfer"]
        .as_str()
        .unwrap()
        .to_owned();
    let inspected = a.json(&["inspect", &id]);
    assert_eq!(inspected["items"][0]["state"], "paused", "{inspected}");
    assert_eq!(inspected["retained_bytes"], 200_000);
    // The spool is still the sealed, read-only file with the same bytes.
    let spools = names(&a.state.join("spool"));
    assert_eq!(spools.len(), 1);
    let spool = a.state.join("spool").join(&spools[0]);
    assert_eq!(
        std::fs::metadata(&spool).unwrap().permissions().mode() & 0o777,
        0o400
    );
    assert_eq!(std::fs::read(&spool).unwrap(), piped);
    assert_eq!(a.json(&["status"])["paused"], 1);

    // Cancelling releases the spool.
    a.run(&["cancel", &id], b"");
    a.wait_status(|s| s["spool_bytes"] == 0);
    assert!(names(&a.state.join("spool")).is_empty());
    a.stop();
}

#[test]
fn standard_input_that_is_too_large_or_too_slow_offers_nothing() {
    let p = pair();
    let mut a = p.a;
    a.stop();
    a.start_with(&[], &["--stdin-deadline", "1"]);
    a.unlock();
    let route = p.b.route();
    // A link larger than its limit.
    let long = format!("https://example.com/{}", "a".repeat(9000));
    let out = a.run(&["send", "beta", "--at", &route, "--url", &long], b"");
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("nothing was offered"),
        "{}",
        text(&out)
    );

    // Input that does not end within the deadline: the command keeps its
    // standard input open and silent.
    let mut slow = a
        .command(&["send", "beta", "--at", &route, "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut held = slow.stdin.take().unwrap();
    held.write_all(b"some of the input").unwrap();
    let out = wait_output(slow);
    drop(held);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("did not end in time"),
        "{}",
        text(&out)
    );

    // Input cut off by the client going away.
    let mut cut = a
        .command(&["send", "beta", "--at", &route, "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    cut.stdin
        .as_mut()
        .unwrap()
        .write_all(&[1u8; 100_000])
        .unwrap();
    cut.kill().unwrap();
    let _ = cut.wait();

    // Text that is not UTF-8, which only a client other than this command
    // can send.
    let mut s = raw(&a);
    s.write_all(&frame(1, br#"{"type":"hello","version":1}"#))
        .unwrap();
    read_reply(&mut s);
    let request = serde_json::json!({"type": "request", "id": 1, "request":
        {"op": "send-stream", "device": "beta", "route": route, "kind": "text", "name": null}});
    s.write_all(&frame(1, request.to_string().as_bytes()))
        .unwrap();
    s.write_all(&frame(2, b"a\xffb")).unwrap();
    s.write_all(&frame(1, br#"{"type":"stream-end"}"#)).unwrap();
    let refused = read_reply(&mut s).unwrap();
    assert_eq!(refused["reply"]["failure"]["code"], "invalid", "{refused}");

    // None of these produced an offer or kept a spool.
    assert!(
        p.b.json(&["transfers"])["transfers"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    a.wait_status(|s| s["spool_bytes"] == 0);
    assert!(names(&a.state.join("spool")).is_empty());
    a.stop();
}

#[test]
fn unfinished_input_that_cannot_be_deleted_stays_charged_until_cleanup_deletes_it() {
    let p = pair();
    let route = p.b.route();
    let spool = p.a.state.join("spool");
    let mut s = raw(&p.a);
    s.write_all(&frame(1, br#"{"type":"hello","version":1}"#))
        .unwrap();
    read_reply(&mut s);
    let request = serde_json::json!({"type": "request", "id": 1, "request":
        {"op": "send-stream", "device": "beta", "route": route, "kind": "stdin", "name": null}});
    s.write_all(&frame(1, request.to_string().as_bytes()))
        .unwrap();
    s.write_all(&frame(2, &[5u8; 1024])).unwrap();
    let start = Instant::now();
    while !names(&spool)
        .first()
        .is_some_and(|n| std::fs::metadata(spool.join(n)).unwrap().len() == 1024)
    {
        assert!(
            start.elapsed() < DEADLINE,
            "the input never reached its spool"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // The client leaves while nothing can be removed from the spool folder.
    std::fs::set_permissions(&spool, std::fs::Permissions::from_mode(0o500)).unwrap();
    drop(s);
    let status = p.a.wait_status(|s| s["spool_bytes"] == 1024);
    std::fs::set_permissions(&spool, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(names(&spool).len(), 1, "{status}");

    // Cleanup retries it once the folder allows it, and only then frees it.
    p.a.wait_status(|s| s["spool_bytes"] == 0);
    assert!(names(&spool).is_empty());
}

#[test]
fn a_state_folder_under_a_linked_ancestor_still_cleans_up_its_spools() {
    let home = Home::new();
    let real = home.folder("real");
    std::os::unix::fs::symlink(&real, home.0.join("link")).unwrap();
    let mut a = Service {
        bin: PathBuf::from(BIN),
        state: home.0.join("link/alpha/state"),
        run: home.0.join("link/alpha/run"),
        log: home.0.join("alpha.log"),
        child: None,
    };
    a.start();
    a.init("alpha");
    let mut b = started(&home, "beta", Path::new(BIN));
    pair_services(&a, &b);

    let inbox = home.folder("inbox");
    let mut rx = Tty::spawn(&b, &["receive", "--dir", inbox.to_str().unwrap()]);
    rx.expect("Receiving into");
    let route = b.route();
    let send = a.spawn(&["send", "beta", "--at", &route, "--text", "hello"], b"");
    rx.expect(QUESTION);
    rx.type_line("y");
    let out = send.join().unwrap();
    assert!(out.status.success(), "{}", text(&out));

    // The finished transfer's spool goes, through the real path.
    a.wait_status(|s| s["spool_bytes"] == 0);
    assert!(names(&real.join("alpha/state/spool")).is_empty());
    drop(rx);
    a.stop();
    b.stop();
}

#[test]
fn a_sealed_spool_the_ledger_cannot_record_is_deleted_at_once() {
    let mut p = pair();
    let route = p.b.route();
    // The database cannot write its journal; the spool folder still works.
    let private = std::fs::Permissions::from_mode(0o700);
    std::fs::set_permissions(&p.a.state, std::fs::Permissions::from_mode(0o500)).unwrap();
    let out =
        p.a.run(&["send", "beta", "--at", &route, "--text", "hello"], b"");
    std::fs::set_permissions(&p.a.state, private).unwrap();
    assert!(!out.status.success(), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("nothing was offered"),
        "{}",
        text(&out)
    );
    assert!(names(&p.a.state.join("spool")).is_empty());
    let status = p.a.json(&["status"]);
    assert_eq!(status["spool_bytes"], 0);
    // The failed write is visible, and new work waits for a restart.
    assert_eq!(status["store_failed"], true, "{status}");
    p.a.stop();
    p.b.stop();
}

#[test]
fn an_unusable_destination_is_refused_before_any_offer_is_shown() {
    let home = Home::new();
    let a = started(&home, "alpha", Path::new(BIN));
    let real = home.folder("real");
    let link = home.0.join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let shared = home.folder("shared");
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
    // The service's own directory in /proc belongs to this user, on a
    // filesystem nothing is published into.
    let proc_dir = format!("/proc/{}", a.child.as_ref().unwrap().id());
    for (dir, why) in [
        (link.to_str().unwrap(), "symbolic link"),
        (shared.to_str().unwrap(), "other users can write"),
        (proc_dir.as_str(), "not one Hyakume can publish into"),
    ] {
        let mut rx = Tty::spawn(&a, &["receive", "--dir", dir]);
        let screen = rx.expect("Receive into Hyakume's own folder instead");
        assert!(screen.contains(why), "{screen}");
        rx.type_line("n");
        let (code, _) = rx.finish();
        assert_eq!(code, Some(1));
    }
    // Without a terminal nobody could answer offers, so it is refused.
    // setsid runs it with no controlling terminal at all.
    let out = wait_output(
        Command::new("setsid")
            .args(["-w", BIN, "receive"])
            .env("HYAKUME_STATE_DIR", &a.state)
            .env("HYAKUME_RUNTIME_DIR", &a.run)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
}

/// Speaks the IPC protocol directly.
fn raw(service: &Service) -> std::os::unix::net::UnixStream {
    let stream = std::os::unix::net::UnixStream::connect(service.run.join("hyakume.sock")).unwrap();
    stream.set_read_timeout(Some(DEADLINE)).unwrap();
    stream
}

fn frame(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut out = (body.len() as u32).to_be_bytes().to_vec();
    out.push(kind);
    out.extend_from_slice(body);
    out
}

fn read_reply(stream: &mut std::os::unix::net::UnixStream) -> Option<Value> {
    let mut header = [0u8; 5];
    stream.read_exact(&mut header).ok()?;
    let len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

#[test]
fn the_local_socket_refuses_bad_frames_stale_answers_and_extra_clients() {
    let home = Home::new();
    let mut a = Service::new(&home, "alpha");
    a.start();
    let socket = a.run.join("hyakume.sock");
    assert_eq!(
        std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let hello = frame(1, br#"{"type":"hello","version":1}"#);

    // A frame claiming more than the limit is refused before it is read.
    let mut s = raw(&a);
    s.write_all(&hello).unwrap();
    assert_eq!(read_reply(&mut s).unwrap()["type"], "hello");
    s.write_all(&((2u32 << 20).to_be_bytes())).unwrap();
    s.write_all(&[1]).unwrap();
    assert!(read_reply(&mut s).is_none(), "the connection stayed open");

    // A message that is not part of the protocol ends the connection.
    let mut s = raw(&a);
    s.write_all(&hello).unwrap();
    read_reply(&mut s);
    s.write_all(&frame(
        1,
        br#"{"type":"request","id":1,"request":{"op":"format"}}"#,
    ))
    .unwrap();
    assert!(read_reply(&mut s).is_none());

    // A request the service cannot serve yet gets its refusal, and the
    // connection stays usable.
    let mut s = raw(&a);
    s.write_all(&hello).unwrap();
    read_reply(&mut s);
    s.write_all(&frame(
        1,
        br#"{"type":"request","id":1,"request":{"op":"unlock"}}"#,
    ))
    .unwrap();
    let refused = read_reply(&mut s).unwrap();
    assert_eq!(
        refused["reply"]["failure"]["code"], "not-initialized",
        "{refused}"
    );
    s.write_all(&frame(
        1,
        br#"{"type":"request","id":2,"request":{"op":"status"}}"#,
    ))
    .unwrap();
    assert_eq!(
        read_reply(&mut s).unwrap()["reply"]["body"]["state"],
        "uninitialized"
    );

    // An answer naming a prompt this connection never received approves
    // nothing and is refused.
    let mut s = raw(&a);
    s.write_all(&hello).unwrap();
    read_reply(&mut s);
    s.write_all(&frame(
        1,
        br#"{"type":"answer","prompt":7,"context":"00","accept":true}"#,
    ))
    .unwrap();
    let refused = read_reply(&mut s).unwrap();
    assert_eq!(refused["type"], "answer-refused");

    // Past the client limit, a new client is told the service is busy.
    let mut held: Vec<_> = (0..7).map(|_| raw(&a)).collect();
    held.push(s);
    let mut extra = raw(&a);
    let busy = read_reply(&mut extra).unwrap();
    assert_eq!(busy["reply"]["failure"]["code"], "busy", "{busy}");
    drop(held);
    // The service still answers once clients leave.
    let start = Instant::now();
    loop {
        let out = a.run(&["status"], b"");
        if out.status.success() {
            break;
        }
        assert!(start.elapsed() < DEADLINE, "{}", text(&out));
        std::thread::sleep(Duration::from_millis(100));
    }
    a.stop();
}

#[test]
fn the_stop_command_is_always_told_the_service_stopped() {
    let home = Home::new();
    let mut a = Service::new(&home, "alpha");
    // Shutdown raced the reply to `stop` and lost about once in a hundred.
    for _ in 0..200 {
        a.start();
        a.stop();
    }
}

#[test]
fn a_client_that_stops_reading_gives_its_place_back() {
    let home = Home::new();
    let mut a = Service::new(&home, "alpha");
    a.start();
    let hello = frame(1, br#"{"type":"hello","version":1}"#);

    // This client starts an init that waits for a passphrase, then asks
    // again and again without ever reading the busy replies.
    let mut flood = raw(&a);
    flood.write_all(&hello).unwrap();
    read_reply(&mut flood);
    let init =
        br#"{"type":"request","id":1,"request":{"op":"init","store":"key-file","name":"a"}}"#;
    flood.write_all(&frame(1, init)).unwrap();
    flood
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let status = frame(1, br#"{"type":"request","id":2,"request":{"op":"status"}}"#);
    let spammer = std::thread::spawn(move || {
        while flood.write_all(&status).is_ok() {}
        flood
    });

    // Seven well-behaved clients hold every other place.
    let held: Vec<_> = (0..7)
        .map(|_| {
            let mut s = raw(&a);
            s.write_all(&hello).unwrap();
            assert_eq!(read_reply(&mut s).unwrap()["type"], "hello");
            s
        })
        .collect();

    // The flooding client is dropped, so a new one gets its place.
    let start = Instant::now();
    loop {
        let mut s = raw(&a);
        s.write_all(&hello).unwrap();
        if read_reply(&mut s).is_some_and(|r| r["type"] == "hello") {
            break;
        }
        assert!(
            start.elapsed() < DEADLINE,
            "the stalled client kept its place"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    drop(held);
    drop(spammer.join().unwrap());
    a.stop();
}

/// The Secret Service store in an isolated session bus with its own
/// keyring. Needs `dbus-run-session` and `gnome-keyring-daemon`, so it runs
/// in its own task: `mise run test-keyring`.
#[test]
#[ignore = "needs dbus-run-session and gnome-keyring-daemon; run with mise run test-keyring"]
fn a_secret_service_key_unlocks_at_start_and_is_deleted_on_reset() {
    let home = Home::new();
    let a = Service::new(&home, "alpha");
    let data = home.folder("data");
    let script = format!(
        r#"set -e
export XDG_DATA_HOME={data}
svc=
# Stops a service still running when the script ends, keeping its status.
trap 'status=$?; kill $svc 2>/dev/null || true; exit $status' EXIT
# Waits at most 10 s for the service to answer.
ready() {{
    n=0
    until {bin} status >/dev/null 2>&1; do
        n=$((n + 1))
        if [ $n -gt 100 ]; then echo "the service never answered" >&2; exit 1; fi
        sleep 0.1
    done
}}
echo -n keyring-test | gnome-keyring-daemon --unlock --components=secrets >/dev/null
{bin} service run & svc=$!
ready
{bin} init --store secret-service --name alpha
{bin} service stop; wait $svc
DBUS_SESSION_BUS_ADDRESS=unix:path={home}/no-bus {bin} service run & svc=$!
ready
if {bin} reset --yes 2> {home}/refused.txt; then echo "reset went ahead" > {home}/refused.txt; fi
{bin} --json status > {home}/unreachable.json
{bin} service stop; wait $svc
{bin} service run & svc=$!
ready
{bin} --json status > {home}/status.json
{bin} reset --yes
secret-tool search --all application hyakume > {home}/left.txt 2>&1 || true
{bin} service stop; wait $svc
"#,
        data = data.display(),
        bin = BIN,
        home = home.0.display(),
    );
    let session = Command::new("dbus-run-session")
        .args(["--", "sh", "-c", &script])
        .env("HYAKUME_STATE_DIR", &a.state)
        .env("HYAKUME_RUNTIME_DIR", &a.run)
        .env_remove("HYAKUME_LOG")
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let out = wait_group(session);
    assert!(out.status.success(), "{}", text(&out));
    let read = |name: &str| -> Value {
        serde_json::from_slice(&std::fs::read(home.0.join(name)).unwrap()).unwrap()
    };
    // With the keyring out of reach, reset refuses rather than forget the
    // installation that finds the old key.
    let refused = std::fs::read_to_string(home.0.join("refused.txt")).unwrap();
    assert!(refused.contains("could not be deleted"), "{refused}");
    assert_eq!(read("unreachable.json")["state"], "locked");
    let status = read("status.json");
    // The key came from the keyring at start, with nobody typing anything.
    assert_eq!(status["state"], "unlocked");
    assert_eq!(status["store"], "secret-service");
    assert!(!a.state.join("identity.age").exists());
    let left = std::fs::read_to_string(home.0.join("left.txt")).unwrap();
    assert!(
        !left.contains("hyakume"),
        "the key stayed in the keyring: {left}"
    );
}

/// Real full disks: a receiving folder and a sender's spool directory on
/// small filesystems. Needs unprivileged user namespaces, so it runs in its
/// own task: `mise run test-full-disk`.
#[test]
#[ignore = "needs unprivileged user namespaces; run with mise run test-full-disk"]
fn a_full_disk_refuses_offers_and_input_and_leaves_nothing_behind() {
    let home = Home::new();
    let bin = Path::new(BIN);
    let mut a = Service::with_bin(&home, "alpha", bin);
    let spool = a.state.join("spool");
    std::fs::create_dir_all(&spool).unwrap();
    for dir in [&a.state, &spool] {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    a.start_on_small_disks(&[(&spool, "70m")]);
    a.init("alpha");
    let mut b = Service::with_bin(&home, "beta", bin);
    let inbox = home.folder("inbox");
    b.start_on_small_disks(&[(&inbox, "16m")]);
    b.init("beta");
    pair_services(&a, &b);

    // An offer that would leave less than the safety margin free is refused
    // before anyone is asked, and nothing is written.
    let mut rx = Tty::spawn(&b, &["receive", "--dir", inbox.to_str().unwrap()]);
    rx.expect("Receiving into");
    let file = home.folder("outbox").join("big.bin");
    std::fs::write(&file, vec![3u8; 1024 * 1024]).unwrap();
    let route = b.route();
    let out = a.run(
        &[
            "--json",
            "send",
            "beta",
            "--at",
            &route,
            file.to_str().unwrap(),
        ],
        b"",
    );
    assert_eq!(out.status.code(), Some(5), "{}", text(&out));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["offer"], "rejected:too-large", "{report}");
    assert!(!rx.screen().contains("Accept this offer"));
    assert!(names(&b.inside(&inbox)).is_empty());
    drop(rx);

    // Input that fills the spool's disk is refused partway; no offer is
    // made and the spool goes.
    let out = a.run(&["send", "beta", "--at", &route, "-"], &vec![1u8; 20 << 20]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not enough free disk space"),
        "{}",
        text(&out)
    );
    assert!(names(&a.inside(&spool)).is_empty());
    assert_eq!(a.json(&["status"])["spool_bytes"], 0);
    // The receiver keeps no record of an offer refused before consent.
    assert!(
        b.json(&["transfers"])["transfers"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    a.stop();
    b.stop();
}

/// The journey a packaged binary must pass on a host that never ran
/// Hyakume. `cargo xtask qualify` runs it in a fresh container with
/// HYAKUME_BIN naming the binary from the package.
#[test]
#[ignore = "qualifies a packaged binary; run by cargo xtask qualify"]
fn clean_host_journey() {
    let packaged =
        PathBuf::from(std::env::var_os("HYAKUME_BIN").expect("HYAKUME_BIN names the binary"));
    let home = Home::new();
    // Install it the way INSTALL.md says.
    let installed = home.0.join(".local/bin/hyakume");
    std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
    std::fs::copy(&packaged, &installed).unwrap();
    std::fs::set_permissions(&installed, std::fs::Permissions::from_mode(0o755)).unwrap();
    let version = Command::new(&installed).arg("--version").output().unwrap();
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("hyakume "));

    let p = pair_using(home, &installed);
    let (mut a, mut b) = (p.a, p.b);
    for s in [&a, &b] {
        let status = s.json(&["status"]);
        assert_eq!(status["state"], "unlocked");
        assert_eq!(status["store"], "key-file");
    }
    // A restart leaves the key locked until it is unlocked.
    a.stop();
    a.start();
    assert_eq!(a.json(&["status"])["state"], "locked");
    a.unlock();

    let inbox = p.home.folder("inbox");
    let outbox = p.home.folder("outbox");
    let mut rx = Tty::spawn(&b, &["receive", "--dir", inbox.to_str().unwrap()]);
    rx.expect("Receiving into");
    let send = |args: &[&str], input: &[u8], rx: &mut Tty, answer: &str| {
        let route = b.route();
        let mut all = vec!["send", "beta", "--at", &route];
        all.extend_from_slice(args);
        let send = a.spawn(&all, input);
        rx.expect(QUESTION);
        rx.type_line(answer);
        send.join().unwrap()
    };

    std::fs::write(outbox.join("one.txt"), b"one").unwrap();
    std::fs::write(outbox.join("two.txt"), b"two").unwrap();
    std::fs::write(outbox.join("three.txt"), b"three").unwrap();
    let one = outbox.join("one.txt");
    let two = outbox.join("two.txt");
    let three = outbox.join("three.txt");
    for (args, input) in [
        (vec![one.to_str().unwrap()], &b""[..]),
        (vec![two.to_str().unwrap(), three.to_str().unwrap()], b""),
        (vec!["--text", "some text"], b""),
        (vec!["--url", "https://example.com"], b""),
        (vec!["-", "--name", "piped.txt"], b"piped"),
    ] {
        let out = send(&args, input, &mut rx, "y");
        assert!(out.status.success(), "{args:?}: {}", text(&out));
    }
    assert_eq!(std::fs::read(inbox.join("one.txt")).unwrap(), b"one");
    assert_eq!(std::fs::read(inbox.join("three.txt")).unwrap(), b"three");
    assert_eq!(std::fs::read(inbox.join("piped.txt")).unwrap(), b"piped");

    let out = send(&["--text", "no thanks"], b"", &mut rx, "n");
    assert_eq!(out.status.code(), Some(5), "{}", text(&out));
    let route = b.route();
    let pending = a.spawn(
        &["send", "beta", "--at", &route, "--text", "never mind"],
        b"",
    );
    rx.expect(QUESTION);
    let offered = a.json(&["transfers"])["transfers"][0]["transfer"]
        .as_str()
        .unwrap()
        .to_owned();
    let cancelled = a.run(&["cancel", &offered], b"");
    assert_eq!(cancelled.status.code(), Some(5), "{}", text(&cancelled));
    rx.type_line("y");
    let out = pending.join().unwrap();
    assert_eq!(out.status.code(), Some(5), "{}", text(&out));

    // An existing output is preserved.
    std::fs::write(outbox.join("one.txt"), b"changed").unwrap();
    let out = send(&[one.to_str().unwrap()], b"", &mut rx, "y");
    assert_eq!(out.status.code(), Some(5), "{}", text(&out));
    assert_eq!(std::fs::read(inbox.join("one.txt")).unwrap(), b"one");
    drop(rx);

    // Crash the receiver and restart it: what it published stays published.
    b.child.as_mut().unwrap().kill().unwrap();
    b.child.take().unwrap().wait().unwrap();
    b.start();
    b.unlock();
    let transfers = b.json(&["transfers"]);
    let completed = transfers["transfers"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["outcome"] == "completed")
        .count();
    assert_eq!(completed, 5, "{transfers}");

    // Revoke, then old trust no longer works in either direction.
    let out = b.run(&["remove", "alpha"], b"");
    assert!(out.status.success(), "{}", text(&out));
    let route = b.route();
    let out = a.run(
        &["send", "beta", "--at", &route, "--text", "old trust"],
        b"",
    );
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));

    let out = a.run(&["lock"], b"");
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(a.json(&["status"])["state"], "locked");
    a.stop();
    b.stop();

    // Uninstall: the binary goes; received files and the encrypted key
    // stay until the person removes them, as INSTALL.md says.
    std::fs::remove_file(&installed).unwrap();
    assert!(Command::new(&installed).arg("--version").output().is_err());
    assert_eq!(std::fs::read(inbox.join("two.txt")).unwrap(), b"two");
    let key = std::fs::read(a.state.join("identity.age")).unwrap();
    assert!(key.starts_with(b"age-encryption.org/v1"));
}
