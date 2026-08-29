// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{num::NonZeroU32, os::raw::c_ulong};
use tauri_runtime::ProgressBarState;
use tauri_utils::config::Color;

use crate::{window::AppWindow, window_handle::SoftbufferWindowHandle};

use super::{taskbar, utils::set_wm_state};

impl AppWindow {
  /// The native parent handle passed to `CefWindowInfo::SetAsChild`: an X11
  /// `Window` under Ozone/X11, or a `wl_surface*` under Ozone/Wayland.
  pub(crate) fn raw_cef_handle(&self) -> cef::sys::cef_window_handle_t {
    if crate::runtime::is_wayland() {
      let handle = self
        .window
        .window_handle()
        .expect("failed to get window handle");
      let RawWindowHandle::Wayland(handle) = handle.as_raw() else {
        panic!("expected Wayland window handle, got {:?}", handle.as_raw());
      };
      return handle.surface.as_ptr() as cef::sys::cef_window_handle_t;
    }
    self.xid() as cef::sys::cef_window_handle_t
  }

  pub(crate) fn xid(&self) -> c_ulong {
    let handle = self
      .window
      .window_handle()
      .expect("failed to get window handle");
    match handle.as_raw() {
      RawWindowHandle::Xlib(handle) => handle.window as c_ulong,
      RawWindowHandle::Xcb(handle) => handle.window.get() as c_ulong,
      other => panic!("expected X11 window handle, got {other:?}"),
    }
  }

  pub(crate) fn raise_native(&self) {
    super::utils::activate_window(self.xid());
  }

  pub(crate) fn set_enabled(&self, enabled: bool) {
    let _ = (self, enabled);
    // TODO: implement native window enabled state on Linux/BSD.
  }

  pub(crate) fn is_enabled(&self) -> bool {
    let _ = self;
    // TODO: query native window enabled state on Linux/BSD.
    true
  }

  pub(crate) fn set_background_color(&self, color: Option<Color>) {
    // No X11 window to paint under Wayland; the browser's own background
    // (BrowserSettings.background_color) is what's visible there.
    if crate::runtime::is_wayland() {
      return;
    }
    let xid = self.xid();
    let Some(color) = color else {
      return;
    };

    super::utils::with_x11((), |xlib, display| unsafe {
      let screen = (xlib.XDefaultScreen)(display);
      let colormap = (xlib.XDefaultColormap)(display, screen);
      let mut xcolor = x11_dl::xlib::XColor {
        pixel: 0,
        red: u16::from(color.0) * 257,
        green: u16::from(color.1) * 257,
        blue: u16::from(color.2) * 257,
        flags: x11_dl::xlib::DoRed | x11_dl::xlib::DoGreen | x11_dl::xlib::DoBlue,
        pad: 0,
      };

      if (xlib.XAllocColor)(display, colormap, &mut xcolor) != 0 {
        (xlib.XSetWindowBackground)(display, xid, xcolor.pixel);
        (xlib.XClearWindow)(display, xid);
      }
    });
  }

  pub(crate) fn set_skip_taskbar(&self, skip: bool) {
    // `_NET_WM_STATE` is an X11 window-manager property; Wayland has no
    // equivalent for a client to set on itself (compositor-specific
    // protocols aside), so this becomes the client's job there.
    if crate::runtime::is_wayland() {
      return;
    }
    set_wm_state(self.xid(), skip, "_NET_WM_STATE_SKIP_TASKBAR", None);
  }

  pub(crate) fn set_visible_on_all_workspaces(&self, visible: bool) {
    if crate::runtime::is_wayland() {
      return;
    }
    set_wm_state(self.xid(), visible, "_NET_WM_STATE_STICKY", None);
  }

  pub(crate) fn set_progress_bar(&self, state: ProgressBarState) {
    taskbar::set_progress_bar(state);
  }

  /// Paints the window's own background over everything its webviews do not
  /// cover.
  ///
  /// This is the only thing that ever paints the host window itself on
  /// Wayland, and it isn't optional there: a toplevel `wl_surface` isn't
  /// actually mapped by the compositor until the client attaches and commits
  /// a buffer to it, and winit deliberately never does this on the app's
  /// behalf (it fires `RedrawRequested` instead, once per configure, and
  /// leaves drawing to the app). Without this, the window can still get a
  /// taskbar entry -- the `xdg_toplevel` exists -- while never actually
  /// becoming visible. X11 has no such requirement, so this is skipped there;
  /// its background painting still goes through `set_background_color`.
  pub(crate) fn draw_background_surface(&mut self) {
    if !crate::runtime::is_wayland() {
      return;
    }

    let size = self.window.surface_size();
    let (Some(width), Some(height)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
    else {
      return;
    };

    if self.background_surface.is_none() {
      let Some(handle) = SoftbufferWindowHandle::new(self.window.as_ref()) else {
        return;
      };
      let Ok(context) = softbuffer::Context::new(handle) else {
        return;
      };
      let Ok(surface) = softbuffer::Surface::new(&context, handle) else {
        return;
      };
      self.background_surface = Some(surface);
    }

    let Some(surface) = &mut self.background_surface else {
      return;
    };

    let color = match self.attrs.background_color {
      Some(Color(r, g, b, _)) => (b as u32) | ((g as u32) << 8) | ((r as u32) << 16),
      // A transparent window paints nothing so the desktop shows through, while
      // an ordinary one falls back to the opaque white a blank browser shows.
      None if self.attrs.inner.transparent => 0,
      None => 0x00ff_ffff,
    };

    if surface.resize(width, height).is_ok()
      && let Ok(mut buffer) = surface.buffer_mut()
    {
      buffer.fill(color);
      let _ = buffer.present();
    }
  }
}
