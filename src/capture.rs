//! Screen capture.
//!
//! Deliberately two steps: select the region FIRST, then grab only that region.
//! Grabbing the whole desktop and cropping afterwards costs ~420ms on a
//! multi-monitor setup because of PNG encoding; a region grab to PPM is ~42ms.

use anyhow::{ensure, Context, Result};
use std::process::Command;

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

/// Ask the user to drag a region. `Ok(None)` means they cancelled (Esc).
pub fn select_region() -> Result<Option<Region>> {
    let out = Command::new("slurp")
        .output()
        .context("could not run `slurp` - is it installed?")?;

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
