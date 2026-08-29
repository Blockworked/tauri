// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

#[cfg(any(
  windows,
  target_os = "linux",
  target_os = "dragonfly",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd"
))]
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
#[cfg(any(
  windows,
  target_os = "linux",
  target_os = "dragonfly",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd"
))]
use winit::window::Window as WinitWindow;

pub(crate) struct SendRawWindowHandle(pub raw_window_handle::RawWindowHandle);
unsafe impl Send for SendRawWindowHandle {}

pub(crate) struct SendRawDisplayHandle(pub raw_window_handle::RawDisplayHandle);
unsafe impl Send for SendRawDisplayHandle {}

#[cfg(any(
  windows,
  target_os = "linux",
  target_os = "dragonfly",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd"
))]
#[derive(Clone, Copy, Debug)]
pub(crate) struct SoftbufferWindowHandle {
  display: RawDisplayHandle,
  window: RawWindowHandle,
}

#[cfg(any(
  windows,
  target_os = "linux",
  target_os = "dragonfly",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd"
))]
impl SoftbufferWindowHandle {
  pub(crate) fn new(window: &dyn WinitWindow) -> Option<Self> {
    Some(Self {
      display: window.display_handle().ok()?.as_raw(),
      window: window.window_handle().ok()?.as_raw(),
    })
  }
}

#[cfg(any(
  windows,
  target_os = "linux",
  target_os = "dragonfly",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd"
))]
impl HasDisplayHandle for SoftbufferWindowHandle {
  fn display_handle(
    &self,
  ) -> std::result::Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
    Ok(unsafe { raw_window_handle::DisplayHandle::borrow_raw(self.display) })
  }
}

#[cfg(any(
  windows,
  target_os = "linux",
  target_os = "dragonfly",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd"
))]
impl HasWindowHandle for SoftbufferWindowHandle {
  fn window_handle(
    &self,
  ) -> std::result::Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
    Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(self.window) })
  }
}
