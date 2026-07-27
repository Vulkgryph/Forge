//! macOS Dock-icon context menu.
//!
//! Right-clicking (or long-pressing) the Dock icon shows a menu that AppKit
//! builds from the application delegate's `applicationDockMenu:`. Everything
//! below that line — "Options", "Show in Finder", the window list, "Quit" — is
//! added by the system; this module only contributes the custom items above it.
//!
//! Two wrinkles shape the implementation:
//!
//!  * AppKit asks the *app delegate* for the menu, and winit owns the delegate,
//!    so upstream winit gives an application no way to supply one. The vendored
//!    winit carries a small patch adding `applicationDockMenu:` plus
//!    `ActiveEventLoopExtMacOS::set_dock_menu`, which is what this uses.
//!
//!  * A menu item's click has to reach the event loop, which is busy sleeping
//!    and does not own this menu. So the item targets a tiny Objective-C class
//!    declared here; its action pushes onto a queue that `Ide::about_to_wait`
//!    drains, then wakes the loop the same way any background producer does.

#![cfg(target_os = "macos")]

use std::sync::Mutex;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{declare_class, msg_send_id, mutability, sel, ClassType, DeclaredClass};
use objc2_app_kit::{NSMenu, NSMenuItem};
use objc2_foundation::{ns_string, MainThreadMarker, NSObject, NSObjectProtocol};

/// Requests raised by Dock-menu clicks, drained by the event loop.
///
/// A queue rather than a direct call: the click arrives on the main thread
/// inside AppKit, which has no handle on the `Ide` state that owns the window
/// list. `Ide::about_to_wait` is the one place that legitimately does.
static PENDING: Mutex<Vec<DockRequest>> = Mutex::new(Vec::new());

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DockRequest {
    /// Open an additional window, same as "New Window" inside the app.
    NewWindow,
}

/// Take everything queued since the last call.
pub fn take_requests() -> Vec<DockRequest> {
    PENDING.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default()
}

fn push(req: DockRequest) {
    if let Ok(mut q) = PENDING.lock() {
        q.push(req);
    }
    // The loop is very likely asleep — a Dock click is not a window event, so
    // nothing else will wake it.
    crate::wake::wake();
}

declare_class!(
    /// Target for the Dock menu items. Exists only to turn a menu action into a
    /// `DockRequest`; holds no state.
    struct DockTarget;

    unsafe impl ClassType for DockTarget {
        type Super = NSObject;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "ForgeDockTarget";
    }

    impl DeclaredClass for DockTarget {}

    unsafe impl NSObjectProtocol for DockTarget {}

    unsafe impl DockTarget {
        #[method(forgeNewWindow:)]
        fn new_window(&self, _sender: Option<&AnyObject>) {
            push(DockRequest::NewWindow);
        }
    }
);

impl DockTarget {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        unsafe { msg_send_id![mtm.alloc::<Self>(), init] }
    }
}

/// Build the Dock menu.
///
/// The target is deliberately leaked: AppKit holds menu items with an
/// *unretained* target, and this menu lives for the whole process, so letting
/// the target drop would leave a dangling pointer for AppKit to message on the
/// next click.
pub fn build(mtm: MainThreadMarker) -> Retained<NSMenu> {
    let target = DockTarget::new(mtm);
    let target: &'static DockTarget = Box::leak(Box::new(target));

    let menu = NSMenu::new(mtm);
    add_item(mtm, &menu, "New Window", sel!(forgeNewWindow:), target);
    menu
}

fn add_item(
    mtm:    MainThreadMarker,
    menu:   &NSMenu,
    title:  &str,
    action: Sel,
    target: &DockTarget,
) {
    // `ns_string!` needs a literal, so map the small fixed set by hand rather
    // than allocating an NSString per call.
    let title = match title {
        "New Window" => ns_string!("New Window"),
        other => {
            debug_assert!(false, "dock item {other:?} has no NSString literal");
            return;
        }
    };
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc(), title, Some(action), ns_string!(""),
        )
    };
    unsafe { item.setTarget(Some(&**target as &AnyObject)) };
    menu.addItem(&item);
}
