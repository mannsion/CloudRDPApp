//! Thin, safe wrapper around `gtk4-layer-shell` for floating Wayland overlays.
//!
//! When `gtk4-layer-shell-0` is not present at build time (`cfg(not(has_layer_shell))`),
//! every function degrades cleanly to a safe `false` no-op.

#[cfg(has_layer_shell)]
mod imp {
    use std::ffi::{CString, c_char, c_int};

    use gtk::glib::translate::ToGlibPtr;

    const LAYER_OVERLAY: c_int = 3; // GTK_LAYER_SHELL_LAYER_OVERLAY
    const EDGE_TOP: c_int = 2; // GTK_LAYER_SHELL_EDGE_TOP
    const KEYBOARD_MODE_NONE: c_int = 0; // GTK_LAYER_SHELL_KEYBOARD_MODE_NONE

    unsafe extern "C" {
        fn gtk_layer_is_supported() -> gtk::glib::ffi::gboolean;
        fn gtk_layer_init_for_window(window: *mut gtk::ffi::GtkWindow);
        fn gtk_layer_set_namespace(window: *mut gtk::ffi::GtkWindow, namespace: *const c_char);
        fn gtk_layer_set_layer(window: *mut gtk::ffi::GtkWindow, layer: c_int);
        fn gtk_layer_set_anchor(window: *mut gtk::ffi::GtkWindow, edge: c_int, anchor: gtk::glib::ffi::gboolean);
        fn gtk_layer_set_exclusive_zone(window: *mut gtk::ffi::GtkWindow, exclusive_zone: c_int);
        fn gtk_layer_set_keyboard_mode(window: *mut gtk::ffi::GtkWindow, mode: c_int);
    }

    pub fn is_supported() -> bool {
        // SAFETY: `gtk_layer_is_supported` takes no arguments and queries the active GDK display.
        unsafe { gtk_layer_is_supported() != 0 }
    }

    pub fn init_overlay_top_bar(window: &gtk::Window, namespace: &str) -> bool {
        if !is_supported() {
            return false;
        }
        let ns = CString::new(namespace).ok();
        let ptr: *mut gtk::ffi::GtkWindow = window.to_glib_none().0;
        // SAFETY: `ptr` is a valid, unrealized `GtkWindow` pointer borrowed for this call,
        // and `ns` outlives `gtk_layer_set_namespace` (which copies the string).
        unsafe {
            gtk_layer_init_for_window(ptr);
            gtk_layer_set_namespace(ptr, ns.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()));
            gtk_layer_set_layer(ptr, LAYER_OVERLAY);
            gtk_layer_set_anchor(ptr, EDGE_TOP, 1);
            gtk_layer_set_exclusive_zone(ptr, 0);
            gtk_layer_set_keyboard_mode(ptr, KEYBOARD_MODE_NONE);
        }
        true
    }
}

#[cfg(not(has_layer_shell))]
mod imp {
    pub fn is_supported() -> bool {
        false
    }

    pub fn init_overlay_top_bar(_window: &gtk::Window, _namespace: &str) -> bool {
        false
    }
}

/// Returns `true` when running on a Wayland compositor supporting `zwlr_layer_shell_v1`
/// and `gtk4-layer-shell` was linked at build time.
pub fn is_supported() -> bool {
    imp::is_supported()
}

/// Configures `window` (before it is realized) as an overlay layer surface anchored
/// to the top edge with zero exclusive zone and no keyboard grab.
pub fn init_overlay_top_bar(window: &gtk::Window, namespace: &str) -> bool {
    imp::init_overlay_top_bar(window, namespace)
}
