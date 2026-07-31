//! Screenshots.
//!
//! Same capture primitives as the OCR path, different destination: a real PNG
//! that goes to the clipboard and to disk, rather than straight into tesseract.
//!
//! Window picking has no compositor-specific picker in it. The compositor is
//! only asked *where the windows are*; `slurp -r` does the highlighting and
//! snapping. That keeps the Wayland-portable story intact - adding sway or niri
//! means one more function returning rectangles, nothing else.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::capture::{self, Frozen};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Region,
    Window,
    Screen,
}

impl std::str::FromStr for Mode {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        Ok(match value {
            "region" | "area" => Mode::Region,
            "window" | "active" => Mode::Window,
            "screen" | "output" | "full" => Mode::Screen,
            other => bail!("unknown mode '{other}' (expected region, window or screen)"),
        })
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Mode::Region => "region",
            Mode::Window => "window",
            Mode::Screen => "screen",
        })
    }
}

pub struct Shot {
    pub png: Vec<u8>,
    pub saved: Option<PathBuf>,
}

/// A frozen output, ready to pick a region out of.
#[derive(Debug, Clone)]
pub struct Capture {
    /// PNG of the whole focused output.
    pub png: Vec<u8>,
    /// Image pixels per logical point on that output.
    pub scale: f64,
    /// Where the selection should start, in image pixels. `None` means start
    /// with nothing selected and let the drag define it.
    pub preset: Option<(u32, u32, u32, u32)>,
}

/// Grab the focused output whole, plus wherever the selection should start.
///
/// Nothing is frozen with hyprpicker here: the capture *is* the freeze. From
/// this point on the overlay shows a still image, so the selection can be
/// adjusted for as long as you like without the screen moving underneath.
pub fn capture_for_overlay(mode: Mode) -> Result<Capture> {
    let monitor = focused_monitor()?;
    let png = capture::grab_output_png(&monitor.name)?;
    let scale = monitor.scale.max(0.1);

    let preset = match mode {
        Mode::Region => None,
        Mode::Screen => {
            let (width, height) = png_dimensions(&png).unwrap_or((0, 0));
            (width > 0 && height > 0).then_some((0, 0, width, height))
        }
        Mode::Window => focused_window_in(&monitor).map(|(x, y, w, h)| {
            (
                (x as f64 * scale) as u32,
                (y as f64 * scale) as u32,
                (w as f64 * scale) as u32,
                (h as f64 * scale) as u32,
            )
        }),
    };

    Ok(Capture { png, scale, preset })
}

/// Cut a region out of a captured PNG and re-encode it.
pub fn crop(png: &[u8], x: u32, y: u32, width: u32, height: u32) -> Result<Vec<u8>> {
    use std::io::Cursor;

    let full = image::load_from_memory(png).context("could not decode the capture")?;
    let cut = full.crop_imm(x, y, width, height);

    let mut out = Vec::new();
    cut.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
        .context("could not encode the cropped capture")?;

    Ok(out)
}

/// Width and height read straight out of the PNG header - the IHDR fields sit
/// at a fixed offset, so this is a read rather than a decode.
pub fn png_dimensions(png: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

    if png.len() < 24 || &png[..8] != SIGNATURE || &png[12..16] != b"IHDR" {
        return None;
    }

    Some((
        u32::from_be_bytes(png[16..20].try_into().ok()?),
        u32::from_be_bytes(png[20..24].try_into().ok()?),
    ))
}

pub struct Monitor {
    pub name: String,
    pub scale: f64,
    pub x: i64,
    pub y: i64,
}

fn focused_monitor() -> Result<Monitor> {
    let monitors: Value = hyprctl(&["monitors", "-j"])?;

    let monitor = monitors
        .as_array()
        .context("hyprctl monitors: not a list")?
        .iter()
        .find(|monitor| monitor.get("focused").and_then(Value::as_bool) == Some(true))
        .context("no focused output reported")?;

    Ok(Monitor {
        name: monitor
            .get("name")
            .and_then(Value::as_str)
            .context("output has no name")?
            .to_string(),
        scale: monitor.get("scale").and_then(Value::as_f64).unwrap_or(1.0),
        x: monitor.get("x").and_then(Value::as_i64).unwrap_or(0),
        y: monitor.get("y").and_then(Value::as_i64).unwrap_or(0),
    })
}

/// The focused window, in coordinates local to `monitor`.
fn focused_window_in(monitor: &Monitor) -> Option<(i64, i64, i64, i64)> {
    let active: Value = hyprctl(&["activewindow", "-j"]).ok()?;

    let x = active.pointer("/at/0").and_then(Value::as_i64)?;
    let y = active.pointer("/at/1").and_then(Value::as_i64)?;
    let width = active.pointer("/size/0").and_then(Value::as_i64)?;
    let height = active.pointer("/size/1").and_then(Value::as_i64)?;

    (width > 0 && height > 0).then_some((x - monitor.x, y - monitor.y, width, height))
}

/// Take a screenshot. `Ok(None)` means the selection was cancelled.
pub fn take(mode: Mode, freeze: bool, save: bool) -> Result<Option<Shot>> {
    let Some(png) = capture(mode, freeze)? else {
        return Ok(None);
    };

    copy_image(&png)?;

    let saved = if save { Some(self::save(&png)?) } else { None };

    Ok(Some(Shot { png, saved }))
}

pub fn capture(mode: Mode, freeze: bool) -> Result<Option<Vec<u8>>> {
    // Only the interactive modes need the screen held still, and as with OCR
    // the grab has to happen before the freeze is released.
    let frozen = match mode {
        Mode::Screen => None,
        _ if freeze => Frozen::start(),
        _ => None,
    };

    let region = match mode {
        Mode::Screen => {
            let png = capture::grab_output_png(&focused_output()?);
            drop(frozen);
            return png.map(Some);
        }
        Mode::Region => capture::select_region()?,
        Mode::Window => capture::select_from(&visible_windows()?)?,
    };

    let Some(region) = region else {
        return Ok(None); // cancelled
    };

    let png = capture::grab_png(&region);
    drop(frozen);

    png.map(Some)
}

/// Rectangles of the windows you can actually see right now.
///
/// Filtered to the workspaces currently displayed: offering a window on some
/// other workspace would let you "pick" something invisible and capture
/// whatever happens to be in that screen area instead.
fn visible_windows() -> Result<Vec<String>> {
    let shown = active_workspaces()?;
    let clients: Value = hyprctl(&["clients", "-j"])?;

    let mut boxes = Vec::new();

    for client in clients.as_array().context("hyprctl clients: not a list")? {
        let width = client.pointer("/size/0").and_then(Value::as_i64).unwrap_or(0);
        let height = client.pointer("/size/1").and_then(Value::as_i64).unwrap_or(0);
        let workspace = client
            .pointer("/workspace/id")
            .and_then(Value::as_i64)
            .unwrap_or(i64::MIN);

        let mapped = client.get("mapped").and_then(Value::as_bool).unwrap_or(false);
        let hidden = client.get("hidden").and_then(Value::as_bool).unwrap_or(false);

        if !mapped || hidden || width <= 0 || height <= 0 || !shown.contains(&workspace) {
            continue;
        }

        let x = client.pointer("/at/0").and_then(Value::as_i64).unwrap_or(0);
        let y = client.pointer("/at/1").and_then(Value::as_i64).unwrap_or(0);

        boxes.push(format!("{x},{y} {width}x{height}"));
    }

    Ok(boxes)
}

fn active_workspaces() -> Result<Vec<i64>> {
    let monitors: Value = hyprctl(&["monitors", "-j"])?;

    Ok(monitors
        .as_array()
        .context("hyprctl monitors: not a list")?
        .iter()
        .filter_map(|monitor| monitor.pointer("/activeWorkspace/id").and_then(Value::as_i64))
        .collect())
}

fn focused_output() -> Result<String> {
    let monitors: Value = hyprctl(&["monitors", "-j"])?;

    monitors
        .as_array()
        .context("hyprctl monitors: not a list")?
        .iter()
        .find(|monitor| monitor.get("focused").and_then(Value::as_bool) == Some(true))
        .and_then(|monitor| monitor.get("name").and_then(Value::as_str))
        .map(str::to_owned)
        .context("no focused output reported")
}

fn hyprctl(args: &[&str]) -> Result<Value> {
    let out = Command::new("hyprctl")
        .args(args)
        .output()
        .context("could not run `hyprctl` - window and screen modes need Hyprland")?;

    anyhow::ensure!(out.status.success(), "hyprctl {} failed", args.join(" "));

    serde_json::from_slice(&out.stdout).context("hyprctl returned invalid json")
}

pub fn copy_image(png: &[u8]) -> Result<()> {
    use std::io::Write;

    let mut child = Command::new("wl-copy")
        .args(["--type", "image/png"])
        .stdin(Stdio::piped())
        .spawn()
        .context("could not run `wl-copy` - is wl-clipboard installed?")?;

    child
        .stdin
        .as_mut()
        .context("wl-copy stdin unavailable")?
        .write_all(png)?;

    child.wait()?;
    Ok(())
}

/// Write a captured PNG out and return where it went.
pub fn save(png: &[u8]) -> Result<PathBuf> {
    let path = destination()?;
    std::fs::write(&path, png).with_context(|| format!("could not write {}", path.display()))?;
    Ok(path)
}

/// `<pictures>/Screenshots/Screenshot_<timestamp>.png`.
fn destination() -> Result<PathBuf> {
    let dir = pictures_dir().join("Screenshots");

    std::fs::create_dir_all(&dir)
        .with_context(|| format!("could not create {}", dir.display()))?;

    Ok(dir.join(format!("Screenshot_{}.png", timestamp())))
}

fn pictures_dir() -> PathBuf {
    // xdg-user-dir knows about localised directory names; falling back to a
    // hardcoded "Pictures" would be wrong on an Italian-locale desktop.
    if let Ok(out) = Command::new("xdg-user-dir").arg("PICTURES").output() {
        if out.status.success() {
            let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !path.is_empty() && Path::new(&path).is_dir() {
                return PathBuf::from(path);
            }
        }
    }

    home().join("Pictures")
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Shelled out rather than pulling in a date crate for one format string, in
/// keeping with the rest of the tool composing grim/slurp/wl-clipboard.
fn timestamp() -> String {
    Command::new("date")
        .arg("+%Y-%m-%d_%H-%M-%S")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|stamp| !stamp.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::Mode;

    #[test]
    fn parses_the_modes_and_their_aliases() {
        assert_eq!("region".parse::<Mode>().unwrap(), Mode::Region);
        assert_eq!("area".parse::<Mode>().unwrap(), Mode::Region);
        assert_eq!("window".parse::<Mode>().unwrap(), Mode::Window);
        assert_eq!("screen".parse::<Mode>().unwrap(), Mode::Screen);
        assert_eq!("full".parse::<Mode>().unwrap(), Mode::Screen);
    }

    #[test]
    fn rejects_an_unknown_mode() {
        assert!("everything".parse::<Mode>().is_err());
    }
}
