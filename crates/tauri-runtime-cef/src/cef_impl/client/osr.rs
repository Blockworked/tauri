// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::sync::{Arc, Mutex};

use cef::*;
use tauri_runtime::{UserEvent, window::WindowId};

use crate::{
  macros::wrap_with_args,
  runtime::{Message, RuntimeContext},
};

/// The most recent software-rendered CEF view frame.
///
/// Popup painting is intentionally not represented here. The transparent
/// webviews this path was introduced for contain no native menus or selects;
/// supporting richer OSR pages will require a second popup buffer and bounds.
#[derive(Default)]
pub(crate) struct OsrFrame {
  pub(crate) view_width: i32,
  pub(crate) view_height: i32,
  pub(crate) device_scale_factor: f32,
  pub(crate) paint_width: i32,
  pub(crate) paint_height: i32,
  pub(crate) bgra: Vec<u8>,
}

pub(crate) type SharedOsrFrame = Arc<Mutex<OsrFrame>>;

wrap_with_args! {
  wrap_render_handler => TauriCefOsrRenderHandlerArgs;

  pub(crate) struct TauriCefOsrRenderHandler<T: UserEvent> {
    frame: SharedOsrFrame,
    context: RuntimeContext<T>,
    window_id: WindowId,
  }

  impl RenderHandler {
    fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
      let Some(rect) = rect else {
        return;
      };
      let frame = self.frame.lock().unwrap();
      rect.x = 0;
      rect.y = 0;
      rect.width = frame.view_width.max(1);
      rect.height = frame.view_height.max(1);
    }

    fn screen_info(
      &self,
      _browser: Option<&mut Browser>,
      screen_info: Option<&mut ScreenInfo>,
    ) -> ::std::os::raw::c_int {
      let Some(screen_info) = screen_info else {
        return 0;
      };
      screen_info.device_scale_factor = self.frame.lock().unwrap().device_scale_factor.max(1.0);
      1
    }

    fn on_paint(
      &self,
      _browser: Option<&mut Browser>,
      type_: PaintElementType,
      _dirty_rects: Option<&[Rect]>,
      buffer: *const u8,
      width: ::std::os::raw::c_int,
      height: ::std::os::raw::c_int,
    ) {
      if type_ != PaintElementType::VIEW || buffer.is_null() || width <= 0 || height <= 0 {
        return;
      }

      let Some(len) = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
      else {
        return;
      };
      let pixels = unsafe { std::slice::from_raw_parts(buffer, len) };
      {
        let mut frame = self.frame.lock().unwrap();
        frame.paint_width = width;
        frame.paint_height = height;
        frame.bgra.clear();
        frame.bgra.extend_from_slice(pixels);
      }

      // CEF may paint on its own callback thread. RuntimeContext already owns
      // the cross-thread event-loop channel, so ask the winit thread to redraw
      // instead of retaining a native window reference here.
      let _ = self.context.send_message(Message::RequestRedraw(self.window_id));
    }
  }
}
