// Modified by Vulkgryph LLC in 2026 for the Forge project.
//
// Change: added `ActiveEventLoopExtMacOS::set_dock_menu`, so an application can
// supply the menu shown when its Dock icon is right-clicked. AppKit asks only
// the application delegate for that menu (`applicationDockMenu:`) and winit
// owns the delegate, so upstream provides no way to offer one. The patch
// carries a menu through the event loop to the delegate and returns it there.
//
// This notice is required by the Apache License 2.0, section 4(b), which the
// original carries; the original licence is at ../../../LICENSE.
#[macro_use]
mod util;

mod app;
pub(crate) mod app_state;
mod cursor;
mod event;
mod event_handler;
mod event_loop;
mod ffi;
mod menu;
mod monitor;
mod observer;
mod view;
mod window;
mod window_delegate;

use std::fmt;

pub(crate) use self::event::{physicalkey_to_scancode, scancode_to_physicalkey, KeyEventExtra};
pub(crate) use self::event_loop::{
    ActiveEventLoop, EventLoop, EventLoopProxy, OwnedDisplayHandle,
    PlatformSpecificEventLoopAttributes,
};
pub(crate) use self::monitor::{MonitorHandle, VideoModeHandle};
pub(crate) use self::window::WindowId;
pub(crate) use self::window_delegate::PlatformSpecificWindowAttributes;
use crate::event::DeviceId as RootDeviceId;

pub(crate) use self::cursor::CustomCursor as PlatformCustomCursor;
pub(crate) use self::window::Window;
pub(crate) use crate::cursor::OnlyCursorImageSource as PlatformCustomCursorSource;
pub(crate) use crate::icon::NoIcon as PlatformIcon;
pub(crate) use crate::platform_impl::Fullscreen;

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceId;

impl DeviceId {
    pub const fn dummy() -> Self {
        DeviceId
    }
}

// Constant device ID; to be removed when if backend is updated to report real device IDs.
pub(crate) const DEVICE_ID: RootDeviceId = RootDeviceId(DeviceId);

#[derive(Debug)]
pub enum OsError {
    CGError(core_graphics::base::CGError),
    CreationError(&'static str),
}

impl fmt::Display for OsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OsError::CGError(e) => f.pad(&format!("CGError {e}")),
            OsError::CreationError(e) => f.pad(e),
        }
    }
}
