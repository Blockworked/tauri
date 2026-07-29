// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use cef::*;
use std::sync::{Mutex, OnceLock};

#[cfg(target_os = "linux")]
type CefOsEvent<'a> = Option<&'a mut cef::sys::XEvent>;
#[cfg(target_os = "macos")]
type CefOsEvent<'a> = *mut u8;
#[cfg(windows)]
type CefOsEvent<'a> = Option<&'a mut cef::sys::MSG>;

/// Host-app hook fed every raw key event this browser window sees, regardless
/// of `devtools_enabled`. On Windows, Chromium grabs raw keyboard input for
/// its own focused window (documented upstream:
/// https://github.com/chromiumembedded/cef/issues/2609), which starves any
/// `WH_KEYBOARD_LL` hook a host app installs — so a host that also relies on
/// a global low-level hook for hotkeys sees nothing while this window has
/// focus. This is the only path that still sees those keystrokes in that
/// case, letting the host feed them into its own hotkey pipeline. Returning
/// `true` suppresses the key (equivalent to the existing devtools-blocking
/// branches below) so a matched hotkey doesn't also land in a focused text
/// field.
static FOCUSED_KEY_HOOK: OnceLock<Mutex<Box<dyn FnMut(i32, bool) -> bool + Send>>> =
  OnceLock::new();

pub fn set_focused_key_hook(cb: impl FnMut(i32, bool) -> bool + Send + 'static) {
  let _ = FOCUSED_KEY_HOOK.set(Mutex::new(Box::new(cb)));
}

wrap_keyboard_handler! {
  pub struct TauriCefKeyboardHandler {
    devtools_enabled: bool,
  }

  impl KeyboardHandler {
    fn on_pre_key_event(
      &self,
      _browser: Option<&mut Browser>,
      event: Option<&KeyEvent>,
      _os_event: CefOsEvent<'_>,
      _is_keyboard_shortcut: Option<&mut ::std::os::raw::c_int>,
    ) -> ::std::os::raw::c_int {
      use cef::sys::cef_key_event_type_t;

      if let Some(event) = event {
        let rawdown: cef::KeyEventType = cef_key_event_type_t::KEYEVENT_RAWKEYDOWN.into();
        let keyup: cef::KeyEventType = cef_key_event_type_t::KEYEVENT_KEYUP.into();
        let pressed = event.type_ == rawdown;
        if pressed || event.type_ == keyup {
          if let Some(hook) = FOCUSED_KEY_HOOK.get() {
            if let Ok(mut hook) = hook.lock() {
              if hook(event.windows_key_code, pressed) {
                return 1;
              }
            }
          }
        }
      }

      // If devtools is disabled, block devtools keyboard shortcuts.
      if !self.devtools_enabled {
        let Some(event) = event else {
          return 0;
        };

        // Check if this is a keydown event.
        let keydown_type: cef::KeyEventType = cef_key_event_type_t::KEYEVENT_RAWKEYDOWN.into();
        if event.type_ != keydown_type {
          return 0;
        }

        // Get modifier keys.
        use cef::sys::cef_event_flags_t;
        #[cfg(windows)]
        let modifiers = event.modifiers as i32;
        #[cfg(not(windows))]
        let modifiers = event.modifiers;

        #[cfg(not(target_os = "macos"))]
        let ctrl = (modifiers & (cef_event_flags_t::EVENTFLAG_CONTROL_DOWN.0)) != 0;
        #[cfg(not(target_os = "macos"))]
        let shift = (modifiers & (cef_event_flags_t::EVENTFLAG_SHIFT_DOWN.0)) != 0;

        let key_code = event.windows_key_code;

        // Block F12 (key code 123).
        if key_code == 123 {
          if let Some(is_keyboard_shortcut) = _is_keyboard_shortcut {
            *is_keyboard_shortcut = 1;
          }
          return 1;
        }

        // Block Ctrl+Shift+I (key code 73 = 'I') on Linux/Windows.
        #[cfg(not(target_os = "macos"))]
        if key_code == 73 && ctrl && shift {
          if let Some(is_keyboard_shortcut) = _is_keyboard_shortcut {
            *is_keyboard_shortcut = 1;
          }
          return 1;
        }

        // Block Cmd+Opt+I on macOS.
        #[cfg(target_os = "macos")]
        {
          let meta = (modifiers & cef_event_flags_t::EVENTFLAG_COMMAND_DOWN.0) != 0;
          let alt = (modifiers & cef_event_flags_t::EVENTFLAG_ALT_DOWN.0) != 0;
          if key_code == 73 && meta && alt {
            if let Some(is_keyboard_shortcut) = _is_keyboard_shortcut {
              *is_keyboard_shortcut = 1;
            }
            return 1;
          }
        }
      }

      0
    }
  }
}
