//! The `hyakume` command. It holds no keys and no state, so every decision
//! stays with the service and a closed client loses nothing.
//!
//! Exit codes: 0 done, 1 failed or refused, 2 wrong command line, 3 no
//! service, 4 needs `init` or `unlock`, 5 not every item arrived, 130 Ctrl-C.

use std::io::Write;
use std::net::SocketAddr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use tokio::io::AsyncReadExt;
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use zeroize::Zeroizing;

use crate::consent::QUESTION;
use crate::ipc::{
    self, Body, ClientMessage, DeviceView, ErrorCode, Event, Failure, Frame, Reply, Request,
    ServerMessage, StatusView, StreamKind, TransferView,
};
use crate::paths::{Locations, RUNTIME_VAR, STATE_VAR};
use crate::service::{self, ServiceConfig};
use crate::storage::StoreKind;
use crate::terminal::{self, TerminalError};

/// Pass files, text, and links between your own devices.
#[derive(Parser)]
#[command(name = "hyakume", version, about, arg_required_else_help = true)]
pub struct Cli {
    /// The service's state directory [default: $XDG_STATE_HOME/hyakume]
    #[arg(long, global = true, env = STATE_VAR, value_name = "DIR")]
    state_dir: Option<PathBuf>,
    /// The service's runtime directory [default: $XDG_RUNTIME_DIR/hyakume]
    #[arg(long, global = true, env = RUNTIME_VAR, value_name = "DIR")]
    runtime_dir: Option<PathBuf>,
    /// Print results as JSON
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create this device's identity and choose where its key is kept
    Init {
        /// Where to keep the device key
        #[arg(long)]
        store: Option<StoreArg>,
        /// The name this device shows to devices it pairs with
        #[arg(long)]
        name: Option<String>,
        /// Read the key file passphrase from this file descriptor. A
        /// passphrase kept in a file or script no longer protects against a
        /// stolen disk.
        #[arg(long, value_name = "FD")]
        passphrase_fd: Option<i32>,
    },
    /// Run or stop the service
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Show the service, its key store, and its work
    Status,
    /// Load the device key into the service
    Unlock {
        /// Read the passphrase from this file descriptor
        #[arg(long, value_name = "FD")]
        passphrase_fd: Option<i32>,
    },
    /// Remove the device key from the service's memory; work pauses
    Lock,
    /// Pair with another device through an invitation
    Pair {
        /// Create an invitation for the other device to use
        #[arg(long, conflicts_with = "invitation_fd")]
        invite: bool,
        /// Read the other device's invitation from this file descriptor
        /// instead of asking for it
        #[arg(long, value_name = "FD")]
        invitation_fd: Option<i32>,
        /// What this device will call the other one
        #[arg(long)]
        alias: String,
    },
    /// List paired devices
    Devices,
    /// Stop trusting a device and cancel its work
    Remove {
        /// The device's alias or fingerprint
        device: String,
    },
    /// Wait for offers and answer them; Ctrl-C stops
    Receive {
        /// The folder to receive into [default: Hyakume's own folder]
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Send files, text, a link, or standard input ("-") to a device
    Send {
        /// The device's alias or fingerprint
        device: String,
        /// Where the device is reached now, as its `hyakume status` shows
        #[arg(long, value_name = "ROUTE")]
        at: String,
        /// Send this text
        #[arg(long, conflicts_with_all = ["url", "files"])]
        text: Option<String>,
        /// Send this link as text; it is never opened
        #[arg(long, conflicts_with_all = ["text", "files"])]
        url: Option<String>,
        /// The file name standard input arrives under
        #[arg(long)]
        name: Option<String>,
        /// Files to send, or "-" for standard input
        files: Vec<PathBuf>,
    },
    /// Cancel a transfer
    Cancel {
        /// At least 8 hex digits of the transfer's ID
        transfer: String,
    },
    /// Show one transfer's items, outcome, and kept data
    Inspect {
        /// At least 8 hex digits of the transfer's ID
        transfer: String,
    },
    /// List recent transfers
    Transfers,
    /// Retry deleting files Hyakume kept after it could not delete them
    Cleanup {
        /// Start a new retry window for files kept after failed deletion
        #[arg(long, required = true)]
        retry: bool,
    },
    /// Forget this device's identity, trust, and transfers. Paired
    /// devices must pair with it again.
    Reset {
        /// Skip the confirmation question
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Run the service in the foreground
    Run {
        /// Where the peer endpoint listens: an address other devices can
        /// reach, such as 192.168.1.20:7400
        #[arg(long, default_value = "127.0.0.1:0", value_parser = reachable)]
        listen: SocketAddr,
        /// Seconds standard input may take to reach its end, at most 600
        #[arg(long, default_value_t = 600, value_parser = clap::value_parser!(u64).range(1..=600))]
        stdin_deadline: u64,
    },
    /// Ask the running service to stop
    Stop,
}

/// A listening address the service can give to other devices.
fn reachable(text: &str) -> Result<SocketAddr, String> {
    let addr: SocketAddr = text.parse().map_err(|e| format!("{e}"))?;
    if addr.ip().is_unspecified() {
        return Err(crate::node::UNSPECIFIED_BIND.into());
    }
    Ok(addr)
}

#[derive(Clone, Copy, ValueEnum)]
enum StoreArg {
    SecretService,
    KeyFile,
}

/// A failure that ends the command with an exit code.
struct Exit(u8, String);

impl Exit {
    fn new(code: u8, message: impl Into<String>) -> Self {
        Self(code, message.into())
    }
}

impl From<Failure> for Exit {
    fn from(f: Failure) -> Self {
        let code = match f.code {
            ErrorCode::Stopping => 3,
            ErrorCode::Locked | ErrorCode::NotInitialized => 4,
            _ => 1,
        };
        Self(code, f.message)
    }
}

impl From<TerminalError> for Exit {
    fn from(e: TerminalError) -> Self {
        match e {
            TerminalError::Interrupted => Self(130, String::new()),
            e => Self(1, e.to_string()),
        }
    }
}

impl From<ipc::IpcError> for Exit {
    fn from(e: ipc::IpcError) -> Self {
        Self(3, format!("lost the connection to the service: {e}"))
    }
}

/// Runs the parsed command line.
pub fn main(cli: Cli) -> ExitCode {
    let result = match cli.command {
        Command::Service {
            action:
                ServiceAction::Run {
                    listen,
                    stdin_deadline,
                },
        } => run_service(
            cli.state_dir,
            cli.runtime_dir,
            listen,
            std::time::Duration::from_secs(stdin_deadline),
        ),
        command => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            match runtime {
                Ok(rt) => {
                    let result =
                        rt.block_on(client(cli.state_dir, cli.runtime_dir, cli.json, command));
                    // A read of standard input or the terminal may still
                    // be blocked after an interruption or a refusal; it
                    // must not keep the command from ending.
                    rt.shutdown_background();
                    result
                }
                Err(e) => Err(Exit::new(1, e.to_string())),
            }
        }
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(Exit(code, message)) => {
            if !message.is_empty() {
                eprintln!("hyakume: {message}");
            }
            ExitCode::from(code)
        }
    }
}

fn run_service(
    state: Option<PathBuf>,
    runtime: Option<PathBuf>,
    listen: SocketAddr,
    stdin_deadline: std::time::Duration,
) -> Result<u8, Exit> {
    let locations = Locations::resolve(state, runtime).map_err(|e| Exit::new(1, e.to_string()))?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Exit::new(1, e.to_string()))?;
    let result = rt.block_on(service::run(ServiceConfig {
        locations,
        listen,
        stdin_deadline,
    }));
    // Blocking work that outlived the drain is not waited for again.
    rt.shutdown_background();
    result.map(|()| 0).map_err(|e| Exit::new(1, e.to_string()))
}

/// One connection to the service.
struct Conn {
    rd: ipc::FrameReader<OwnedReadHalf>,
    wr: OwnedWriteHalf,
    next: u64,
}

impl Conn {
    async fn open(locations: &Locations) -> Result<Self, Exit> {
        let socket = locations.socket();
        let stream = UnixStream::connect(&socket).await.map_err(|_| {
            Exit::new(
                3,
                format!(
                    "the service is not running (no socket at {}); start it with hyakume service run",
                    socket.display()
                ),
            )
        })?;
        // Passphrases and invitations go over this socket, so it must be a
        // service of this user, wherever the runtime directory points.
        let owner = stream.peer_cred().map(|c| c.uid());
        if owner.ok() != Some(rustix::process::geteuid().as_raw()) {
            return Err(Exit::new(
                3,
                format!(
                    "{} is not a service of this user; nothing was sent",
                    socket.display()
                ),
            ));
        }
        let (rd, wr) = stream.into_split();
        let mut conn = Self {
            rd: ipc::FrameReader::new(rd),
            wr,
            next: 1,
        };
        ipc::write_message(
            &mut conn.wr,
            &ClientMessage::Hello {
                version: ipc::VERSION,
            },
        )
        .await?;
        match conn.message().await? {
            ServerMessage::Hello { version } if version == ipc::VERSION => Ok(conn),
            ServerMessage::Hello { version } => Err(Exit::new(
                3,
                format!(
                    "the service speaks protocol {version}, this command {}",
                    ipc::VERSION
                ),
            )),
            ServerMessage::Reply {
                reply: Reply::Failed { failure },
                ..
            } => Err(Exit::new(3, failure.message)),
            _ => Err(Exit::new(3, "the service answered out of turn")),
        }
    }

    async fn message(&mut self) -> Result<ServerMessage, Exit> {
        match self.rd.next().await? {
            Frame::Message(body) => Ok(ipc::decode(&body)?),
            _ => Err(Exit::new(3, "the service sent an unexpected frame")),
        }
    }

    async fn request(&mut self, request: Request) -> Result<u64, Exit> {
        let id = self.next;
        self.next += 1;
        ipc::write_message(&mut self.wr, &ClientMessage::Request { id, request }).await?;
        Ok(id)
    }

    /// Sends a request and waits for its reply, ignoring events.
    async fn call(&mut self, request: Request) -> Result<Body, Exit> {
        let id = self.request(request).await?;
        loop {
            match self.message().await? {
                ServerMessage::Reply { id: got, reply } if got == id => return reply_body(reply),
                _ => {}
            }
        }
    }

    async fn answer(&mut self, prompt: u64, context: String, accept: bool) -> Result<(), Exit> {
        ipc::write_message(
            &mut self.wr,
            &ClientMessage::Answer {
                prompt,
                context,
                accept,
            },
        )
        .await?;
        Ok(())
    }
}

fn reply_body(reply: Reply) -> Result<Body, Exit> {
    match reply {
        Reply::Ok { body } => Ok(body),
        Reply::Failed { failure } => Err(failure.into()),
    }
}

fn print_json(value: &impl serde::Serialize) {
    if let Ok(text) = serde_json::to_string(value) {
        println!("{text}");
    }
}

async fn client(
    state: Option<PathBuf>,
    runtime: Option<PathBuf>,
    json: bool,
    command: Command,
) -> Result<u8, Exit> {
    let locations = Locations::resolve(state, runtime).map_err(|e| Exit::new(1, e.to_string()))?;
    let mut conn = Conn::open(&locations).await?;
    match command {
        Command::Service {
            action: ServiceAction::Stop,
        } => {
            conn.call(Request::Stop {}).await?;
            if !json {
                println!("The service is stopping.");
            }
            Ok(0)
        }
        Command::Service { .. } => Ok(0),
        Command::Status => {
            let body = conn.call(Request::Status {}).await?;
            show(json, &body);
            Ok(0)
        }
        Command::Init {
            store,
            name,
            passphrase_fd,
        } => init(&mut conn, json, store, name, passphrase_fd).await,
        Command::Unlock { passphrase_fd } => unlock(&mut conn, json, passphrase_fd).await,
        Command::Lock => {
            let body = conn.call(Request::Lock {}).await?;
            show(json, &body);
            Ok(0)
        }
        Command::Pair {
            invite,
            invitation_fd,
            alias,
        } => pair(&mut conn, json, invite, invitation_fd, alias).await,
        Command::Devices => {
            let body = conn.call(Request::Devices {}).await?;
            show(json, &body);
            Ok(0)
        }
        Command::Remove { device } => {
            let body = conn.call(Request::Remove { device }).await?;
            show(json, &body);
            Ok(0)
        }
        Command::Receive { dir } => receive(&mut conn, json, dir).await,
        Command::Send {
            device,
            at,
            text,
            url,
            name,
            files,
        } => {
            send(
                &locations, &mut conn, json, device, at, text, url, name, files,
            )
            .await
        }
        Command::Cancel { transfer } => {
            let body = conn.call(Request::Cancel { transfer }).await?;
            show(json, &body);
            Ok(transfer_code(&body))
        }
        Command::Inspect { transfer } => {
            let body = conn.call(Request::Inspect { transfer }).await?;
            show(json, &body);
            Ok(0)
        }
        Command::Transfers => {
            let body = conn.call(Request::Transfers {}).await?;
            show(json, &body);
            Ok(0)
        }
        Command::Cleanup { .. } => {
            let body = conn.call(Request::Cleanup {}).await?;
            show(json, &body);
            Ok(0)
        }
        Command::Reset { yes } => {
            if !yes {
                if !terminal::available() {
                    return Err(Exit::new(
                        2,
                        "reset needs --yes when no terminal is attached",
                    ));
                }
                terminal::say(
                    "This forgets this device's identity, its paired devices, and its transfers.\n\
                     Paired devices will have to pair with it again.\n",
                )?;
                if !terminal::yes("Forget this device's identity? [y/N]").await? {
                    return Err(Exit::new(1, "nothing was changed"));
                }
            }
            let body = conn.call(Request::Reset {}).await?;
            show(json, &body);
            Ok(0)
        }
    }
}

/// Reads a passphrase from a descriptor, or from the terminal with echo
/// off, typed twice when `confirm` is set.
async fn passphrase(fd: Option<i32>, confirm: bool) -> Result<Zeroizing<Vec<u8>>, Exit> {
    if let Some(fd) = fd {
        return Ok(terminal::read_fd_line(fd)?);
    }
    if !terminal::available() {
        return Err(Exit::new(
            2,
            "no terminal to read the passphrase from; pass --passphrase-fd",
        ));
    }
    let first = terminal::secret("Passphrase: ").await?;
    if confirm {
        let second = terminal::secret("Passphrase again: ").await?;
        if first != second {
            return Err(Exit::new(1, "the passphrases differ"));
        }
    }
    Ok(first)
}

async fn init(
    conn: &mut Conn,
    json: bool,
    store: Option<StoreArg>,
    name: Option<String>,
    passphrase_fd: Option<i32>,
) -> Result<u8, Exit> {
    let store = match store {
        Some(StoreArg::KeyFile) => StoreKind::KeyFile,
        Some(StoreArg::SecretService) => StoreKind::SecretService,
        None => {
            let Body::Stores(stores) = conn.call(Request::Stores {}).await? else {
                return Err(Exit::new(1, "the service answered out of turn"));
            };
            if !terminal::available() {
                return Err(Exit::new(
                    2,
                    "choose a store with --store secret-service or --store key-file",
                ));
            }
            let secret_service = stores.secret_service == "unlocked";
            let mut text = String::from("Where should this device's key be kept?\n");
            if secret_service {
                text.push_str("  1. The Secret Service (your desktop keyring), unlocked now\n");
            } else {
                text.push_str(&format!(
                    "  1. The Secret Service: not usable now ({})\n",
                    stores.secret_service
                ));
            }
            text.push_str("  2. A key file protected by a passphrase\n");
            terminal::say(&text)?;
            match terminal::line("Choice [1/2]: ").await?.as_str() {
                "1" if secret_service => StoreKind::SecretService,
                "2" => StoreKind::KeyFile,
                _ => {
                    return Err(Exit::new(
                        1,
                        "no store chosen; set up the Secret Service or choose the key file",
                    ));
                }
            }
        }
    };

    let name = match name {
        Some(n) => n,
        None if terminal::available() => terminal::line("Name for this device: ").await?,
        None => return Err(Exit::new(2, "give this device a name with --name")),
    };

    let secret = match store {
        StoreKind::KeyFile => {
            if passphrase_fd.is_some() {
                eprintln!(
                    "hyakume: note: a passphrase stored for scripts gives up its protection against a stolen disk"
                );
            }
            Some(passphrase(passphrase_fd, true).await?)
        }
        StoreKind::SecretService => None,
    };

    let id = conn.request(Request::Init { store, name }).await?;
    if let Some(secret) = secret {
        ipc::write_secret(&mut conn.wr, &secret).await?;
    }
    let body = wait_reply(conn, id).await?;
    show(json, &body);
    Ok(0)
}

async fn unlock(conn: &mut Conn, json: bool, passphrase_fd: Option<i32>) -> Result<u8, Exit> {
    let Body::Status(status) = conn.call(Request::Status {}).await? else {
        return Err(Exit::new(1, "the service answered out of turn"));
    };
    if status.state == "unlocked" {
        show(json, &Body::Status(status));
        return Ok(0);
    }

    let secret = match status.store {
        Some(StoreKind::KeyFile) => Some(passphrase(passphrase_fd, false).await?),
        _ => None,
    };

    let id = conn.request(Request::Unlock {}).await?;
    if let Some(secret) = secret {
        ipc::write_secret(&mut conn.wr, &secret).await?;
    }
    let body = wait_reply(conn, id).await?;
    show(json, &body);
    Ok(0)
}

async fn wait_reply(conn: &mut Conn, id: u64) -> Result<Body, Exit> {
    loop {
        if let ServerMessage::Reply { id: got, reply } = conn.message().await?
            && got == id
        {
            return reply_body(reply);
        }
    }
}

async fn pair(
    conn: &mut Conn,
    json: bool,
    invite: bool,
    invitation_fd: Option<i32>,
    alias: String,
) -> Result<u8, Exit> {
    if !terminal::available() {
        return Err(Exit::new(
            2,
            "pairing needs a terminal: both people compare the words on their screens",
        ));
    }

    let id = if invite {
        conn.request(Request::Invite { alias }).await?
    } else {
        let token = match invitation_fd {
            Some(fd) => terminal::read_fd_line(fd)?,
            None => terminal::secret("Invitation from the other device: ").await?,
        };
        let id = conn.request(Request::Join { alias }).await?;
        ipc::write_secret(&mut conn.wr, &token).await?;
        id
    };

    loop {
        let message = tokio::select! {
            m = conn.message() => m?,
            _ = tokio::signal::ctrl_c() => return Err(Exit::new(130, "")),
        };
        match message {
            ServerMessage::Event {
                event: Event::Invitation { token },
                ..
            } => {
                // The invitation goes only to this terminal's output, as the
                // explicit result of asking for it.
                println!("Give this invitation to the other device:\n{token}");
                println!("Waiting for it to be used...");
                let _ = std::io::stdout().flush();
            }
            ServerMessage::Event {
                event:
                    Event::Verify {
                        prompt,
                        context,
                        fingerprint,
                        name,
                        words,
                    },
                ..
            } => {
                terminal::say(&format!(
                    "Device {fingerprint}, which calls itself {name} (set by that device)\n\
                     Both screens must show the same words:\n\n    {words}\n\n"
                ))?;
                let accept = terminal::yes("Do the words match? [y/N]").await?;
                conn.answer(prompt, context, accept).await?;
            }
            ServerMessage::Reply { id: got, reply } if got == id => {
                let body = reply_body(reply)?;
                if json {
                    print_json(&body);
                } else if let Body::Device(d) = &body {
                    println!("Paired with {} ({}).", d.alias, d.fingerprint);
                }
                return Ok(0);
            }
            ServerMessage::AnswerRefused { .. } => {
                return Err(Exit::new(1, "the answer came too late; nothing was saved"));
            }
            _ => {}
        }
    }
}

async fn receive(conn: &mut Conn, json: bool, dir: Option<PathBuf>) -> Result<u8, Exit> {
    if !terminal::available() {
        return Err(Exit::new(
            2,
            "receiving needs a terminal to ask you about each offer",
        ));
    }
    // Relative to this command's directory, without resolving links: the
    // service refuses a path through a symbolic link.
    let mut dir = match dir {
        Some(d) => Some(
            std::path::absolute(&d)
                .map_err(|e| Exit::new(1, format!("{}: {e}", d.display())))?
                .as_os_str()
                .as_bytes()
                .to_vec(),
        ),
        None => None,
    };

    loop {
        let id = conn.request(Request::Receive { dir: dir.clone() }).await?;
        loop {
            let message = tokio::select! {
                m = conn.message() => m?,
                _ = tokio::signal::ctrl_c() => return Err(Exit::new(130, "")),
            };
            match message {
                ServerMessage::Event {
                    event: Event::Receiving { dir },
                    ..
                } => {
                    eprintln!("Receiving into {dir}. Ctrl-C stops.");
                }
                ServerMessage::Event {
                    event:
                        Event::Offer {
                            prompt,
                            context,
                            screen,
                            ..
                        },
                    ..
                } => {
                    let shown = screen.strip_suffix(QUESTION).unwrap_or(&screen);
                    terminal::say(shown)?;
                    let accept = terminal::yes(QUESTION).await?;
                    conn.answer(prompt, context, accept).await?;
                }
                ServerMessage::Event {
                    event: Event::Received(view),
                    ..
                } => {
                    if json {
                        print_json(&view);
                    } else {
                        print_transfer(&view);
                    }
                }
                ServerMessage::AnswerRefused { .. } => {
                    eprintln!("That answer came too late; the offer was already closed.");
                }
                ServerMessage::Reply { id: got, reply } if got == id => match reply {
                    Reply::Ok { .. } => return Ok(0),
                    Reply::Failed { failure }
                        if failure.code == ErrorCode::Destination && dir.is_some() =>
                    {
                        terminal::say(&format!("{}\n", failure.message))?;
                        if terminal::yes(
                            "Receive into Hyakume's own folder instead, which supports safe publication? [y/N]",
                        )
                        .await?
                        {
                            dir = None;
                            break;
                        }
                        return Err(Exit::new(1, "nothing will be received"));
                    }
                    Reply::Failed { failure } => return Err(failure.into()),
                },
                _ => {}
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn send(
    locations: &Locations,
    conn: &mut Conn,
    json: bool,
    device: String,
    route: String,
    text: Option<String>,
    url: Option<String>,
    name: Option<String>,
    files: Vec<PathBuf>,
) -> Result<u8, Exit> {
    let stdin = files.len() == 1 && files[0].as_os_str() == "-";
    let stream = match (&text, &url, stdin) {
        (Some(t), _, _) => Some((StreamKind::Text, Some(t.clone().into_bytes()))),
        (_, Some(u), _) => Some((StreamKind::Url, Some(u.clone().into_bytes()))),
        (_, _, true) => Some((StreamKind::Stdin, None)),
        _ => None,
    };

    if name.is_some() && !stdin {
        return Err(Exit::new(
            2,
            "--name applies only to standard input (\"-\")",
        ));
    }

    let id = match stream {
        Some((kind, bytes)) => {
            let id = conn
                .request(Request::SendStream {
                    device,
                    route,
                    kind,
                    name,
                })
                .await?;
            let streamed = match bytes {
                Some(bytes) => stream_bytes(conn, &bytes).await,
                None => stream_stdin(conn, id).await,
            };
            match streamed {
                Ok(()) => {}
                // The connection broke while sending: the service may have
                // refused the input and said why before closing.
                Err(exit) if exit.0 == 3 => {
                    return Err(early_reply(conn, id).await.unwrap_or(exit));
                }
                Err(exit) => return Err(exit),
            }
            ipc::write_message(&mut conn.wr, &ClientMessage::StreamEnd).await?;
            id
        }
        None => {
            if files.is_empty() {
                return Err(Exit::new(
                    2,
                    "name files to send, or use --text, --url, or -",
                ));
            }
            let mut paths = Vec::with_capacity(files.len());
            for f in &files {
                // Relative to this command's directory, without resolving
                // links: the service refuses a link it is given.
                let path = std::path::absolute(f)
                    .map_err(|e| Exit::new(1, format!("{}: {e}", f.display())))?;
                paths.push(path.as_os_str().as_bytes().to_vec());
            }
            conn.request(Request::SendFiles {
                device,
                route,
                paths,
            })
            .await?
        }
    };

    let mut started: Option<String> = None;
    loop {
        let message = tokio::select! {
            m = conn.message() => m?,
            _ = tokio::signal::ctrl_c() => {
                return interrupt(locations, json, started).await;
            }
        };
        match message {
            ServerMessage::Event {
                event: Event::Started { transfer },
                ..
            } => {
                if !json {
                    eprintln!("Offered as transfer {transfer}; waiting for an answer.");
                }
                started = Some(transfer);
            }
            ServerMessage::Reply { id: got, reply } if got == id => {
                let body = reply_body(reply)?;
                show(json, &body);
                return Ok(transfer_code(&body));
            }
            _ => {}
        }
    }
}

async fn stream_bytes(conn: &mut Conn, bytes: &[u8]) -> Result<(), Exit> {
    for part in bytes.chunks(ipc::MAX_FRAME_BYTES) {
        ipc::write_data(&mut conn.wr, part).await?;
    }
    Ok(())
}

/// Streams standard input as data frames. Stops on Ctrl-C, or when the
/// service answers early, which can only be a refusal.
async fn stream_stdin(conn: &mut Conn, id: u64) -> Result<(), Exit> {
    let mut stdin = tokio::io::stdin();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let read = tokio::select! {
            r = stdin.read(&mut buf) => r.map_err(|e| Exit::new(1, format!("reading standard input: {e}")))?,
            m = conn.message() => match m? {
                ServerMessage::Reply { id: got, reply } if got == id => {
                    return Err(match reply_body(reply) {
                        Err(exit) => exit,
                        Ok(_) => Exit::new(1, "the service answered before the input ended"),
                    });
                }
                _ => continue,
            },
            _ = tokio::signal::ctrl_c() => return Err(Exit::new(130, "")),
        };
        if read == 0 {
            return Ok(());
        }
        ipc::write_data(&mut conn.wr, &buf[..read]).await?;
    }
}

/// A refusal the service sent while the client was still streaming.
async fn early_reply(conn: &mut Conn, id: u64) -> Option<Exit> {
    let wait = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match conn.message().await {
                Ok(ServerMessage::Reply { id: got, reply }) if got == id => {
                    return reply_body(reply).err();
                }
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    });
    wait.await.ok().flatten()
}

/// Ctrl-C during a send: once the offer exists, ask the service to cancel
/// it and wait a bounded time for the answer.
async fn interrupt(locations: &Locations, json: bool, started: Option<String>) -> Result<u8, Exit> {
    let Some(transfer) = started else {
        // Nothing was offered; the service drops the unfinished input.
        return Err(Exit::new(130, ""));
    };

    eprintln!("Cancelling {transfer}...");
    let mut conn = Conn::open(locations).await?;
    let body = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        conn.call(Request::Cancel { transfer }),
    )
    .await
    .map_err(|_| Exit::new(130, "the service did not confirm the cancellation in time"))??;
    show(json, &body);
    Err(Exit::new(130, ""))
}

/// Exit code 5 when a transfer ended without every item arriving.
fn transfer_code(body: &Body) -> u8 {
    match body {
        Body::Transfer(t) if t.outcome != "completed" => 5,
        _ => 0,
    }
}

fn show(json: bool, body: &Body) {
    if json {
        print_json(body);
        return;
    }
    match body {
        Body::Done => {}
        Body::Status(s) => print_status(s),
        Body::Stores(s) => {
            println!("Secret Service: {}", s.secret_service);
            println!("Key file: available");
        }
        Body::Devices { devices } => {
            if devices.is_empty() {
                println!("No paired devices.");
            }
            for d in devices {
                print_device(d);
            }
        }
        Body::Device(d) => print_device(d),
        Body::Transfer(t) => print_transfer(t),
        Body::Transfers { transfers } => {
            if transfers.is_empty() {
                println!("No transfers.");
            }
            for t in transfers {
                println!(
                    "{}  {}  {} {}  {}",
                    &t.transfer[..8],
                    if t.outgoing { "to  " } else { "from" },
                    t.peer,
                    t.kind,
                    t.outcome
                );
            }
        }
        Body::Cleanup { retried } => {
            println!("Retrying deletion of {retried} kept files.");
        }
    }
}

fn print_status(s: &StatusView) {
    println!("Service: running, {}", s.state);
    if let Some(store) = s.store {
        println!("Key store: {}", store.name());
    }
    if let Some(fp) = &s.fingerprint {
        println!("Device: {} ({fp})", s.name.as_deref().unwrap_or(""));
    }
    if let Some(route) = &s.route {
        println!("Route: {route}");
    }
    println!("Paired devices: {}", s.trusted);
    println!(
        "Unfinished transfers: {} ({} paused)",
        s.unfinished, s.paused
    );
    if let Some(dir) = &s.receiving {
        println!("Receiving into: {dir}");
    }
    println!(
        "Private storage in use: {} bytes in spools, {} bytes in partial files",
        s.spool_bytes, s.partial_bytes
    );
    if s.retained_files > 0 {
        println!(
            "Kept after failed deletion: {} files, {} bytes; run hyakume cleanup --retry",
            s.retained_files, s.retained_bytes
        );
    }
    if s.store_failed {
        println!("A write to the state database failed; restart the service.");
    }
}

fn print_device(d: &DeviceView) {
    let state = if d.trusted { "trusted" } else { "removed" };
    println!("{}  {}  {}  {state}", d.alias, d.fingerprint, d.name);
}

fn print_transfer(t: &TransferView) {
    let direction = if t.outgoing { "to" } else { "from" };
    println!(
        "Transfer {} {direction} {}: {} ({})",
        t.transfer, t.peer, t.outcome, t.kind
    );
    if let Some(place) = &t.place {
        println!("  Folder: {place}");
    }
    for item in &t.items {
        match &item.output {
            Some(output) => println!(
                "  {} ({} bytes): {} as {output}",
                item.name, item.size, item.state
            ),
            None => println!("  {} ({} bytes): {}", item.name, item.size, item.state),
        }
    }
    if t.retained_bytes > 0 {
        println!("  Data still kept on disk: {} bytes", t.retained_bytes);
    }
}
