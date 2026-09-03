//! Same-build helper lifecycle and opt-in private runtime initialization. No devices or media yet.

mod runtime;

use std::io;
use std::io::Write;
use std::sync::mpsc;
use std::time::Duration;

use codex_realtime_webrtc::Message;
use codex_realtime_webrtc::encode_frame;
use codex_realtime_webrtc::read_message;

const BUILD_COMMIT: &str = match option_env!("STABLE_GIT_COMMIT") {
    Some(commit) => commit,
    None => "dev",
};

fn main() {
    codex_process_hardening::pre_main_hardening();
    let mut args = std::env::args_os().skip(/*n*/ 1);
    match (args.next(), args.next()) {
        (Some(arg), None) if arg == "--build-commit" => println!("{BUILD_COMMIT}"),
        (None, None) => {
            if run().is_err() {
                std::process::exit(/*code*/ 1);
            }
        }
        _ => std::process::exit(/*code*/ 2),
    }
}

fn run() -> io::Result<()> {
    let (sender, receiver) = mpsc::sync_channel(/*bound*/ 1);
    std::thread::Builder::new()
        .name("voice-control".into())
        .spawn(move || {
            let mut input = io::stdin().lock();
            loop {
                match read_message(&mut input) {
                    Ok(Some(message)) => {
                        if sender.try_send(message).is_err() {
                            std::process::exit(/*code*/ 1);
                        }
                    }
                    Ok(None) => break,
                    Err(_) => std::process::exit(/*code*/ 1),
                }
            }
            drop(sender);
            // Independent of the main worker or a blocked stdout write, including after parent death.
            std::thread::sleep(Duration::from_secs(/*secs*/ 2));
            std::process::exit(/*code*/ 1);
        })?;
    let Ok(hello) = receiver.recv() else {
        return Ok(());
    };
    if hello
        != (Message::Hello {
            protocol: 1,
            build_commit: BUILD_COMMIT.to_owned(),
        })
    {
        return Err(io::Error::other("incompatible voice helper"));
    }
    let mut output = io::stdout().lock();
    output.write_all(&encode_frame(&Message::Ready {})?)?;
    output.flush()?;
    let mut runtime = None;
    loop {
        let reply = match receiver.recv() {
            Ok(Message::InitializeRuntime {}) => {
                if runtime.is_some() {
                    return Err(io::Error::other("runtime already initialized"));
                }
                runtime = Some(runtime::Runtime::initialize()?);
                Message::RuntimeReady {}
            }
            Ok(Message::Close {}) => {
                output.write_all(&encode_frame(&Message::Closed {})?)?;
                return output.flush();
            }
            Err(_) => return Ok(()),
            Ok(
                Message::Hello { .. }
                | Message::Ready {}
                | Message::RuntimeReady {}
                | Message::Closed {},
            ) => return Err(io::Error::other("invalid voice control sequence")),
        };
        output.write_all(&encode_frame(&reply)?)?;
        output.flush()?;
    }
}
