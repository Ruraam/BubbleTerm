//! Native Wayland window lifecycle, keyboard input, and PTY resize integration.
//!
//! The surface uses dynamically-sized shared-memory buffers rendered from VT
//! snapshots. Keyboard input is translated using the compositor-provided XKB
//! keymap. Fractional scaling and a full Unicode font remain future work.

use std::{io::Write, os::fd::AsFd, sync::mpsc::TryRecvError, time::Instant};

use anyhow::{Context, Result, bail, ensure};
use bubbleterm_pty::{PtyOutputEvent, PtyOutputReader, PtySession};
use bubbleterm_vt::{GridSize, Terminal};
use nix::poll::{PollFd, PollFlags, poll};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer, wl_compositor, wl_keyboard, wl_registry, wl_seat, wl_shm, wl_shm_pool,
        wl_surface,
    },
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

use crate::{
    CellMetrics, SurfaceSize,
    keyboard::{KeyRepeat, XkbKeyboard},
    renderer::render_snapshot,
    resize_pty_for_surface,
};

const INITIAL_WIDTH: u32 = 800;
const INITIAL_HEIGHT: u32 = 600;
const MAX_BUFFER_BYTES: u32 = 64 * 1024 * 1024;
const MAX_BUFFER_DIMENSION: u32 = 8192;
const WINDOW_TITLE: &str = "BubbleTerm";

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

    let initial_pty_size = session
        .size()
        .context("could not read initial PTY dimensions")?;
    let terminal = Terminal::new(
        GridSize {
            columns: initial_pty_size.cols,
            rows: initial_pty_size.rows,
        },
        10_000,
    )
    .context("could not initialize the VT parser")?;
    let pty_output =
        PtyOutputReader::spawn(&session).context("could not start the PTY output reader")?;

    let mut state = WindowState {
        session,
        pty_output,
        terminal,
        pty_closed: false,
        pty_writer: None,
        cell_metrics,
        shm,
        surface,
        keyboard: None,
        keyboard_lifecycle: KeyboardLifecycle::default(),
        xkb: XkbKeyboard::new()?,
        key_repeat: KeyRepeat::default(),
        buffer: None,
        buffer_released: false,
        retired_buffers: Vec::new(),
        pending_size: None,
        current_size: (INITIAL_WIDTH as i32, INITIAL_HEIGHT as i32),
        running: true,
        configured: false,
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
            .dispatch_pending(&mut state)
            .context("Wayland pending-event dispatch failed")?;
        dispatch_key_repeat(&mut state);
        if drain_pty_output(&mut state) && state.configured {
            let (width, height) = state.current_size;
            if width > 0 && height > 0 {
                present_surface_buffer(&mut state, width as u32, height as u32, &queue_handle)?;
            }
        }
        if let Some(error) = state.event_error.take() {
            bail!("Wayland window event failed: {error}");
        }

        event_queue
            .flush()
            .context("Wayland request flush failed")?;
        let Some(read_guard) = event_queue.prepare_read() else {
            continue;
        };
        let mut poll_fds = [PollFd::new(read_guard.connection_fd(), PollFlags::POLLIN)];
        let poll_result =
            poll(&mut poll_fds, 8_u16).context("could not poll the Wayland connection")?;
        let socket_ready = poll_result > 0
            && poll_fds[0].revents().is_some_and(|flags| {
                flags.intersects(PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR)
            });

        if socket_ready {
            read_guard.read().context("could not read Wayland events")?;
        }
        // Dropping the guard on timeout cancels the prepared read. PTY output
        // is drained on the next iteration without blocking the UI thread.
    }

    // Keep the bound seat alive for as long as its keyboard proxy may exist.
    drop(seat);
    Ok(())
}

/// Emit at most one due repeat per event-loop iteration.
fn dispatch_key_repeat(state: &mut WindowState) {
    let Some(key) = state.key_repeat.take_due(Instant::now()) else {
        return;
    };
    let application_cursor = state.terminal.application_cursor();
    let Some(bytes) = state.xkb.translate(key, application_cursor) else {
        return;
    };
    if let Some(writer) = state.pty_writer.as_mut() {
        if let Err(error) = writer.write_all(&bytes).and_then(|_| writer.flush()) {
            state.event_error = Some(error.to_string());
            state.running = false;
        }
    }
}

/// Drain all currently available PTY output into the VT parser.
///
/// This function is called only on the Wayland thread and never waits for
/// additional PTY data. The reader worker owns all blocking reads.
fn drain_pty_output(state: &mut WindowState) -> bool {
    let mut changed = false;
    loop {
        match state.pty_output.try_recv() {
            Ok(PtyOutputEvent::Data(bytes)) => {
                state.terminal.process(&bytes);
                changed = true;
            }
            Ok(PtyOutputEvent::Closed) => {
                state.pty_closed = true;
                break;
            }
            Ok(PtyOutputEvent::Error(error)) => {
                state.event_error = Some(format!("PTY output reader failed: {error}"));
                state.running = false;
                break;
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
        }
    }
    changed
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
    pixels: &[u8],
    queue_handle: &QueueHandle<WindowState>,
) -> Result<wl_buffer::WlBuffer> {
    let (width_i32, height_i32, stride_i32, pool_size) = checked_buffer_layout(width, height)?;
    ensure!(
        pixels.len() == pool_size as usize,
        "rendered pixel buffer has an invalid length"
    );

    let mut file = tempfile::tempfile().context("could not create Wayland SHM backing file")?;
    file.write_all(pixels)?;
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

/// Render the current VT snapshot and publish a fresh immutable SHM buffer.
fn present_surface_buffer(
    state: &mut WindowState,
    width: u32,
    height: u32,
    queue_handle: &QueueHandle<WindowState>,
) -> Result<()> {
    let snapshot = state.terminal.snapshot();
    let pixels = render_snapshot(&snapshot, width, height, state.cell_metrics)?;
    let new_buffer = create_surface_buffer(&state.shm, width, height, &pixels, queue_handle)?;

    if let Some(old_buffer) = state.buffer.replace(new_buffer) {
        if state.buffer_released {
            old_buffer.destroy();
        } else {
            state.retired_buffers.push(old_buffer);
        }
    }
    state.buffer_released = false;
    if let Some(buffer) = &state.buffer {
        state.surface.attach(Some(buffer), 0, 0);
        state.surface.damage(0, 0, width as i32, height as i32);
    }
    state.surface.commit();
    Ok(())
}

struct WindowState {
    session: PtySession,
    pty_output: PtyOutputReader,
    terminal: Terminal,
    pty_closed: bool,
    pty_writer: Option<Box<dyn Write + Send>>,
    cell_metrics: CellMetrics,
    shm: wl_shm::WlShm,
    surface: wl_surface::WlSurface,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    keyboard_lifecycle: KeyboardLifecycle,
    xkb: XkbKeyboard,
    key_repeat: KeyRepeat,
    buffer: Option<wl_buffer::WlBuffer>,
    buffer_released: bool,
    retired_buffers: Vec<wl_buffer::WlBuffer>,
    pending_size: Option<(i32, i32)>,
    current_size: (i32, i32),
    running: bool,
    configured: bool,
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

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct KeyboardLifecycle {
    available: bool,
    focused: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CapabilityChange {
    Added,
    Removed,
    Unchanged,
}

impl KeyboardLifecycle {
    fn update_capability(&mut self, available: bool) -> CapabilityChange {
        if self.available == available {
            return CapabilityChange::Unchanged;
        }
        self.available = available;
        if available {
            CapabilityChange::Added
        } else {
            // A removed keyboard cannot retain focus.
            self.focused = false;
            CapabilityChange::Removed
        }
    }

    fn enter(&mut self) -> bool {
        if !self.available || self.focused {
            return false;
        }
        self.focused = true;
        true
    }

    fn leave(&mut self) -> bool {
        std::mem::replace(&mut self.focused, false)
    }

    /// Whether keyboard input may be consumed for the currently focused surface.
    fn accepts_input(&self) -> bool {
        self.available && self.focused
    }
}

fn supports_xkb_keymap(format: WEnum<wl_keyboard::KeymapFormat>) -> bool {
    format == WEnum::Value(wl_keyboard::KeymapFormat::XkbV1)
}

fn has_keyboard_capability(capabilities: WEnum<wl_seat::Capability>) -> bool {
    match capabilities {
        WEnum::Value(capabilities) => capabilities.contains(wl_seat::Capability::Keyboard),
        WEnum::Unknown(raw) => raw & 0b010 != 0,
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
            match state
                .keyboard_lifecycle
                .update_capability(has_keyboard_capability(capabilities))
            {
                CapabilityChange::Added => {
                    state.keyboard = Some(seat.get_keyboard(queue_handle, ()));
                }
                CapabilityChange::Removed => {
                    state.keyboard = None;
                    state.xkb.clear_keymap();
                    state.key_repeat.clear();
                }
                CapabilityChange::Unchanged => {}
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
                // A destroyed keyboard proxy may still have queued events.
                // Do not let those events restore a keymap after seat removal.
                if !state.keyboard_lifecycle.available {
                    return;
                }
                if supports_xkb_keymap(format) {
                    state.key_repeat.clear();
                    if let Err(error) = state.xkb.install_keymap(fd, size) {
                        state.event_error = Some(error.to_string());
                        state.running = false;
                    }
                } else {
                    // NoKeymap and any future/unknown format are unsupported.
                    // Never keep translating with a stale compositor keymap.
                    state.key_repeat.clear();
                    state.xkb.clear_keymap();
                }
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if state.keyboard_lifecycle.accepts_input() {
                    state
                        .xkb
                        .update_modifiers(mods_depressed, mods_latched, mods_locked, group);
                }
            }
            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                if state.keyboard_lifecycle.available {
                    state.key_repeat.configure(rate, delay, Instant::now());
                }
            }
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(wl_keyboard::KeyState::Pressed),
                ..
            } => {
                if state.keyboard_lifecycle.accepts_input() {
                    let application_cursor = state.terminal.application_cursor();
                    if let Some(bytes) = state.xkb.translate(key, application_cursor) {
                        if let Some(writer) = state.pty_writer.as_mut() {
                            if let Err(error) =
                                writer.write_all(&bytes).and_then(|_| writer.flush())
                            {
                                state.event_error = Some(error.to_string());
                                state.running = false;
                            }
                        }
                    }
                    state
                        .key_repeat
                        .press(key, state.xkb.is_repeatable(key), Instant::now());
                }
            }
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(wl_keyboard::KeyState::Released),
                ..
            } => {
                if state.keyboard_lifecycle.accepts_input() {
                    state.key_repeat.release(key);
                }
            }
            wl_keyboard::Event::Enter { .. } => {
                // A fresh modifier snapshot follows focus entry. Do not carry
                // repeat state across surfaces or duplicate Enter events.
                if state.keyboard_lifecycle.enter() {
                    state.key_repeat.clear();
                }
            }
            wl_keyboard::Event::Leave { .. } if state.keyboard_lifecycle.leave() => {
                state.key_repeat.clear();
                state.xkb.clear_modifiers();
            }
            wl_keyboard::Event::Leave { .. } => {}
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
            match resize_pty_for_surface(&state.session, size, state.cell_metrics) {
                Ok(Some(pty_size)) => {
                    if let Err(error) = state.terminal.resize(GridSize {
                        columns: pty_size.cols,
                        rows: pty_size.rows,
                    }) {
                        state.event_error = Some(error.to_string());
                        state.running = false;
                        return;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    state.event_error = Some(error.to_string());
                    state.running = false;
                    return;
                }
            }

            state.current_size = (width, height);
            state.configured = true;
            if let Err(error) = present_surface_buffer(state, size.width, size.height, queue_handle)
            {
                state.event_error = Some(error.to_string());
                state.running = false;
            }
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
    use super::{
        CapabilityChange, KeyboardLifecycle, checked_buffer_layout, has_keyboard_capability,
        supports_xkb_keymap,
    };
    use wayland_client::WEnum;
    use wayland_client::protocol::{wl_keyboard, wl_seat};

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

    #[test]
    fn keyboard_capability_transitions_are_idempotent() {
        let mut lifecycle = KeyboardLifecycle::default();
        assert_eq!(lifecycle.update_capability(true), CapabilityChange::Added);
        assert_eq!(
            lifecycle.update_capability(true),
            CapabilityChange::Unchanged
        );
        assert_eq!(
            lifecycle.update_capability(false),
            CapabilityChange::Removed
        );
        assert_eq!(
            lifecycle.update_capability(false),
            CapabilityChange::Unchanged
        );
    }

    #[test]
    fn focus_transitions_require_an_available_keyboard() {
        let mut lifecycle = KeyboardLifecycle::default();
        assert!(!lifecycle.enter());
        assert_eq!(lifecycle.update_capability(true), CapabilityChange::Added);
        assert!(lifecycle.enter());
        assert!(!lifecycle.enter());
        assert!(lifecycle.leave());
        assert!(!lifecycle.leave());
    }

    #[test]
    fn input_is_accepted_only_while_keyboard_is_available_and_focused() {
        let mut lifecycle = KeyboardLifecycle::default();
        assert!(!lifecycle.accepts_input());

        lifecycle.update_capability(true);
        assert!(!lifecycle.accepts_input());

        assert!(lifecycle.enter());
        assert!(lifecycle.accepts_input());

        lifecycle.leave();
        assert!(!lifecycle.accepts_input());

        lifecycle.update_capability(false);
        assert!(!lifecycle.accepts_input());
    }

    #[test]
    fn removing_keyboard_capability_discards_focus() {
        let mut lifecycle = KeyboardLifecycle::default();
        lifecycle.update_capability(true);
        assert!(lifecycle.enter());
        assert_eq!(
            lifecycle.update_capability(false),
            CapabilityChange::Removed
        );
        assert!(!lifecycle.leave());
        assert!(!lifecycle.enter());
    }

    #[test]
    fn rejects_missing_and_unknown_keymap_formats() {
        assert!(supports_xkb_keymap(WEnum::Value(
            wl_keyboard::KeymapFormat::XkbV1
        )));
        assert!(!supports_xkb_keymap(WEnum::Value(
            wl_keyboard::KeymapFormat::NoKeymap
        )));
        assert!(!supports_xkb_keymap(WEnum::Unknown(2)));
    }

    #[test]
    fn detects_keyboard_capability_in_known_and_unknown_masks() {
        assert!(has_keyboard_capability(WEnum::Value(
            wl_seat::Capability::Keyboard
        )));
        assert!(!has_keyboard_capability(WEnum::Value(
            wl_seat::Capability::Pointer
        )));
        assert!(has_keyboard_capability(WEnum::Unknown(0b010)));
        assert!(!has_keyboard_capability(WEnum::Unknown(0b001)));
    }
}
