//! The returned image is the coordinate authority, not the monitor's physical resolution.
use crate::types::{Crop, Point};
use anyhow::{Result, bail, ensure};

#[derive(Debug, Clone)]
pub struct FrameMapping {
    pub frame_id: String,
    pub display: usize,
    pub node_id: u32,
    pub source_size: (u32, u32),
    pub image_size: (u32, u32),
    pub logical_size: Option<(u32, u32)>,
    pub crop: Crop,
}

impl FrameMapping {
    pub fn point(&self, p: Point) -> Result<Point> {
        ensure!(
            p.x.is_finite() && p.y.is_finite(),
            "Coordinates must be finite"
        );
        let (iw, ih) = self.image_size;
        ensure!(
            p.x >= 0.0 && p.y >= 0.0 && p.x < iw as f64 && p.y < ih as f64,
            "Point ({}, {}) is outside the {}×{} returned image",
            p.x,
            p.y,
            iw,
            ih
        );
        // Missing logical geometry does not prevent observation. It does prevent an honest
        // absolute-input mapping: assuming scale=1 would click the wrong UI at fractional scale.
        let Some((lw, lh)) = self.logical_size else {
            bail!(
                "Portal did not provide logical display size; absolute input is unavailable, but keyboard input and observation still work"
            )
        };
        let px = self.crop.x as f64 + p.x * self.crop.width as f64 / iw as f64;
        let py = self.crop.y as f64 + p.y * self.crop.height as f64 / ih as f64;
        Ok(Point {
            x: px * lw as f64 / self.source_size.0 as f64,
            y: py * lh as f64 / self.source_size.1 as f64,
        })
    }
}

pub fn checked_crop(size: (u32, u32), crop: Option<Crop>) -> Result<Crop> {
    let (w, h) = size;
    ensure!(w > 0 && h > 0, "Capture has empty dimensions");
    let c = crop.unwrap_or(Crop {
        x: 0,
        y: 0,
        width: w,
        height: h,
    });
    ensure!(
        c.width > 0 && c.height > 0,
        "Crop dimensions must be positive"
    );
    ensure!(
        c.x.checked_add(c.width).is_some_and(|v| v <= w)
            && c.y.checked_add(c.height).is_some_and(|v| v <= h),
        "Crop is outside the captured image"
    );
    Ok(c)
}

pub fn output_size(c: Crop, max: u32) -> (u32, u32) {
    if max == 0 || c.width.max(c.height) <= max {
        return (c.width, c.height);
    }
    let ratio = max as f64 / c.width.max(c.height) as f64;
    (
        (c.width as f64 * ratio).round().max(1.0) as u32,
        (c.height as f64 * ratio).round().max(1.0) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn mapping() -> FrameMapping {
        FrameMapping {
            frame_id: "x".into(),
            display: 0,
            node_id: 1,
            source_size: (3840, 2160),
            image_size: (960, 540),
            logical_size: Some((2560, 1440)),
            crop: Crop {
                x: 960,
                y: 540,
                width: 1920,
                height: 1080,
            },
        }
    }
    #[test]
    fn maps_crop_resize_and_fractional_scale() {
        let p = mapping().point(Point { x: 480.0, y: 270.0 }).unwrap();
        assert_eq!((p.x, p.y), (1280.0, 720.0));
    }
    #[test]
    fn rejects_nonfinite_and_outside() {
        for p in [
            Point {
                x: f64::NAN,
                y: 0.0,
            },
            Point { x: -1.0, y: 0.0 },
            Point { x: 960.0, y: 0.0 },
            Point {
                x: 0.0,
                y: f64::INFINITY,
            },
        ] {
            assert!(mapping().point(p).is_err());
        }
    }
    #[test]
    fn no_guessed_scale() {
        let mut m = mapping();
        m.logical_size = None;
        assert!(m.point(Point { x: 1.0, y: 1.0 }).is_err());
    }
    #[test]
    fn validates_crop_without_overflow() {
        assert!(
            checked_crop(
                (10, 10),
                Some(Crop {
                    x: u32::MAX,
                    y: 0,
                    width: 2,
                    height: 1
                })
            )
            .is_err()
        );
        assert!(
            checked_crop(
                (10, 10),
                Some(Crop {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 1
                })
            )
            .is_err()
        );
        assert_eq!(
            output_size(
                Crop {
                    x: 0,
                    y: 0,
                    width: 3840,
                    height: 2160
                },
                1600
            ),
            (1600, 900)
        );
    }
}
