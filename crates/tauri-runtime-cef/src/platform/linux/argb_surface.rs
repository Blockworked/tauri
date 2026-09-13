// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

//! Minimal alpha-capable Wayland software surface for CEF OSR frames.
//!
//! `softbuffer` deliberately creates `Xrgb8888` wl_shm buffers, which makes
//! every transparent CEF pixel opaque black. OSR needs `Argb8888`; keeping
//! this small presenter local avoids changing the normal window background
//! path or imposing alpha semantics on other softbuffer users.

use std::{
  ffi::CStr,
  fs::File,
  os::fd::{AsFd, AsRawFd, FromRawFd},
  ptr::NonNull,
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  },
};

use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use wayland_client::{
  Connection, Dispatch, EventQueue, Proxy, QueueHandle,
  backend::{Backend, ObjectId},
  globals::{GlobalListContents, registry_queue_init},
  protocol::{wl_buffer, wl_registry, wl_shm, wl_shm_pool, wl_surface},
};

use crate::{cef_impl::client::SharedOsrFrame, window_handle::SoftbufferWindowHandle};

struct State;

pub(crate) struct ArgbSurface {
  connection: Option<Connection>,
  event_queue: EventQueue<State>,
  queue_handle: QueueHandle<State>,
  shm: wl_shm::WlShm,
  surface: Option<wl_surface::WlSurface>,
  buffers: Vec<ArgbBuffer>,
}

impl ArgbSurface {
  pub(crate) fn new(window: &dyn winit::window::Window) -> Option<Self> {
    let handle = SoftbufferWindowHandle::new(window)?;
    let RawDisplayHandle::Wayland(display) = handle.display_handle().ok()?.as_raw() else {
      return None;
    };
    let RawWindowHandle::Wayland(window) = handle.window_handle().ok()?.as_raw() else {
      return None;
    };

    let backend = unsafe { Backend::from_foreign_display(display.display.as_ptr().cast()) };
    let connection = Connection::from_backend(backend);
    let (globals, event_queue) = registry_queue_init(&connection).ok()?;
    let queue_handle = event_queue.handle();
    let shm = globals.bind(&queue_handle, 1..=1, ()).ok()?;
    let surface_id = unsafe {
      ObjectId::from_ptr(
        wl_surface::WlSurface::interface(),
        window.surface.as_ptr().cast(),
      )
    }
    .ok()?;
    let surface = wl_surface::WlSurface::from_id(&connection, surface_id).ok()?;

    Some(Self {
      connection: Some(connection),
      event_queue,
      queue_handle,
      shm,
      surface: Some(surface),
      buffers: Vec::new(),
    })
  }

  pub(crate) fn present(&mut self, width: u32, height: u32, frame: &SharedOsrFrame) {
    let Ok(width) = i32::try_from(width) else {
      return;
    };
    let Ok(height) = i32::try_from(height) else {
      return;
    };
    if width <= 0 || height <= 0 {
      return;
    }

    // Never wait for the compositor from winit's event-loop thread. A
    // blocking dispatch here freezes pointer/key delivery and makes the
    // overlay appear hung. Read any immediately available Wayland events,
    // dispatch buffer releases, then either use a free buffer or drop this
    // intermediate frame.
    let _ = self.event_queue.dispatch_pending(&mut State);
    if let Some(connection) = &self.connection
      && let Some(guard) = connection.prepare_read()
    {
      let mut descriptor = libc::pollfd {
        fd: connection.as_fd().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
      };
      if unsafe { libc::poll(&mut descriptor, 1, 0) } > 0 {
        let _ = guard.read();
      }
    }
    let _ = self.event_queue.dispatch_pending(&mut State);
    self.buffers.retain(|buffer| {
      !buffer.released.load(Ordering::SeqCst) || (buffer.width == width && buffer.height == height)
    });

    let mut buffer_index = self.buffers.iter().position(|buffer| {
      buffer.released.load(Ordering::SeqCst) && buffer.width == width && buffer.height == height
    });
    if buffer_index.is_none() && self.buffers.len() < 3 {
      if let Some(buffer) = ArgbBuffer::new(&self.shm, &self.queue_handle, width, height) {
        self.buffers.push(buffer);
        buffer_index = Some(self.buffers.len() - 1);
      }
    }
    let Some(buffer_index) = buffer_index else {
      return;
    };
    let buffer = &mut self.buffers[buffer_index];
    buffer.copy_frame(frame);
    buffer.released.store(false, Ordering::SeqCst);
    let Some(surface) = &self.surface else {
      return;
    };
    surface.attach(Some(&buffer.proxy), 0, 0);
    if surface.version() >= 4 {
      surface.damage_buffer(0, 0, width, height);
    } else {
      surface.damage(0, 0, i32::MAX, i32::MAX);
    }
    surface.commit();
    let _ = self.connection.as_ref().unwrap().flush();
  }
}

impl Drop for ArgbSurface {
  fn drop(&mut self) {
    self.buffers.clear();
    self.surface = None;
    self.connection = None;
  }
}

struct ArgbBuffer {
  width: i32,
  height: i32,
  len: usize,
  map: NonNull<u8>,
  _file: File,
  pool: wl_shm_pool::WlShmPool,
  proxy: wl_buffer::WlBuffer,
  released: Arc<AtomicBool>,
}

impl ArgbBuffer {
  fn new(
    shm: &wl_shm::WlShm,
    queue_handle: &QueueHandle<State>,
    width: i32,
    height: i32,
  ) -> Option<Self> {
    let len = (width as usize)
      .checked_mul(height as usize)?
      .checked_mul(4)?;
    let name = CStr::from_bytes_with_nul(b"tauri-cef-osr\0").ok()?;
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
      return None;
    }
    let file = unsafe { File::from_raw_fd(fd) };
    file.set_len(len as u64).ok()?;
    let map = unsafe {
      libc::mmap(
        std::ptr::null_mut(),
        len,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_SHARED,
        fd,
        0,
      )
    };
    let map = NonNull::new(map.cast()).filter(|_| map != libc::MAP_FAILED)?;
    let pool = shm.create_pool(file.as_fd(), len as i32, queue_handle, ());
    let released = Arc::new(AtomicBool::new(true));
    let proxy = pool.create_buffer(
      0,
      width,
      height,
      width * 4,
      wl_shm::Format::Argb8888,
      queue_handle,
      released.clone(),
    );
    Some(Self {
      width,
      height,
      len,
      map,
      _file: file,
      pool,
      proxy,
      released,
    })
  }

  fn copy_frame(&mut self, frame: &SharedOsrFrame) {
    let frame = frame.lock().unwrap();
    let src_width = frame.paint_width.max(0) as usize;
    let src_height = frame.paint_height.max(0) as usize;
    let dst_width = self.width as usize;
    let dst_height = self.height as usize;
    let dst = unsafe { std::slice::from_raw_parts_mut(self.map.as_ptr(), self.len) };
    if src_width == 0
      || src_height == 0
      || frame.bgra.len() < src_width.saturating_mul(src_height).saturating_mul(4)
    {
      dst.fill(0);
      return;
    }

    for dst_y in 0..dst_height {
      let src_y = dst_y.saturating_mul(src_height) / dst_height;
      for dst_x in 0..dst_width {
        let src_x = dst_x.saturating_mul(src_width) / dst_width;
        let src = (src_y * src_width + src_x) * 4;
        let dst_index = (dst_y * dst_width + dst_x) * 4;
        dst[dst_index..dst_index + 4].copy_from_slice(&frame.bgra[src..src + 4]);
      }
    }
  }
}

impl Drop for ArgbBuffer {
  fn drop(&mut self) {
    self.proxy.destroy();
    self.pool.destroy();
    unsafe {
      libc::munmap(self.map.as_ptr().cast(), self.len);
    }
  }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
  fn event(
    _: &mut State,
    _: &wl_registry::WlRegistry,
    _: wl_registry::Event,
    _: &GlobalListContents,
    _: &Connection,
    _: &QueueHandle<State>,
  ) {
  }
}

impl Dispatch<wl_shm::WlShm, ()> for State {
  fn event(
    _: &mut State,
    _: &wl_shm::WlShm,
    _: wl_shm::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<State>,
  ) {
  }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for State {
  fn event(
    _: &mut State,
    _: &wl_shm_pool::WlShmPool,
    _: wl_shm_pool::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<State>,
  ) {
  }
}

impl Dispatch<wl_buffer::WlBuffer, Arc<AtomicBool>> for State {
  fn event(
    _: &mut State,
    _: &wl_buffer::WlBuffer,
    event: wl_buffer::Event,
    released: &Arc<AtomicBool>,
    _: &Connection,
    _: &QueueHandle<State>,
  ) {
    if let wl_buffer::Event::Release = event {
      released.store(true, Ordering::SeqCst);
    }
  }
}
