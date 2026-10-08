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

/// The proportions of the area: free, or a fixed ratio, kept within a
/// pixel with even sides.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Aspect {
    #[default]
    Free,
    Wide,
    Square,
}

impl Aspect {
    pub const ALL: [Aspect; 3] = [Aspect::Free, Aspect::Wide, Aspect::Square];

    /// The ratio as width to height; `None` when free.
    pub fn ratio(self) -> Option<(i64, i64)> {
        match self {
            Aspect::Free => None,
            Aspect::Wide => Some((16, 9)),
            Aspect::Square => Some((1, 1)),
        }
    }

    /// Name in the settings, also shown in the window.
    pub fn name(self) -> &'static str {
        match self {
            Aspect::Free => "free",
            Aspect::Wide => "16:9",
            Aspect::Square => "1:1",
        }
    }

    pub fn from_name(name: &str) -> Option<Aspect> {
        Self::ALL.into_iter().find(|a| a.name() == name)
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

    /// The rectangle dragged from `start` to `end` on the monitor
    /// `bounds`. Free, it is the rectangle between the corners. With a
    /// fixed ratio the corner at `start` stays, and the width follows the
    /// pointer along the side it went farther, in steps of 2 pixels, with
    /// the height of the ratio (`height_for`), as large as fits on the
    /// monitor from that corner. `fit` then gives what is recorded, or
    /// `None` when it is too small.
    pub fn drag(start: (i32, i32), end: (i32, i32), aspect: Aspect, bounds: Rect) -> Region {
        let Some(ratio) = aspect.ratio() else { return Region::from_drag(start, end) };
        let (n, d) = ratio;
        let (dx, dy) = (i64::from(end.0) - i64::from(start.0), i64::from(end.1) - i64::from(start.1));
        let (right, down) = (dx >= 0, dy >= 0);
        // The width asked for by the pointer, the pixel under it included,
        // made even.
        let asked = (dx.abs() + 1).max(((dy.abs() + 1) * n + d / 2) / d);
        let asked = (asked + 1) & !1;
        // Room from the corner to the edges of the monitor it points to.
        let room_w = if right { i64::from(bounds.right) - i64::from(start.0) } else { i64::from(start.0) + 1 - i64::from(bounds.left) };
        let room_h = if down { i64::from(bounds.bottom) - i64::from(start.1) } else { i64::from(start.1) + 1 - i64::from(bounds.top) };
        let w = asked.min(widest(room_w, room_h, ratio));
        let h = height_for(w, ratio);
        let x = if right { i64::from(start.0) } else { i64::from(start.0) + 1 - w };
        let y = if down { i64::from(start.1) } else { i64::from(start.1) + 1 - h };
        Region { x: x as i32, y: y as i32, width: w as u32, height: h as u32 }
    }

    /// The region brought to `aspect` around its centre: the largest area
    /// of the ratio it holds, at least the smallest one allowed, moved
    /// inside `bounds` when that makes it larger. Free leaves it as it is;
    /// `None` when the monitor is too small for the ratio.
    pub fn with_aspect(self, aspect: Aspect, bounds: Rect) -> Option<Region> {
        let Some(ratio) = aspect.ratio() else { return Some(self) };
        let min = i64::from(Self::MIN_SIDE);
        // The narrowest width whose height is not below the smallest side.
        let mut least = min;
        while height_for(least, ratio) < min {
            least += 2;
        }
        let most = widest(i64::from(bounds.width()), i64::from(bounds.height()), ratio);
        if most < least {
            return None;
        }
        let w = widest(i64::from(self.width), i64::from(self.height), ratio).clamp(least, most);
        let h = height_for(w, ratio);
        let cx = i64::from(self.x) + i64::from(self.width) / 2;
        let cy = i64::from(self.y) + i64::from(self.height) / 2;
        let x = (cx - w / 2).clamp(i64::from(bounds.left), i64::from(bounds.right) - w);
        let y = (cy - h / 2).clamp(i64::from(bounds.top), i64::from(bounds.bottom) - h);
        Some(Region { x: x as i32, y: y as i32, width: w as u32, height: h as u32 })
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

/// The height of an area `width` wide (even) in the ratio `n:d`: rounded
/// to the nearest even number, as H.264 in NV12 needs. Off by less than a
/// pixel, and exact where the ratio allows it (1280×720, 1920×1080).
fn height_for(width: i64, (n, d): (i64, i64)) -> i64 {
    (width * d + n) / (2 * n) * 2
}

/// The widest even width of the ratio within `w` by `h`.
fn widest(w: i64, h: i64, ratio: (i64, i64)) -> i64 {
    let (n, d) = ratio;
    let mut width = w.min(h * n / d).max(0) & !1;
    while width > 0 && height_for(width, ratio) > h {
        width -= 2;
    }
    width
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
    fn free_drag_is_the_rectangle() {
        assert_eq!(Region::drag((10, 20), (109, 219), Aspect::Free, SCREEN), Region::from_drag((10, 20), (109, 219)));
    }

    #[test]
    fn wide_drag_in_any_direction() {
        // 1279 px to the right: 40 steps of 32, the height follows.
        let r = Region::drag((100, 100), (1378, 150), Aspect::Wide, SCREEN);
        assert_eq!(r, Region { x: 100, y: 100, width: 1280, height: 720 });
        // Up and to the left, the corner at the start kept.
        let r = Region::drag((1379, 819), (100, 769), Aspect::Wide, SCREEN);
        assert_eq!(r, Region { x: 100, y: 100, width: 1280, height: 720 });
        // The farther side wins: a tall drag gives the height.
        let r = Region::drag((0, 0), (10, 359), Aspect::Wide, SCREEN);
        assert_eq!((r.width, r.height), (640, 360));
        assert_eq!(r.fit(SCREEN), Some(r));
    }

    #[test]
    fn wide_drag_is_smooth() {
        // 1281 px: 2 more than 1280, the height rounded to an even 722.
        let r = Region::drag((0, 0), (1281, 10), Aspect::Wide, SCREEN);
        assert_eq!((r.width, r.height), (1282, 722));
        let r = Region::drag((0, 0), (1289, 10), Aspect::Wide, SCREEN);
        assert_eq!((r.width, r.height), (1290, 726));
        // Within a pixel of 16:9 at every even width.
        for w in (128..=1920).step_by(2) {
            let h = height_for(w, (16, 9));
            assert!(h % 2 == 0 && (h - w * 9 / 16).abs() <= 1, "{w}x{h}");
        }
    }

    #[test]
    fn wide_drag_stops_at_the_monitor() {
        // Room for 1600 px to the right but only 300 down.
        let r = Region::drag((320, 780), (1919, 1079), Aspect::Wide, SCREEN);
        assert_eq!(r, Region { x: 320, y: 780, width: 532, height: 300 });
        // The pointer past the edge changes nothing.
        assert_eq!(Region::drag((320, 780), (5000, 5000), Aspect::Wide, SCREEN), r);
    }

    #[test]
    fn square_drag_and_the_smallest() {
        // 101 px up and to the left round to 102, ending at the start.
        let r = Region::drag((500, 500), (400, 450), Aspect::Square, SCREEN);
        assert_eq!(r, Region { x: 399, y: 399, width: 102, height: 102 });
        // Too small to record: fit refuses it.
        assert_eq!(Region::drag((500, 500), (520, 520), Aspect::Square, SCREEN).fit(SCREEN), None);
        assert_eq!(Region::drag((500, 500), (500, 500), Aspect::Wide, SCREEN).fit(SCREEN), None);
    }

    #[test]
    fn existing_area_takes_the_ratio() {
        // 1000×1000 around (500, 500): the full width, 562 high.
        let r = Region { x: 0, y: 0, width: 1000, height: 1000 }.with_aspect(Aspect::Wide, SCREEN).unwrap();
        assert_eq!(r, Region { x: 0, y: 219, width: 1000, height: 562 });
        // Too narrow for 16:9 at the smallest: grown around the centre.
        let r = Region { x: 0, y: 0, width: 64, height: 400 }.with_aspect(Aspect::Wide, SCREEN).unwrap();
        assert_eq!(r, Region { x: 0, y: 168, width: 112, height: 64 });
        let r = Region { x: 1800, y: 0, width: 120, height: 300 }.with_aspect(Aspect::Square, SCREEN).unwrap();
        assert_eq!(r, Region { x: 1800, y: 90, width: 120, height: 120 });
        let free = Region { x: 1, y: 2, width: 301, height: 77 };
        assert_eq!(free.with_aspect(Aspect::Free, SCREEN), Some(free));
    }

    #[test]
    fn aspect_names() {
        for a in Aspect::ALL {
            assert_eq!(Aspect::from_name(a.name()), Some(a));
        }
        assert_eq!(Aspect::from_name("4:3"), None);
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
