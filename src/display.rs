//! The monitors, as DXGI lists them: which adapter and output each one is,
//! and where it lies on the virtual screen.

use windows::Win32::Graphics::Dxgi::Common::DXGI_MODE_ROTATION_IDENTITY;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIFactory1};

use crate::region::Rect;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Monitor {
    /// Index of the adapter in DXGI's enumeration.
    pub adapter: u32,
    /// Index of the output on that adapter.
    pub output: u32,
    /// Windows' name of the output, `\\.\DISPLAY1`; identifies the
    /// monitor between runs.
    pub device_name: String,
    /// Where the monitor lies on the virtual screen, physical pixels.
    pub rect: Rect,
    pub primary: bool,
    /// Rotated displays are captured in their native orientation, which
    /// the program does not turn yet.
    pub rotated: bool,
}

impl Monitor {
    pub fn width(&self) -> u32 {
        self.rect.width().max(0) as u32
    }

    pub fn height(&self) -> u32 {
        self.rect.height().max(0) as u32
    }

    /// As listed in the window: "Display 2 (2560×1440)".
    pub fn label(&self, index: usize) -> String {
        let name = tr!("Display", "Экран");
        let main = if self.primary { tr!(", main", ", основной") } else { "" };
        format!("{name} {} ({}×{}{main})", index + 1, self.width(), self.height())
    }
}

/// The monitors attached to the desktop, the primary one first.
pub fn monitors() -> Vec<Monitor> {
    let mut list = Vec::new();
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else {
        return list;
    };
    for a in 0.. {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(a) }) else { break };
        if let Ok(desc) = unsafe { adapter.GetDesc1() }
            && desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0
        {
            continue;
        }
        for o in 0.. {
            let Ok(output) = (unsafe { adapter.EnumOutputs(o) }) else { break };
            let Ok(desc) = (unsafe { output.GetDesc() }) else { continue };
            if !desc.AttachedToDesktop.as_bool() {
                continue;
            }
            let r = desc.DesktopCoordinates;
            let rect = Rect { left: r.left, top: r.top, right: r.right, bottom: r.bottom };
            let end = desc.DeviceName.iter().position(|&c| c == 0).unwrap_or(desc.DeviceName.len());
            list.push(Monitor {
                adapter: a,
                output: o,
                device_name: String::from_utf16_lossy(&desc.DeviceName[..end]),
                rect,
                primary: r.left == 0 && r.top == 0,
                rotated: desc.Rotation != DXGI_MODE_ROTATION_IDENTITY,
            });
        }
    }
    list.sort_by_key(|m| (!m.primary, m.rect.left, m.rect.top));
    list
}

/// The monitor that holds the point.
pub fn monitor_at(monitors: &[Monitor], x: i32, y: i32) -> Option<&Monitor> {
    monitors.iter().find(|m| m.rect.contains(x, y))
}

/// The monitor most of `rect` lies on.
pub fn monitor_of<'a>(monitors: &'a [Monitor], rect: &Rect) -> Option<&'a Monitor> {
    monitors
        .iter()
        .filter_map(|m| m.rect.intersect(rect).map(|i| (i.width() as i64 * i.height() as i64, m)))
        .max_by_key(|(area, _)| *area)
        .map(|(_, m)| m)
}
