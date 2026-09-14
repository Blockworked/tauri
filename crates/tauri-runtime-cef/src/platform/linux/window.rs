// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{
  cell::{Cell, RefCell},
  num::NonZeroU32,
  os::raw::c_ulong,
  rc::Rc,
};
use tauri_runtime::ProgressBarState;
use tauri_runtime::dpi::PhysicalSize;
use tauri_utils::config::Color;
use winit::platform::gtk4::WindowExtGtk4;

use crate::{window::AppWindow, window_handle::SoftbufferWindowHandle};

use super::{taskbar, utils::set_wm_state};

/// 24-bit X11 parent for CEF browser children.
///
/// GTK owns the toplevel layout and menu widgets, while CEF creates native X11
/// child windows. This host is kept sized to GTK's content box so CEF renders
/// below GTK UI instead of covering it. The host uses a 24-bit TrueColor visual
/// because CEF does not render correctly with the inherited GTK window visual on
/// all X11 setups.
///
/// Hierarchy:
/// - GtkApplicationWindow
///   - GtkBox
///     - menu
///     - content GtkBox
///   - CefX11Host, positioned over the content GtkBox
///     - CEF webview
///     - CEF webview
///     - CEF webview
pub(crate) struct CefX11Host {
  default_vbox: gtk::Box,
  /// Native X11 container for CEF browser children. `None` when the GDK
  /// backend is Wayland: there browsers parent to the `wl_surface` (or render
  /// off-screen) and no X11 window exists, so anything touching it must have
  /// returned early on `is_wayland()` already.
  x11: Option<X11Host>,
  geometry: Rc<HostGeometry>,
  /// CSS provider currently backing this window's background color, kept so it can be removed
  /// from the display instead of accumulating one provider per `set_background_color` call.
  background_color_provider: RefCell<Option<gtk::CssProvider>>,
}

/// Native X11 container a [`CefX11Host`] reparents CEF browsers into.
struct X11Host {
  xid: c_ulong,
  colormap: c_ulong,
}

/// Geometry of the X11 host, shared with the GTK `layout` handler that keeps it up to date.
#[derive(Default)]
struct HostGeometry {
  size: Cell<PhysicalSize<u32>>,
  /// Set when [`Self::size`] changed and the CEF children have not been laid out against it yet.
  needs_relayout: Cell<bool>,
}

impl CefX11Host {
  pub(crate) fn new(window: &dyn winit::window::Window) -> Option<Self> {
    use gtk::prelude::*;

    let gtk_window = window.gtk_window()?;
    let default_vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
    default_vbox.set_hexpand(true);
    default_vbox.set_vexpand(true);

    let webview_area = gtk::Box::new(gtk::Orientation::Vertical, 0);
    webview_area.set_hexpand(true);
    webview_area.set_vexpand(true);

    default_vbox.append(&webview_area);
    gtk_window.set_child(Some(&default_vbox));

    let initial_size = window.surface_size();
    let geometry = Rc::new(HostGeometry {
      size: Cell::new(initial_size),
      needs_relayout: Cell::new(false),
    });

    // No X11 container under Wayland: the window handle is a `wl_surface*`,
    // not an X11 `Window`, so `window_xid` below would panic. Browsers parent
    // to the surface (or render off-screen) instead.
    let x11 = if crate::runtime::is_wayland() {
      None
    } else {
      let parent_xid = window_xid(window);
      let (xid, colormap) = create_cef_container(parent_xid, initial_size)?;

      if let Some(surface) = gtk_window.surface() {
        let gtk_window = gtk_window.clone();
        let layout_webview_area = webview_area.clone();
        let layout_geometry = geometry.clone();
        surface.connect_layout(move |surface, _, _| {
          let size = set_cef_container_bounds(
            xid,
            &gtk_window,
            &layout_webview_area,
            surface.scale_factor().max(1) as f64,
          );
          if layout_geometry.size.replace(size) != size {
            // The content area changed without the toplevel being resized - a menu bar was
            // attached, hidden or shown - so winit emits no `SurfaceResized` and the CEF children
            // would keep the bounds computed against the previous host size.
            layout_geometry.needs_relayout.set(true);
          }
        });
      }

      Some(X11Host { xid, colormap })
    };

    Some(Self {
      default_vbox,
      x11,
      geometry,
      background_color_provider: RefCell::new(None),
    })
  }

  pub(crate) fn default_vbox(&self) -> gtk::Box {
    self.default_vbox.clone()
  }

  /// Native X11 container, if this window has one. `None` under Wayland;
  /// callers there must have returned early on `is_wayland()` already.
  fn x11(&self) -> &X11Host {
    self.x11.as_ref().expect("no X11 host on a Wayland window")
  }

  /// CSS class carrying this window's background color. The X11 host id makes it unique per
  /// window, since the providers below are registered display-wide.
  fn background_color_class(xid: c_ulong) -> String {
    format!("tauri-cef-window-background-{xid}")
  }

  pub(crate) fn size(&self) -> PhysicalSize<u32> {
    self.geometry.size.get()
  }

  /// Whether the host was resized by GTK since the last time the CEF children were laid out.
  pub(crate) fn take_needs_relayout(&self) -> bool {
    self.geometry.needs_relayout.replace(false)
  }

  fn take_background_color_provider(&self) -> Option<gtk::CssProvider> {
    self.background_color_provider.borrow_mut().take()
  }

  fn set_background_color_provider(&self, provider: gtk::CssProvider) {
    self.background_color_provider.replace(Some(provider));
  }
}

impl Drop for CefX11Host {
  fn drop(&mut self) {
    if let Some(provider) = self.background_color_provider.borrow_mut().take() {
      gtk::style_context_remove_provider_for_display(
        &gtk::prelude::WidgetExt::display(&self.default_vbox),
        &provider,
      );
    }

    if let Some(x11) = &self.x11 {
      let (xid, colormap) = (x11.xid, x11.colormap);
      super::utils::with_x11((), |xlib, display| unsafe {
        (xlib.XDestroyWindow)(display, xid);
        (xlib.XFreeColormap)(display, colormap);
      });
    }
  }
}

impl AppWindow {
  /// The native parent handle passed to `CefWindowInfo::SetAsChild`: the X11 host window under
  /// Ozone/X11, or a `wl_surface*` under Ozone/Wayland (winit-gtk4 reports a Wayland handle
  /// when GDK itself runs on the Wayland backend).
  pub(crate) fn cef_host_handle(&self) -> cef::sys::cef_window_handle_t {
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
    self.cef_host.x11().xid as cef::sys::cef_window_handle_t
  }

  pub(crate) fn xid(&self) -> c_ulong {
    window_xid(self.window.as_ref())
  }

  pub(crate) fn raise_native(&self) {
    // Wayland offers clients no protocol to raise their own toplevel above
    // others; activation goes through xdg-activation, which `focus_window`
    // (called right after this in `activate`) already drives. The X11
    // `_NET_ACTIVE_WINDOW` dance below has no target there.
    if crate::runtime::is_wayland() {
      return;
    }
    super::utils::activate_window(self.xid());
  }

  /// Applies the transient parent recorded by the window builder, if any.
  pub(crate) fn apply_transient_for(&self) {
    use gtk::prelude::GtkWindowExt;

    let Some(parent) = &self.attrs.transient_for else {
      return;
    };
    if let Some(window) = self.window.gtk_window() {
      window.set_transient_for(Some(parent));
    }
  }

  /// Note that this only covers the GTK widget tree: the CEF browsers are foreign X11 children
  /// that GTK does not dispatch events for, so the web contents stay interactive.
  pub(crate) fn set_enabled(&self, enabled: bool) {
    use gtk::prelude::*;

    if let Some(window) = self.window.gtk_window() {
      window.set_sensitive(enabled);
    }
  }

  pub(crate) fn is_enabled(&self) -> bool {
    use gtk::prelude::*;

    self
      .window
      .gtk_window()
      .map(|window| window.is_sensitive())
      .unwrap_or(true)
  }

  pub(crate) fn set_background_color(&self, color: Option<Color>) {
    use gtk::prelude::*;

    // No X11 host to paint under Wayland; the browser's own background
    // (BrowserSettings.background_color) is what's visible there.
    if crate::runtime::is_wayland() {
      return;
    }
    let Some(window) = self.window.gtk_window() else {
      return;
    };

    let display = gtk::prelude::WidgetExt::display(&window);
    let xid = self.cef_host.x11().xid;
    let class = CefX11Host::background_color_class(xid);

    // GTK has no way to replace a provider, so drop the one installed by the previous call -
    // otherwise every call leaves another provider registered on the display for good.
    if let Some(previous) = self.cef_host.take_background_color_provider() {
      gtk::style_context_remove_provider_for_display(&display, &previous);
    }

    let Some(color) = color else {
      window.remove_css_class(&class);
      return;
    };

    let provider = gtk::CssProvider::new();
    let css = format!(
      ".{class} {{ background-color: rgba({}, {}, {}, {:.3}); }}",
      color.0,
      color.1,
      color.2,
      f64::from(color.3) / 255.
    );
    provider.load_from_bytes(&gtk::glib::Bytes::from_owned(css));
    gtk::style_context_add_provider_for_display(
      &display,
      &provider,
      gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    window.add_css_class(&class);
    self.cef_host.set_background_color_provider(provider);
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
    let osr_frame = self
      .children
      .iter()
      .find_map(|child| child.osr_frame.clone());
    if crate::runtime::is_wayland()
      && let Some(frame) = osr_frame
    {
      let size = self.window.surface_size();
      if self.argb_surface.is_none() {
        self.argb_surface = super::argb_surface::ArgbSurface::new(self.window.as_ref());
      }
      if let Some(surface) = &mut self.argb_surface {
        surface.present(size.width, size.height, &frame);
      }
      return;
    }
    if !crate::runtime::is_wayland() && osr_frame.is_none() {
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
      if let Some(frame) = osr_frame {
        let frame = frame.lock().unwrap();
        let src_width = frame.paint_width.max(0) as usize;
        let src_height = frame.paint_height.max(0) as usize;
        let dst_width = width.get() as usize;
        let dst_height = height.get() as usize;
        if src_width > 0
          && src_height > 0
          && frame.bgra.len() >= src_width.saturating_mul(src_height).saturating_mul(4)
        {
          for dst_y in 0..dst_height {
            let src_y = dst_y.saturating_mul(src_height) / dst_height;
            for dst_x in 0..dst_width {
              let src_x = dst_x.saturating_mul(src_width) / dst_width;
              let src = (src_y * src_width + src_x) * 4;
              let b = frame.bgra[src] as u32;
              let g = frame.bgra[src + 1] as u32;
              let r = frame.bgra[src + 2] as u32;
              let a = frame.bgra[src + 3] as u32;
              buffer[dst_y * dst_width + dst_x] = b | (g << 8) | (r << 16) | (a << 24);
            }
          }
        } else {
          buffer.fill(color);
        }
      } else {
        buffer.fill(color);
      }
      let _ = buffer.present();
    }
  }
}

fn window_xid(window: &dyn winit::window::Window) -> c_ulong {
  let handle = window.window_handle().expect("failed to get window handle");
  match handle.as_raw() {
    RawWindowHandle::Xlib(handle) => handle.window as c_ulong,
    RawWindowHandle::Xcb(handle) => handle.window.get() as c_ulong,
    other => panic!("expected X11 window handle, got {other:?}"),
  }
}

fn create_cef_container(
  parent_xid: c_ulong,
  initial_size: PhysicalSize<u32>,
) -> Option<(c_ulong, c_ulong)> {
  use x11_dl::xlib::*;

  super::utils::with_x11(None, |xlib, display| unsafe {
    let screen = (xlib.XDefaultScreen)(display);
    let root = (xlib.XRootWindow)(display, screen);
    let mut visual_info: x11_dl::xlib::XVisualInfo = std::mem::zeroed();

    if (xlib.XMatchVisualInfo)(
      display,
      screen,
      24,
      x11_dl::xlib::TrueColor,
      &mut visual_info,
    ) == 0
    {
      return None;
    }

    let colormap = (xlib.XCreateColormap)(display, root, visual_info.visual, AllocNone);
    if colormap == 0 {
      return None;
    }

    let mut attrs: XSetWindowAttributes = std::mem::zeroed();
    attrs.event_mask = ExposureMask | StructureNotifyMask;
    attrs.colormap = colormap;
    attrs.border_pixel = 0;

    let xid = (xlib.XCreateWindow)(
      display,
      parent_xid as Window,
      0,
      0,
      initial_size.width,
      initial_size.height,
      0,
      visual_info.depth,
      InputOutput as _,
      visual_info.visual,
      CWEventMask | CWColormap | CWBorderPixel,
      &mut attrs,
    );
    if xid == 0 {
      (xlib.XFreeColormap)(display, colormap);
      return None;
    }

    (xlib.XMapWindow)(display, xid);
    Some((xid, colormap))
  })
}

/// Moves and resizes the X11 host over the GTK content area, returning its new size.
///
/// GTK4 widget geometry is in logical units while the X11 toplevel GDK creates is sized in device
/// pixels (logical * scale), so every value handed to `XMoveResizeWindow` must be scaled - without
/// it the host covers only 1/scale of the window on HiDPI screens.
fn set_cef_container_bounds(
  xid: c_ulong,
  gtk_window: &gtk::ApplicationWindow,
  webview_area: &gtk::Box,
  scale_factor: f64,
) -> PhysicalSize<u32> {
  use gtk::prelude::*;

  let width = (webview_area.width() as f64 * scale_factor).round() as u32;
  let height = (webview_area.height() as f64 * scale_factor).round() as u32;
  let point = gtk::graphene::Point::new(0.0, 0.0);
  let point = webview_area
    .compute_point(gtk_window, &point)
    .unwrap_or_else(|| gtk::graphene::Point::new(0.0, 0.0));
  let x = (point.x() as f64 * scale_factor).round() as i32;
  let y = (point.y() as f64 * scale_factor).round() as i32;

  super::utils::with_x11((), |xlib, display| unsafe {
    (xlib.XMoveResizeWindow)(display, xid as _, x, y, width, height);
  });

  PhysicalSize::new(width, height)
}
