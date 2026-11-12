//! Native Wayland window lifecycle and PTY resize integration.
//!
//! This module creates an xdg-toplevel surface, acknowledges compositor
//! configure events, and propagates the committed logical surface size to the
//! active PTY. Rendering is currently a neutral SHM placeholder; terminal
//! cell rendering and fractional-scale support are separate follow-up work.

use std::io::Write;
use std::os::fd::AsFd;

use anyhow::{Context, Result, bail};
use bubbleterm_pty::PtySession;
use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface},
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

use crate::{CellMetrics, SurfaceSize, resize_pty_for_surface};

const INITIAL_WIDTH: u32 = 800;
const INITIAL_HEIGHT: u32 = 600;
const WINDOW_TITLE: &str = "BubbleTerm";

/// Runs one native Wayland window until the compositor requests it to close.
///
/// The caller retains ownership of the PTY session. Configure sizes are
/// interpreted as logical pixels at scale factor 1 until output/fractional
/// scaling is implemented.
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

    let surface = compositor.create_surface(&queue_handle, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &queue_handle, ());
    let toplevel = xdg_surface.get_toplevel(&queue_handle, ());
    toplevel.set_title(WINDOW_TITLE.to_owned());
    toplevel.set_app_id("org.ruraam.BubbleTerm".to_owned());

    let buffer = create_placeholder_buffer(&shm, &queue_handle)?;
    let mut state = WindowState {
        session,
        cell_metrics,
        surface,
        buffer,
        pending_size: None,
        configured: false,
        running: true,
        resize_error: None,
    };

    // The first commit has no buffer. xdg-shell requires the compositor to
    // answer with a configure before the client maps its content.
    state.surface.commit();

    while state.running {
        event_queue
            .blocking_dispatch(&mut state)
            .context("Wayland event dispatch failed")?;
        if let Some(error) = state.resize_error.take() {
            bail!("could not propagate Wayland resize to PTY: {error}");
        }
    }

    Ok(())
}

fn create_placeholder_buffer(
    shm: &wl_shm::WlShm,
    queue_handle: &QueueHandle<WindowState>,
) -> Result<wl_buffer::WlBuffer> {
    let byte_len = INITIAL_WIDTH
        .checked_mul(INITIAL_HEIGHT)
        .and_then(|pixels| pixels.checked_mul(4))
        .context("initial Wayland buffer dimensions overflow")?;
    let mut file = tempfile::tempfile().context("could not create Wayland SHM backing file")?;
    let pixel = [0x18_u8, 0x18, 0x20, 0xff];
    let mut row = Vec::with_capacity(INITIAL_WIDTH as usize * 4);
    for _ in 0..INITIAL_WIDTH {
        row.extend_from_slice(&pixel);
    }
    for _ in 0..INITIAL_HEIGHT {
        file.write_all(&row)?;
    }
    file.flush()?;

    let pool = shm.create_pool(file.as_fd(), byte_len as i32, queue_handle, ());
    let buffer = pool.create_buffer(
        0,
        INITIAL_WIDTH as i32,
        INITIAL_HEIGHT as i32,
        (INITIAL_WIDTH * 4) as i32,
        wl_shm::Format::Argb8888,
        queue_handle,
        (),
    );
    pool.destroy();
    Ok(buffer)
}

struct WindowState {
    session: PtySession,
    cell_metrics: CellMetrics,
    surface: wl_surface::WlSurface,
    buffer: wl_buffer::WlBuffer,
    pending_size: Option<(i32, i32)>,
    configured: bool,
    running: bool,
    resize_error: Option<String>,
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
delegate_noop!(WindowState: ignore wl_buffer::WlBuffer);
delegate_noop!(WindowState: ignore wl_surface::WlSurface);

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
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            let (width, height) = state.pending_size.take().unwrap_or((
                INITIAL_WIDTH as i32,
                INITIAL_HEIGHT as i32,
            ));
            if width > 0 && height > 0 {
                let size = SurfaceSize {
                    width: width as u32,
                    height: height as u32,
                };
                if let Err(error) =
                    resize_pty_for_surface(state.session, size, state.cell_metrics)
                {
                    state.resize_error = Some(error.to_string());
                    state.running = false;
                    return;
                }
            }
            if !state.configured {
                state.surface.attach(Some(&state.buffer), 0, 0);
                state.surface.damage(0, 0, i32::MAX, i32::MAX);
                state.configured = true;
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
