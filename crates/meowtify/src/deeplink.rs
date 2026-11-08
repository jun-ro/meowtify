//! Opening `meowtify://jam/…` links: the app is the handler for the link scheme, and a second
//! start hands its link to the copy that is already running.
//!
//! The copies talk over a loopback TCP port. The first copy to bind it is the main one. Anyone
//! on this machine can connect to it, so a message is only a link, and the app asks the user
//! before it joins anything.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use meowtify_jam::JamLink;
use tokio::sync::mpsc::UnboundedSender;

const PORT: u16 = 47625;
/// First line of a message. Anything else on the port is not from this app.
const HELLO: &str = "MEOWTIFY-LINK";
const IO_TIMEOUT: Duration = Duration::from_secs(2);
/// A link is a few hundred bytes. Longer input is not read.
const MAX_MESSAGE: u64 = 2048;

/// What starting up found.
pub enum Instance {
    /// This copy is the main one. Links that other copies receive arrive on the sender given to
    /// [`start`].
    Main,
    /// The link went to the copy that was running. This one should exit.
    Forwarded,
}

/// Claims the port, or passes `link` to the copy that holds it.
///
/// A copy started without a link never exits here: if the port is busy for another reason, the
/// app still starts, it just cannot receive links.
pub fn start(link: Option<&str>, links: UnboundedSender<String>) -> Instance {
    match TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, PORT))) {
        Ok(listener) => {
            std::thread::Builder::new()
                .name("deeplink".into())
                .spawn(move || serve(listener, links))
                .ok();
            Instance::Main
        }
        Err(_) => match link {
            Some(link) if forward(link) => Instance::Forwarded,
            _ => Instance::Main,
        },
    }
}

fn forward(link: &str) -> bool {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, PORT));
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, IO_TIMEOUT) else { return false };
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    stream.write_all(format!("{HELLO}\n{link}\n").as_bytes()).is_ok()
}

fn serve(listener: TcpListener, links: UnboundedSender<String>) {
    for stream in listener.incoming().flatten() {
        if let Some(link) = read_link(stream) {
            if links.send(link).is_err() {
                return; // the app is shutting down
            }
        }
    }
}

fn read_link(stream: TcpStream) -> Option<String> {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let mut text = String::new();
    stream.take(MAX_MESSAGE).read_to_string(&mut text).ok()?;
    let mut lines = text.lines();
    if lines.next()? != HELLO {
        return None;
    }
    let link = lines.next()?.trim();
    // Only a well formed jam link gets any further.
    JamLink::parse(link).map(|_| link.to_string())
}

/// Makes Windows open `meowtify://` links with this program. Runs on its own thread and does
/// nothing when the link already points here. Development builds skip it unless
/// `MEOWTIFY_REGISTER` is set, so a debug build does not take the link from the installed app.
pub fn register() {
    if cfg!(debug_assertions) && std::env::var_os("MEOWTIFY_REGISTER").is_none() {
        return;
    }
    std::thread::Builder::new().name("register-links".into()).spawn(register_now).ok();
}

#[cfg(windows)]
fn register_now() {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    const KEY: &str = r"HKCU\Software\Classes\meowtify";
    let Ok(exe) = std::env::current_exe() else { return };
    let command = format!("\"{}\" \"%1\"", exe.display());
    let reg = |args: &[&str]| {
        Command::new("reg").args(args).creation_flags(0x0800_0000).output().ok().filter(|o| o.status.success())
    };

    let shell = format!(r"{KEY}\shell\open\command");
    let known = reg(&["query", &shell, "/ve"]).map(|o| String::from_utf8_lossy(&o.stdout).contains(&command)).unwrap_or(false);
    if known {
        return;
    }
    let _ = reg(&["add", KEY, "/ve", "/d", "URL:Meowtify", "/f"]);
    let _ = reg(&["add", KEY, "/v", "URL Protocol", "/d", "", "/f"]);
    let _ = reg(&["add", &shell, "/ve", "/d", &command, "/f"]);
}

#[cfg(not(windows))]
fn register_now() {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn reads(message: &str) -> Option<String> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(addr).unwrap();
        client.write_all(message.as_bytes()).unwrap();
        drop(client);
        read_link(listener.accept().unwrap().0)
    }

    const LINK: &str = "meowtify://jam/abc123xy?relay=wss%3A%2F%2Fjam.example.com";

    #[test]
    fn a_jam_link_gets_through() {
        assert_eq!(reads(&format!("{HELLO}\n{LINK}\n")).as_deref(), Some(LINK));
    }

    #[test]
    fn anything_else_is_dropped() {
        assert_eq!(reads(&format!("GET / HTTP/1.1\n{LINK}\n")), None, "an HTTP request is not a message");
        assert_eq!(reads(&format!("{HELLO}\nhttps://example.com\n")), None, "not a jam link");
        assert_eq!(reads(HELLO), None, "no link");
        assert_eq!(reads(""), None);
    }
}
