//! Native Wayland window lifecycle, keyboard input, and PTY resize integration.
//!
//! The surface currently uses dynamically-sized shared-memory placeholder
//! buffers. Keyboard input is translated using the compositor-provided XKB
//! keymap. Terminal glyph rendering and fractional scaling remain future work.

use std::io::Write;
use std::os::fd::AsFd;

use anyhow::{Context, Result, bail, ensure};
use bubbleterm_pty::PtySession;
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer, wl_compositor, wl_keyboard, wl_registry, wl_seat, wl_shm, wl_shm_pool,
        wl_surface,
    },
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

use crate::{CellMetrics, SurfaceSize, keyboard::XkbKeyboard, resize_pty_for_surface};

const INITIAL_WIDTH: u32 = 800;
const INITIAL_HEIGHT: u32 = 600;
const MAX_BUFFER_BYTES: u32 = 64 * 1024 * 1024;
const MAX_BUFFER_DIMENSION: u32 = 8192;
const WINDOW_TITLE: &str = "BubbleTerm";
const BACKGROUND_PIXEL: [u8; 4] = [0x18, 0x18, 0x20, 0xff];

/// Runs one native Wayland window until the compositor requests it to close.
///
/// Configure dimensions are logical pixels at scale factor 1 for now. The
/// caller transfers ownership of the PTY session to the window event loop.
pub fn run_wayland_window(session: PtySession, cell_metrics: CellMetrics) -> Result<()> {
    let connection = Connection::connect_to_env()
        .context("could not connect to the Wayland compositor (check WAYLAND_DISPLAY)")?;
    let (globals, mut event_queue) =
        registry_queue_init::<WindowState>(&connection).context("Wayland registry init failed")?;
    let queue_handle = event_queue.handle();

    let compositor = globals
        .bind::<wl_compositor::WlCompositor, _, _>(&queue_handle, 1..=6, ())
        .context("compositor does not provide wl_compositor")?;
    let shm = globals
        .bind::<wl_shm::WlShm, _, _>(&queue_handle, 1..=1, ())
        .context("compositor does not provide wl_shm")?;
    let wm_base = globals
        .bind::<xdg_wm_base::XdgWmBase, _, _>(&queue_handle, 1..=6, ())
        .context("compositor does not provide xdg_wm_base")?;
    let seat = globals
        .bind::<wl_seat::WlSeat, _, _>(&queue_handle, 1..=9, ())
        .context("compositor does not provide wl_seat")?;

    let surface = compositor.create_surface(&queue_handle, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &queue_handle, ());
    let toplevel = xdg_surface.get_toplevel(&queue_handle, ());
    toplevel.set_title(WINDOW_TITLE.to_owned());
    toplevel.set_app_id("org.ruraam.BubbleTerm".to_owned());

    let mut state = WindowState {
        session,
        pty_writer: None,
        cell_metrics,
        shm,
        surface,
        seat,
        keyboard: None,
        xkb: XkbKeyboard::new()?,
        buffer: None,
        buffer_released: false,
        retired_buffers: Vec::new(),
        pending_size: None,
        current_size: (INITIAL_WIDTH as i32, INITIAL_HEIGHT as i32),
        running: true,
        event_error: None,
    };
    state.pty_writer = Some(
        state
            .session
            .take_writer()
            .context("could not acquire the PTY input writer")?,
    );

    // xdg-shell requires an initial empty commit before the first configure.
    state.surface.commit();

    while state.running {
        event_queue
            .blocking_dispatch(&mut state)
            .context("Wayland event dispatch failed")?;
        if let Some(error) = state.event_error.take() {
            bail!("Wayland window event failed: {error}");
        }
    }

    Ok(())
}

fn checked_buffer_layout(width: u32, height: u32) -> Result<(i32, i32, i32, i32)> {
    ensure!(
        width > 0 && height > 0,
        "Wayland buffer dimensions must be non-zero"
    );
    ensure!(
        width <= MAX_BUFFER_DIMENSION && height <= MAX_BUFFER_DIMENSION,
        "Wayland buffer dimensions exceed the safety limit"
    );

    let stride = width
        .checked_mul(4)
        .context("Wayland buffer stride overflow")?;
    let byte_len = stride
        .checked_mul(height)
        .context("Wayland buffer size overflow")?;
    ensure!(
        byte_len <= MAX_BUFFER_BYTES,
        "Wayland buffer exceeds the 64 MiB safety limit"
    );

    Ok((
        i32::try_from(width).context("Wayland buffer width exceeds i32")?,
        i32::try_from(height).context("Wayland buffer height exceeds i32")?,
        i32::try_from(stride).context("Wayland buffer stride exceeds i32")?,
        i32::try_from(byte_len).context("Wayland SHM pool exceeds i32")?,
    ))
}

fn create_surface_buffer(
    shm: &wl_shm::WlShm,
    width: u32,
    height: u32,
    queue_handle: &QueueHandle<WindowState>,
) -> Result<wl_buffer::WlBuffer> {
    let (width_i32, height_i32, stride_i32, pool_size) = checked_buffer_layout(width, height)?;
    let byte_len = pool_size as u32;

    let mut file = tempfile::tempfile().context("could not create Wayland SHM backing file")?;
    let mut pixels = vec![0x18; byte_len as usize];
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.copy_from_slice(&BACKGROUND_PIXEL);
    }
    file.write_all(&pixels)?;
    file.flush()?;

    let pool = shm.create_pool(file.as_fd(), pool_size, queue_handle, ());
    let buffer = pool.create_buffer(
        0,
        width_i32,
        height_i32,
        stride_i32,
        wl_shm::Format::Argb8888,
        queue_handle,
        (),
    );
    pool.destroy();
    Ok(buffer)
}

struct WindowState {
    session: PtySession,
    pty_writer: Option<Box<dyn Write + Send>>,
    cell_metrics: CellMetrics,
    shm: wl_shm::WlShm,
    surface: wl_surface::WlSurface,
    seat: wl_seat::WlSeat,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    xkb: XkbKeyboard,
    buffer: Option<wl_buffer::WlBuffer>,
    buffer_released: bool,
    retired_buffers: Vec<wl_buffer::WlBuffer>,
    pending_size: Option<(i32, i32)>,
    current_size: (i32, i32),
    running: bool,
    event_error: Option<String>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WindowState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(WindowState: ignore wl_compositor::WlCompositor);
delegate_noop!(WindowState: ignore wl_shm::WlShm);
delegate_noop!(WindowState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(WindowState: ignore wl_surface::WlSurface);

impl Dispatch<wl_buffer::WlBuffer, ()> for WindowState {
    fn event(
        state: &mut Self,
        buffer: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            if state
                .buffer
                .as_ref()
                .is_some_and(|active| active.id() == buffer.id())
            {
                state.buffer_released = true;
            } else if let Some(index) = state
                .retired_buffers
                .iter()
                .position(|retired| retired.id() == buffer.id())
            {
                state.retired_buffers.remove(index).destroy();
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for WindowState {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        queue_handle: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities } = event {
            let has_keyboard = capabilities.contains(wl_seat::Capability::Keyboard);
            if has_keyboard && state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(queue_handle, ()));
            } else if !has_keyboard {
                state.keyboard = None;
                state.xkb.clear_keymap();
            }
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for WindowState {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap { format, fd, size } => {
                if format == WEnum::Value(wl_keyboard::KeymapFormat::XkbV1) {
                    if let Err(error) = state.xkb.install_keymap(fd, size) {
                        state.event_error = Some(error.to_string());
                        state.running = false;
                    }
                }
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => state.xkb.update_modifiers(
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
            ),
            wl_keyboard::Event::Key {
                key,
                state: key_state,
                ..
            } if key_state == WEnum::Value(wl_keyboard::KeyState::Pressed) => {
                if let Some(bytes) = state.xkb.translate(key) {
                    if let Some(writer) = state.pty_writer.as_mut() {
                        if let Err(error) = writer.write_all(&bytes).and_then(|_| writer.flush()) {
                            state.event_error = Some(error.to_string());
                            state.running = false;
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for WindowState {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for WindowState {
    fn event(
        state: &mut Self,
        xdg_surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        queue_handle: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            let (width, height) = state.pending_size.take().unwrap_or(state.current_size);
            if width <= 0 || height <= 0 {
                state.surface.commit();
                return;
            }

            let size = SurfaceSize {
                width: width as u32,
                height: height as u32,
            };
            if let Err(error) = resize_pty_for_surface(&state.session, size, state.cell_metrics) {
                state.event_error = Some(error.to_string());
                state.running = false;
                return;
            }

            let dimensions_changed = state.current_size != (width, height);
            state.current_size = (width, height);
            let mut buffer_changed = false;
            if dimensions_changed || state.buffer.is_none() {
                match create_surface_buffer(&state.shm, size.width, size.height, queue_handle) {
                    Ok(new_buffer) => {
                        if let Some(old_buffer) = state.buffer.replace(new_buffer) {
                            if state.buffer_released {
                                old_buffer.destroy();
                            } else {
                                state.retired_buffers.push(old_buffer);
                            }
                        }
                        state.buffer_released = false;
                        buffer_changed = true;
                    }
                    Err(error) => {
                        state.event_error = Some(error.to_string());
                        state.running = false;
                        return;
                    }
                }
            }

            if buffer_changed {
                if let Some(buffer) = &state.buffer {
                    state.surface.attach(Some(buffer), 0, 0);
                    state.surface.damage(0, 0, width, height);
                }
            }
            state.surface.commit();
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for WindowState {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } => {
                if width > 0 && height > 0 {
                    state.pending_size = Some((width, height));
                }
            }
            xdg_toplevel::Event::Close => state.running = false,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::checked_buffer_layout;

    #[test]
    fn computes_checked_shm_layout() {
        assert_eq!(
            checked_buffer_layout(800, 600).unwrap(),
            (800, 600, 3200, 1_920_000)
        );
    }

    #[test]
    fn rejects_empty_buffer_dimensions() {
        assert!(checked_buffer_layout(0, 600).is_err());
        assert!(checked_buffer_layout(800, 0).is_err());
    }

    #[test]
    fn rejects_oversized_buffer_dimensions() {
        assert!(checked_buffer_layout(8193, 600).is_err());
        assert!(checked_buffer_layout(8192, 8192).is_err());
    }
}
