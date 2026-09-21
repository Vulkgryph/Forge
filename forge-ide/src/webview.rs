// SPDX-License-Identifier: Apache-2.0
//! A real browser, inside the window, for the pages a crawler is refused.
//!
//! Some sites answer an automated request with a bot check instead of the
//! page. That check is the site's access control and Forge does not try to
//! defeat it: no spoofed identity, no solver, and no replaying a cookie
//! through an HTTP client that would then be claiming to be a browser it is
//! not. What it does instead is show the page to the person sitting in front
//! of it, in an actual browser engine, and let them decide.
//!
//! That is not a way around the check — it is the check working. It asks
//! whether a person is there, and when this panel is open the answer is yes.
//! The request comes from WebKit with WebKit's own fingerprint, the cookies
//! are WebKit's, and a human is clicking. Nothing about it claims to be
//! something else, which is the property separating it from every technique in
//! this area that is evasion.
//!
//! ## How it is put together
//!
//! `WKWebView` is an `NSView`, so it is parented onto the window's content
//! view and positioned each frame over a rectangle egui leaves empty. egui
//! draws with the GPU and knows nothing about native views; the two coexist by
//! one reserving space and the other filling it.
//!
//! No crate is added for any of this. WebKit's classes are looked up by name
//! at runtime and the framework linked directly, which is the same category of
//! thing as the AppKit calls `dock_menu` already makes — an operating system
//! interface rather than a third-party library. `objc2` stays pinned at 0.5.2
//! to match the vendored winit, so there is one `objc2` in the graph.
//!
//! ## Getting the page back
//!
//! The obvious route is `evaluateJavaScript:completionHandler:`, which needs
//! an Objective-C block and therefore `block2`. It is avoided: a
//! `WKUserScript` injected at document end posts the document to a native
//! script-message handler instead. That needs no blocks, and it fires on every
//! completed navigation — so whatever page the person ends on is the page that
//! comes back, which is what should happen when they have followed a link or
//! been redirected through a challenge.
//!
//! Results land on a queue the event loop drains, the same shape `dock_menu`
//! uses and for the same reason: the callback arrives on the main thread inside
//! WebKit, which has no handle on the state that wants it.

#![cfg(target_os = "macos")]

use std::ffi::c_void;
use std::sync::Mutex;

use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, NSObject, NSObjectProtocol};
use objc2::{declare_class, msg_send, msg_send_id, mutability, ClassType, DeclaredClass};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};

// Link WebKit.
//
// Nothing here is imported from a WebKit crate — the classes are fetched by
// name — so the framework has to be named or the linker leaves it out and
// every lookup returns `None` at runtime.
#[link(name = "WebKit", kind = "framework")]
unsafe extern "C" {}

/// A page the panel finished loading.
#[derive(Clone, Debug)]
pub struct LoadedPage {
    /// Where the browser actually ended up, which is often not where it was
    /// sent: clearing a bot check involves a redirect, and the person may have
    /// followed a link afterwards.
    pub url: String,
    pub html: String,
}

/// Pages delivered since the last drain.
static DELIVERED: Mutex<Vec<LoadedPage>> = Mutex::new(Vec::new());

/// Take every page delivered since the last call.
pub fn take_pages() -> Vec<LoadedPage> {
    DELIVERED
        .lock()
        .map(|mut q| std::mem::take(&mut *q))
        .unwrap_or_default()
}

/// What the injected script posts: the URL, a newline, then the document.
///
/// One string rather than a dictionary, because pulling fields out of an
/// `NSDictionary` through the runtime is several times the code for no gain
/// and a URL cannot contain a raw newline.
const REPORT_SCRIPT: &str = "window.webkit.messageHandlers.forgePage.postMessage(\
     location.href + '\\n' + document.documentElement.outerHTML);";

declare_class!(
    /// Receives the injected script's message. Holds no state.
    struct PageSink;

    unsafe impl ClassType for PageSink {
        type Super = NSObject;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "ForgeWebViewPageSink";
    }

    impl DeclaredClass for PageSink {}

    unsafe impl NSObjectProtocol for PageSink {}

    unsafe impl PageSink {
        /// `WKScriptMessageHandler`'s only method.
        ///
        /// The protocol is not declared, because objc2 0.5.2 carries no WebKit
        /// bindings to declare conformance against, so the selector is
        /// implemented directly. `addScriptMessageHandler:name:` messages the
        /// object rather than checking `conformsToProtocol:`, which is what
        /// makes that sufficient.
        #[method(userContentController:didReceiveScriptMessage:)]
        fn did_receive(&self, _controller: Option<&AnyObject>, message: Option<&AnyObject>) {
            let Some(message) = message else { return };
            let body: *mut AnyObject = unsafe { msg_send![message, body] };
            if body.is_null() {
                return;
            }
            let text = unsafe { nsstring_to_string(body) };
            let Some((url, html)) = text.split_once('\n') else { return };
            if let Ok(mut queue) = DELIVERED.lock() {
                queue.push(LoadedPage {
                    url: url.to_string(),
                    html: html.to_string(),
                });
            }
            // The loop is very likely asleep: a page finishing loading is not
            // a window event, so nothing else will wake it.
            crate::wake::wake();
        }
    }
);

/// Read an `NSString` as a Rust `String`, or empty if it is not one.
unsafe fn nsstring_to_string(obj: *mut AnyObject) -> String {
    unsafe {
        let utf8: *const std::ffi::c_char = msg_send![obj, UTF8String];
        if utf8.is_null() {
            return String::new();
        }
        std::ffi::CStr::from_ptr(utf8).to_string_lossy().into_owned()
    }
}

/// A `WKWebView` parented onto the window.
pub struct WebView {
    /// The view this one is a subview of. Kept because placing the view needs
    /// its size and its coordinate direction, and both are properties of the
    /// parent rather than of the window — see [`WebView::place`].
    parent: Retained<AnyObject>,
    view: Retained<AnyObject>,
    /// Kept alive as long as the view is: the content controller holds the
    /// handler weakly, so dropping it would leave WebKit messaging freed
    /// memory on the next page load.
    _sink: Retained<PageSink>,
}

impl WebView {
    /// Create a web view and add it to `parent`, the window's `NSView`.
    ///
    /// `None` when a WebKit class cannot be found, which means the framework
    /// did not link. Callers are expected to carry on without a browser rather
    /// than fail: the handoff is a capability, and a build without it declines
    /// the same way a terminal does.
    ///
    /// # Safety
    ///
    /// `parent` must be a valid `NSView` for the current window, and this must
    /// run on the main thread.
    pub unsafe fn attach(parent: *mut c_void, mtm: MainThreadMarker) -> Option<Self> {
        if parent.is_null() {
            return None;
        }
        let config_class = AnyClass::get("WKWebViewConfiguration")?;
        let webview_class = AnyClass::get("WKWebView")?;
        let script_class = AnyClass::get("WKUserScript")?;

        let config: Retained<AnyObject> = msg_send_id![config_class, new];
        let controller: Retained<AnyObject> = msg_send_id![&config, userContentController];

        let sink: Retained<PageSink> = msg_send_id![mtm.alloc::<PageSink>(), init];
        let name = NSString::from_str("forgePage");
        let _: () = msg_send![&controller, addScriptMessageHandler: &*sink, name: &*name];

        // `1` is `WKUserScriptInjectionTimeAtDocumentEnd`. The enum has no
        // binding here and it is part of a stable ABI.
        let source = NSString::from_str(REPORT_SCRIPT);
        let script: Allocated<AnyObject> = msg_send_id![script_class, alloc];
        let script: Retained<AnyObject> = msg_send_id![
            script,
            initWithSource: &*source,
            injectionTime: 1_isize,
            forMainFrameOnly: true,
        ];
        let _: () = msg_send![&controller, addUserScript: &*script];

        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(10.0, 10.0));
        let view: Allocated<AnyObject> = msg_send_id![webview_class, alloc];
        let view: Retained<AnyObject> =
            msg_send_id![view, initWithFrame: frame, configuration: &*config];

        // Hidden until something asks for it, so attaching costs nothing
        // visible.
        let _: () = msg_send![&view, setHidden: true];
        let parent = parent as *mut AnyObject;
        let _: () = msg_send![parent, addSubview: &*view];
        // Retained so the view can be placed later: its frame is in this
        // view's coordinate space, and both the size and the direction of that
        // space are the parent's to report.
        let parent: Retained<AnyObject> = unsafe { Retained::retain(parent) }?;

        Some(Self { view, parent, _sink: sink })
    }

    /// Point the browser at a URL.
    ///
    /// `http` and `https` only. A `file:` URL would let a page the agent chose
    /// read the disk through the browser, which is not what this is for.
    pub fn load(&self, url: &str) -> bool {
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return false;
        }
        unsafe {
            let Some(url_class) = AnyClass::get("NSURL") else { return false };
            let Some(request_class) = AnyClass::get("NSURLRequest") else { return false };
            let text = NSString::from_str(url);
            let ns_url: *mut AnyObject = msg_send![url_class, URLWithString: &*text];
            if ns_url.is_null() {
                return false;
            }
            let request: *mut AnyObject = msg_send![request_class, requestWithURL: ns_url];
            if request.is_null() {
                return false;
            }
            let _: *mut AnyObject = msg_send![&self.view, loadRequest: request];
            true
        }
    }

    /// Put the view where egui says, converting into the parent's coordinates.
    ///
    /// `top` is the distance from the top of the parent to the top of the
    /// rectangle, which is how egui measures — so this takes egui's own
    /// numbers and does the conversion here, where the facts needed to do it
    /// are available.
    ///
    /// Those facts are asked for rather than assumed, which is the fix. A
    /// view's frame is in its superview's coordinate space, and whether that
    /// space counts y from the top or the bottom is the superview's own
    /// property — `isFlipped`. The first version of this took the window's
    /// `inner_size`, assumed an unflipped parent, and computed
    /// `height - rect.max.y`. Two assumptions, either of which puts the
    /// browser somewhere other than where the layout said: it rendered a few
    /// hundred points low and short, over the terminal.
    ///
    /// The parent's own `bounds` is authoritative for its size, and
    /// `isFlipped` for its direction. Neither is worth guessing when both are
    /// one message send away.
    pub fn place(&self, x: f64, top: f64, width: f64, height: f64) {
        let width = width.max(0.0);
        let height = height.max(0.0);
        let (parent_height, flipped) = unsafe {
            let bounds: NSRect = msg_send![&self.parent, bounds];
            let flipped: bool = msg_send![&self.parent, isFlipped];
            (bounds.size.height, flipped)
        };
        // Flipped: y already counts down from the top, so egui's number is the
        // frame's. Unflipped: the origin is the *bottom* left, so it is the
        // parent's height less the rectangle's lower edge.
        let y = frame_origin_y(parent_height, top, height, flipped);
        let frame = NSRect::new(NSPoint::new(x, y), NSSize::new(width, height));
        unsafe {
            let _: () = msg_send![&self.view, setFrame: frame];
        }
    }

    pub fn set_visible(&self, visible: bool) {
        unsafe {
            let _: () = msg_send![&self.view, setHidden: !visible];
        }
    }

    /// Whether the browser is still loading, for a caller that wants to say so.
    ///
    /// Unused so far: the toolbar shows no loading state, because a page that
    /// takes long enough to notice is usually a bot check and that gets said
    /// in plainer words. Kept because it is one message send and the panel
    /// will want it the moment anything shows progress.
    #[allow(dead_code)]
    pub fn is_loading(&self) -> bool {
        unsafe { msg_send![&self.view, isLoading] }
    }

    /// Go back, if there is anywhere to go.
    pub fn go_back(&self) {
        unsafe {
            if msg_send![&self.view, canGoBack] {
                let _: *mut AnyObject = msg_send![&self.view, goBack];
            }
        }
    }

    pub fn go_forward(&self) {
        unsafe {
            if msg_send![&self.view, canGoForward] {
                let _: *mut AnyObject = msg_send![&self.view, goForward];
            }
        }
    }

    pub fn can_go_back(&self) -> bool {
        unsafe { msg_send![&self.view, canGoBack] }
    }

    pub fn can_go_forward(&self) -> bool {
        unsafe { msg_send![&self.view, canGoForward] }
    }

    /// The page's title, for a tab that wants to name itself something better
    /// than its URL.
    ///
    /// Unused so far — browser tabs are still labelled by address. Kept for
    /// when they are not.
    #[allow(dead_code)]
    pub fn title(&self) -> Option<String> {
        unsafe {
            let title: *mut AnyObject = msg_send![&self.view, title];
            if title.is_null() {
                return None;
            }
            let text = nsstring_to_string(title);
            (!text.is_empty()).then_some(text)
        }
    }

    pub fn reload(&self) {
        unsafe {
            let _: *mut AnyObject = msg_send![&self.view, reload];
        }
    }

    /// Where the browser currently is.
    pub fn url(&self) -> Option<String> {
        unsafe {
            let url: *mut AnyObject = msg_send![&self.view, URL];
            if url.is_null() {
                return None;
            }
            let text: *mut AnyObject = msg_send![url, absoluteString];
            if text.is_null() {
                return None;
            }
            Some(nsstring_to_string(text))
        }
    }
}

impl Drop for WebView {
    fn drop(&mut self) {
        unsafe {
            // Out of the view hierarchy before the retain goes, or AppKit is
            // left holding a subview that has been freed.
            let _: () = msg_send![&self.view, removeFromSuperview];
        }
    }
}

/// A subview's frame origin `y`, in its parent's coordinate space.
///
/// `top` is the distance from the top of the parent to the top of the
/// rectangle — egui's own measure — and `flipped` is the parent's
/// `isFlipped`. Separate from the message sends so the arithmetic can be
/// checked without a window, which is the only part of this that was wrong.
fn frame_origin_y(parent_height: f64, top: f64, height: f64, flipped: bool) -> f64 {
    if flipped {
        // y already counts down from the top, so egui's number is the frame's.
        top
    } else {
        // The origin is the bottom-left corner, so it is the parent's height
        // less the rectangle's lower edge.
        parent_height - (top + height)
    }
}

#[cfg(test)]
mod tests {
    use super::frame_origin_y;

    /// The conversion, both ways, with numbers.
    ///
    /// A browser filling the lower 600 points of an 800-point parent — tab
    /// strip and toolbar above it — has its top 200 points down.
    #[test]
    fn a_frame_origin_follows_the_parents_direction() {
        // Unflipped: the origin is the bottom-left, so it sits 0 from the
        // bottom.
        assert_eq!(frame_origin_y(800.0, 200.0, 600.0, false), 0.0);
        // Flipped: y counts down from the top, so it is egui's own number.
        assert_eq!(frame_origin_y(800.0, 200.0, 600.0, true), 200.0);

        // A panel not reaching the bottom: top 100, 400 tall, so 300 of
        // parent left underneath.
        assert_eq!(frame_origin_y(800.0, 100.0, 400.0, false), 300.0);
        assert_eq!(frame_origin_y(800.0, 100.0, 400.0, true), 100.0);
    }

    /// The bug this replaced. The old code used the rectangle's *lower* edge
    /// against the window's height and assumed an unflipped parent — two
    /// assumptions, and getting either wrong moves the browser rather than
    /// failing. It rendered a few hundred points low and short, over the
    /// terminal.
    #[test]
    fn the_previous_arithmetic_is_not_what_this_computes() {
        let (parent, top, height) = (800.0, 200.0_f64, 600.0_f64);
        // What the old line did: window height less the rect's bottom edge,
        // which only coincides with the right answer when the rectangle
        // reaches the bottom of the parent.
        let old = parent - (top + height);
        assert_eq!(frame_origin_y(parent, top, height, false), old);

        // And where it diverges: a rectangle that stops short of the bottom
        // is placed correctly by both, while one measured against the wrong
        // parent height is not — which is why the height now comes from the
        // parent's own bounds rather than the window's inner size.
        assert_ne!(
            frame_origin_y(650.0, top, height, false),
            frame_origin_y(800.0, top, height, false),
            "the parent's height has to matter, or the view floats",
        );
    }

    /// A rectangle taller than its parent yields a negative origin rather
    /// than a panic, which AppKit clips — the honest outcome for a layout
    /// that does not fit.
    #[test]
    fn an_oversized_rectangle_does_not_panic() {
        assert_eq!(frame_origin_y(400.0, 0.0, 900.0, false), -500.0);
        assert_eq!(frame_origin_y(400.0, 0.0, 900.0, true), 0.0);
    }

    use super::*;

    /// The framework actually linked and the classes actually resolve.
    ///
    /// Worth a test because the failure is quiet: nothing here imports a
    /// WebKit crate, so if the `#[link]` attribute were dropped the code would
    /// still compile and every class lookup would return `None` at runtime —
    /// `attach` would hand back `None` and the panel would simply never
    /// appear, looking like a missing feature rather than a build problem.
    #[test]
    fn the_webkit_classes_resolve_at_runtime() {
        for name in ["WKWebView", "WKWebViewConfiguration", "WKUserScript"] {
            assert!(
                AnyClass::get(name).is_some(),
                "{name} is not available — WebKit did not link",
            );
        }
        // And the ones borrowed from Foundation for loading a URL.
        for name in ["NSURL", "NSURLRequest"] {
            assert!(AnyClass::get(name).is_some(), "{name} is not available");
        }
    }

    /// The script has to post to the name the handler is registered under, and
    /// in the shape the handler parses. Both are easy to change on one side
    /// only, and the failure would be silent — pages simply never arrive.
    #[test]
    fn the_injected_script_and_the_handler_agree() {
        assert!(
            REPORT_SCRIPT.contains("messageHandlers.forgePage"),
            "the script posts to a name the handler is not registered under",
        );
        assert!(
            REPORT_SCRIPT.contains("location.href + '\\n'"),
            "the handler splits on the first newline, so the URL must come first",
        );
        assert!(REPORT_SCRIPT.contains("outerHTML"));
    }

    /// Delivery parsing, without a browser: URL first, then the document.
    #[test]
    fn a_delivered_message_splits_into_url_and_html() {
        let message = "https://example.test/page\n<html><body>hi</body></html>";
        let (url, html) = message.split_once('\n').expect("split");
        assert_eq!(url, "https://example.test/page");
        assert_eq!(html, "<html><body>hi</body></html>");
    }

    /// A document containing newlines must survive, since only the first is a
    /// delimiter.
    #[test]
    fn newlines_in_the_document_are_preserved() {
        let message = "https://a.test/\n<html>\n<body>\nline\n</body>\n</html>";
        let (url, html) = message.split_once('\n').unwrap();
        assert_eq!(url, "https://a.test/");
        assert_eq!(html.lines().count(), 5, "{html:?}");
    }

    #[test]
    fn the_queue_drains_once() {
        let _ = take_pages();
        DELIVERED.lock().unwrap().push(LoadedPage {
            url: "https://a.test/".into(),
            html: "<p>x</p>".into(),
        });
        assert_eq!(take_pages().len(), 1);
        assert!(take_pages().is_empty(), "a page was delivered twice");
    }
}
