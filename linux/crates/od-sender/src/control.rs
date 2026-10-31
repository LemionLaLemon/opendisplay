//! Live control socket for a companion app.
//!
//! The pen area and mode are read per event, so they can be changed while the
//! session runs instead of restarting the sender (which would rebuild the
//! display). The protocol is one line per command, one line back:
//!
//! ```text
//! pencil-area 0.5            -> ok area 0.5x0.5
//! pencil-area 0,0.05,1,0.9   -> ok area 1x0.9
//! pencil-mode move           -> ok move
//! pencil-motion relative     -> ok relative
//! pencil-sensitivity 1.5     -> ok 1.5
//! status                     -> ok area 0.5x0.5 at 0.25,0.25 mode Move
//!                               motion Relative sensitivity 1.50
//! ```
//!
//! Anything else answers `error …`. The socket is removed when the sender
//! starts, and is safe to leave behind on a crash: the next start replaces it.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

use anyhow::{Context, Result};
use clap::ValueEnum;

use crate::wayland::input::{self, PencilArea, PencilMode, PencilMotion};

/// Serve `path` on a background thread until the process exits.
pub fn serve(path: &Path) -> Result<()> {
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).ok();
        }
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("control socket {}", path.display()))?;
    std::thread::Builder::new()
        .name("control".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = handle(stream);
            }
        })
        .context("starting the control thread")?;
    Ok(())
}

fn handle(stream: UnixStream) -> Result<()> {
    let mut out = stream.try_clone()?;
    for line in BufReader::new(stream).lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (cmd, arg) = match line.split_once(char::is_whitespace) {
            Some((c, a)) => (c, a.trim()),
            None => (line, ""),
        };
        let reply = match cmd {
            "pencil-area" | "area" => match PencilArea::parse(arg) {
                Ok(a) => {
                    input::set_pencil_area(a);
                    format!("ok area {}x{} at {}x{}", a.w, a.h, a.x0, a.y0)
                }
                Err(e) => format!("error {e:#}"),
            },
            "pencil-motion" | "motion" => match PencilMotion::from_str(arg, true) {
                Ok(m) => {
                    input::set_pencil_motion(m);
                    format!("ok {arg}")
                }
                Err(e) => format!("error {e}"),
            },
            "pencil-sensitivity" | "sensitivity" => match arg.parse::<f64>() {
                Ok(v) if (0.05..=10.0).contains(&v) => {
                    input::set_pencil_sensitivity(v);
                    format!("ok {v}")
                }
                _ => "error sensitivity must be between 0.05 and 10".into(),
            },
            "pencil-mode" | "mode" => match PencilMode::from_str(arg, true) {
                Ok(m) => {
                    input::set_pencil_mode(m);
                    format!("ok {arg}")
                }
                Err(e) => format!("error {e}"),
            },
            "status" => {
                let a = input::current_pencil_area();
                format!(
                    "ok area {}x{} at {}x{} mode {:?} motion {:?} sensitivity {:.2}",
                    a.w,
                    a.h,
                    a.x0,
                    a.y0,
                    input::current_pencil_mode(),
                    input::current_pencil_motion(),
                    input::current_pencil_sensitivity()
                )
            }
            _ => format!("error unknown command {cmd:?}"),
        };
        writeln!(out, "{reply}").ok();
        out.flush().ok();
    }
    Ok(())
}
