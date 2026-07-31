//! Screen capture.
//!
//! Two shapes, for two different reasons.
//!
//! Unfrozen: select the region FIRST, then grab only that region. Grabbing the
//! whole desktop and cropping afterwards costs ~420ms on a multi-monitor setup
//! because of PNG encoding; a region grab to PPM is ~42ms.
//!
//! Frozen: the screen is held still while you drag, so a video, a scrolling
//! page or a hover tooltip cannot move out from under the selection. The grab
//! then has to happen while the freeze is still up, otherwise the region is
//! taken from a screen that has already moved on.

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{ensure, Context, Result};

/// A region in compositor output space, in slurp's own `X,Y WxH` notation.
/// Kept as an opaque string so it round-trips to grim without reformatting.
pub struct Region(String);

impl Region {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Accept a region given on the command line instead of dragged, in the
    /// same `X,Y WxH` form slurp emits. Lets the OCR path be scripted, tested,
    /// and re-run against a fixed area (subtitles, a lecture slide).
    pub fn parse(spec: &str) -> Result<Self> {
        let spec = spec.trim();
        let (origin, size) = spec
            .split_once(' ')
            .with_context(|| format!("expected geometry as 'X,Y WxH', got '{spec}'"))?;

        let (x, y) = origin
            .split_once(',')
            .with_context(|| format!("expected origin as 'X,Y', got '{origin}'"))?;
        let (w, h) = size
            .split_once('x')
            .with_context(|| format!("expected size as 'WxH', got '{size}'"))?;

        for (label, value) in [("x", x), ("y", y), ("width", w), ("height", h)] {
            value
                .parse::<i32>()
                .with_context(|| format!("{label} is not a number in '{spec}'"))?;
        }

        Ok(Region(spec.to_string()))
    }
}

/// Drag a region and capture it. `Ok(None)` means the drag was cancelled.
///
/// When `freeze` is set the screen is held still for the duration, and the grab
/// happens before it is released - so what you selected is what you get, even
/// if the underlying window was playing a video.
pub fn interactive(freeze: bool) -> Result<Option<Vec<u8>>> {
    let frozen = if freeze { Frozen::start() } else { None };

    let Some(region) = select_region()? else {
        return Ok(None);
    };

    // Still inside the freeze: grim reads what is on screen, which is the held
    // image. Dropping `frozen` after this is what releases it.
    let image = grab(&region);
    drop(frozen);

    image.map(Some)
}

/// Ask the user to drag a region. `Ok(None)` means they cancelled (Esc).
pub fn select_region() -> Result<Option<Region>> {
    slurp(&[], None)
}

/// Ask the user to pick one of `boxes` rather than drag freely.
///
/// `slurp -r` snaps the selection to whichever predefined box is under the
/// pointer, which is how window picking works without any compositor-specific
/// picker: hand it the window rectangles and let it do the highlighting.
pub fn select_from(boxes: &[String]) -> Result<Option<Region>> {
    ensure!(!boxes.is_empty(), "nothing on screen to pick");
    slurp(&["-r"], Some(boxes.join("\n")))
}

fn slurp(args: &[&str], stdin_boxes: Option<String>) -> Result<Option<Region>> {
    let mut command = Command::new("slurp");
    command.args(args).stdout(Stdio::piped());

    if stdin_boxes.is_some() {
        command.stdin(Stdio::piped());
    }

    let mut child = command
        .spawn()
        .context("could not run `slurp` - is it installed?")?;

    if let Some(boxes) = stdin_boxes {
        child
            .stdin
            .as_mut()
            .context("slurp stdin unavailable")?
            .write_all(boxes.as_bytes())?;
    }

    let out = child.wait_with_output()?;

    // slurp exits non-zero on cancel, which is not an error for us.
    if !out.status.success() {
        return Ok(None);
    }

    let geom = String::from_utf8(out.stdout)
        .context("slurp returned non-utf8")?
        .trim()
        .to_string();

    Ok(if geom.is_empty() {
        None
    } else {
        Some(Region(geom))
    })
}

/// Capture one region straight to memory as PPM.
///
/// PPM is uncompressed, which is exactly what we want: leptonica decodes it
/// natively and we skip the PNG encode/decode round trip entirely.
pub fn grab(region: &Region) -> Result<Vec<u8>> {
    let out = Command::new("grim")
        .args(["-t", "ppm", "-g", region.as_str(), "-"])
        .output()
        .context("could not run `grim` - is it installed?")?;

    ensure!(
        out.status.success(),
        "grim failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    ensure!(!out.stdout.is_empty(), "grim produced an empty capture");

    Ok(out.stdout)
}

/// Capture a whole output as PNG. Screenshots want a real image format, unlike
/// the OCR path where PPM saves an encode/decode round trip nobody sees.
pub fn grab_output_png(output: &str) -> Result<Vec<u8>> {
    run_grim(&["-o", output, "-"])
}

/// Capture one region as PNG.
pub fn grab_png(region: &Region) -> Result<Vec<u8>> {
    run_grim(&["-g", region.as_str(), "-"])
}

fn run_grim(args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("grim")
        .args(args)
        .output()
        .context("could not run `grim` - is it installed?")?;

    ensure!(
        out.status.success(),
        "grim failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    ensure!(!out.stdout.is_empty(), "grim produced an empty capture");

    Ok(out.stdout)
}

/// A held screen, released when dropped.
///
/// Implemented with hyprpicker, which is what grimblast uses for the same job:
/// it paints a static copy of every output and sits under slurp's selection
/// surface. `-z` drops its zoom lens and `-d` its colour preview, both of which
/// would otherwise be painted into the captured image.
pub struct Frozen(Child);

impl Frozen {
    pub fn start() -> Option<Self> {
        let child = Command::new("hyprpicker")
            .args(["-r", "-z", "-d", "-q"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        // The frozen surface needs to be mapped before slurp draws over it.
        // Racing it shows the live screen for the first frames of the drag.
        std::thread::sleep(Duration::from_millis(200));

        Some(Self(child))
    }
}

impl Drop for Frozen {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
