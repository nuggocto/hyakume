//! The controlling terminal. Answers and passphrases are read only from
//! it, never from standard input, so piped data cannot answer a question by
//! accident. A passphrase is read with echo off, and the terminal settings
//! come back on every way out.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};

use rustix::termios::{LocalModes, OptionalActions, Termios};
use zeroize::Zeroizing;

/// Longest line read from the terminal.
const MAX_LINE: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    #[error("this needs a terminal to ask you; none is attached")]
    NoTerminal,
    #[error("interrupted")]
    Interrupted,
    #[error("the answer is longer than {MAX_LINE} bytes")]
    TooLong,
    #[error("{0}")]
    Io(#[from] io::Error),
}

/// Whether a controlling terminal is attached.
pub fn available() -> bool {
    open().is_ok()
}

fn open() -> Result<File, TerminalError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|_| TerminalError::NoTerminal)
}

/// Writes to the terminal, not to standard output, which a script may read.
pub fn say(text: &str) -> Result<(), TerminalError> {
    let mut tty = open()?;
    tty.write_all(text.as_bytes())?;
    tty.flush()?;
    Ok(())
}

fn read_line(tty: &mut File) -> Result<Zeroizing<Vec<u8>>, TerminalError> {
    let mut line = Zeroizing::new(Vec::new());
    let mut byte = [0u8; 1];
    loop {
        match tty.read(&mut byte)? {
            0 => break,
            _ if byte[0] == b'\n' => break,
            _ if line.len() == MAX_LINE => return Err(TerminalError::TooLong),
            _ => line.push(byte[0]),
        }
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    Ok(line)
}

/// Puts terminal settings back when dropped, however the read ended.
struct Restore {
    tty: File,
    saved: Termios,
}

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = rustix::termios::tcsetattr(&self.tty, OptionalActions::Now, &self.saved);
    }
}

/// Runs a blocking terminal read, giving up on Ctrl-C. With `restore`, the
/// settings go back at once and the cursor moves past the hidden input.
async fn blocking<T: Send + 'static>(
    restore: Option<&Restore>,
    read: impl FnOnce() -> Result<T, TerminalError> + Send + 'static,
) -> Result<T, TerminalError> {
    let task = tokio::task::spawn_blocking(read);
    tokio::select! {
        result = task => result.map_err(|_| TerminalError::Interrupted)?,
        _ = tokio::signal::ctrl_c() => {
            if let Some(r) = restore {
                let _ = rustix::termios::tcsetattr(&r.tty, OptionalActions::Now, &r.saved);
                let _ = (&r.tty).write_all(b"\n");
            }
            Err(TerminalError::Interrupted)
        }
    }
}

/// Asks a yes-or-no question. Only `y` or `yes` is yes.
pub async fn yes(question: &str) -> Result<bool, TerminalError> {
    let mut tty = open()?;
    tty.write_all(format!("{question} ").as_bytes())?;
    tty.flush()?;
    let line = blocking(None, move || read_line(&mut tty)).await?;
    let answer = String::from_utf8_lossy(&line).trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

/// Asks for a line of text, shown as it is typed.
pub async fn line(prompt: &str) -> Result<String, TerminalError> {
    let mut tty = open()?;
    tty.write_all(prompt.as_bytes())?;
    tty.flush()?;
    let line = blocking(None, move || read_line(&mut tty)).await?;
    Ok(String::from_utf8_lossy(&line).trim().to_owned())
}

/// Asks for a secret with echo off.
pub async fn secret(prompt: &str) -> Result<Zeroizing<Vec<u8>>, TerminalError> {
    let mut tty = open()?;
    let saved = rustix::termios::tcgetattr(&tty).map_err(io::Error::from)?;
    // Ready before echo goes off, so every way out puts it back.
    let restore = Restore {
        tty: tty.try_clone()?,
        saved: saved.clone(),
    };
    let mut quiet = saved.clone();
    quiet.local_modes.remove(LocalModes::ECHO);
    quiet.local_modes.insert(LocalModes::ECHONL);
    // Echo goes off before the prompt shows, so nothing typed or pasted as
    // soon as it appears can reach the screen.
    rustix::termios::tcsetattr(&tty, OptionalActions::Now, &quiet).map_err(io::Error::from)?;
    tty.write_all(prompt.as_bytes())?;
    tty.flush()?;
    blocking(Some(&restore), move || {
        let line = read_line(&mut tty);
        let _ = rustix::termios::tcsetattr(&tty, OptionalActions::Now, &saved);
        line
    })
    .await
}

/// Reads one line from an inherited file descriptor, such as one a script
/// passes with `--passphrase-fd`. At most `MAX_LINE` bytes.
pub fn read_fd_line(fd: i32) -> Result<Zeroizing<Vec<u8>>, TerminalError> {
    // Opening the descriptor's /dev/fd entry reads it without taking
    // ownership of a raw descriptor.
    let mut file = File::open(format!("/dev/fd/{fd}"))?;
    let mut line = Zeroizing::new(Vec::new());
    let mut byte = [0u8; 1];
    loop {
        match file.read(&mut byte)? {
            0 => break,
            _ if byte[0] == b'\n' => break,
            _ if line.len() == MAX_LINE => return Err(TerminalError::TooLong),
            _ => line.push(byte[0]),
        }
    }
    Ok(line)
}
