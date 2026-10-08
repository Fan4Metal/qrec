//! The area recorded: a rectangle in the physical pixels of the virtual
//! screen (Windows' coordinate space over all monitors), and the geometry
//! that keeps it inside one monitor and acceptable to the encoder.

/// A rectangle by its edges, in physical pixels; `right` and `bottom`
/// are exclusive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn width(&self) -> i32 {
        self.right - self.left
    }

    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }

    /// The common part of the two rectangles, if any.
    pub fn intersect(&self, o: &Rect) -> Option<Rect> {
        let r = Rect {
            left: self.left.max(o.left),
            top: self.top.max(o.top),
            right: self.right.min(o.right),
            bottom: self.bottom.min(o.bottom),
        };
        (r.width() > 0 && r.height() > 0).then_some(r)
    }
}

/// The recorded area: its top-left corner and size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Region {
    /// Smallest side the encoders accept comfortably.
    pub const MIN_SIDE: u32 = 64;

    pub fn from_rect(r: Rect) -> Region {
        Region { x: r.left, y: r.top, width: r.width().max(0) as u32, height: r.height().max(0) as u32 }
    }

    pub fn rect(&self) -> Rect {
        Rect { left: self.x, top: self.y, right: self.x + self.width as i32, bottom: self.y + self.height as i32 }
    }

    /// The rectangle between two corners dragged with the mouse, in any
    /// order; the pixel under the pointer at the end is included.
    pub fn from_drag(a: (i32, i32), b: (i32, i32)) -> Region {
        let (left, right) = if a.0 <= b.0 { (a.0, b.0) } else { (b.0, a.0) };
        let (top, bottom) = if a.1 <= b.1 { (a.1, b.1) } else { (b.1, a.1) };
        Region::from_rect(Rect { left, top, right: right + 1, bottom: bottom + 1 })
    }

    /// The region cut down to `bounds` (one monitor) with even sides, as
    /// H.264 in NV12 needs; `None` when what is left is too small.
    pub fn fit(self, bounds: Rect) -> Option<Region> {
        let r = self.rect().intersect(&bounds)?;
        let mut region = Region::from_rect(r);
        region.width &= !1;
        region.height &= !1;
        (region.width >= Self::MIN_SIDE && region.height >= Self::MIN_SIDE).then_some(region)
    }

    /// The same region with its origin moved: into the pixels of the
    /// monitor whose top-left corner is `origin`.
    pub fn relative_to(&self, origin: (i32, i32)) -> Region {
        Region { x: self.x - origin.0, y: self.y - origin.1, ..*self }
    }

    /// As kept in the settings: `x,y,width,height`.
    pub fn to_setting(self) -> String {
        format!("{},{},{},{}", self.x, self.y, self.width, self.height)
    }

    pub fn from_setting(s: &str) -> Option<Region> {
        let mut parts = s.split(',').map(str::trim);
        let mut next = || parts.next()?.parse::<i64>().ok();
        let (x, y, w, h) = (next()?, next()?, next()?, next()?);
        if parts.next().is_some() || w <= 0 || h <= 0 || w > u32::MAX as i64 || h > u32::MAX as i64 {
            return None;
        }
        Some(Region { x: i32::try_from(x).ok()?, y: i32::try_from(y).ok()?, width: w as u32, height: h as u32 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Rect = Rect { left: 0, top: 0, right: 1920, bottom: 1080 };

    #[test]
    fn drag_in_any_direction() {
        let a = Region::from_drag((10, 20), (109, 219));
        let b = Region::from_drag((109, 219), (10, 20));
        assert_eq!(a, b);
        assert_eq!(a, Region { x: 10, y: 20, width: 100, height: 200 });
    }

    #[test]
    fn fit_clamps_and_evens() {
        let r = Region { x: -10, y: 5, width: 101, height: 1101 };
        assert_eq!(r.fit(SCREEN), Some(Region { x: 0, y: 5, width: 90, height: 1074 }));
        assert_eq!(Region { x: 0, y: 0, width: 63, height: 500 }.fit(SCREEN), None);
        assert_eq!(Region { x: 3000, y: 0, width: 100, height: 100 }.fit(SCREEN), None);
    }

    #[test]
    fn setting_round_trip() {
        let r = Region { x: -1920, y: 40, width: 1280, height: 720 };
        assert_eq!(Region::from_setting(&r.to_setting()), Some(r));
        assert_eq!(Region::from_setting("1,2,3"), None);
        assert_eq!(Region::from_setting("1,2,0,3"), None);
        assert_eq!(Region::from_setting("a,b,c,d"), None);
    }

    #[test]
    fn relative_origin() {
        let r = Region { x: 2000, y: 100, width: 10, height: 10 };
        assert_eq!(r.relative_to((1920, 0)), Region { x: 80, y: 100, width: 10, height: 10 });
    }
}
