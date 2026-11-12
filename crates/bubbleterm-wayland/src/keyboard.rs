//! XKB keymap handling for native Wayland keyboard input.
//!
//! The compositor owns the active layout and modifier state. BubbleTerm loads
//! the keymap fd it sends and applies modifier snapshots from wl_keyboard;
//! it never assumes a US layout or updates modifier state from raw key events.

use std::{
    fs::File,
    io::Read,
    os::fd::OwnedFd,
};

use anyhow::{Context, Result, ensure};
use xkbcommon::xkb::{self, Context as XkbContext, Keymap, State};

const MAX_KEYMAP_BYTES: u32 = 1024 * 1024;
const XKB_EVDEV_OFFSET: u32 = 8;

/// Stateful translator for the compositor-provided XKB keymap.
pub struct XkbKeyboard {
    context: XkbContext,
    state: Option<State>,
}

impl XkbKeyboard {
    /// Creates an XKB context. A keymap is installed after wl_keyboard sends it.
    pub fn new() -> Result<Self> {
        let context = XkbContext::new(xkb::CONTEXT_NO_FLAGS)
            .context("could not create the XKB context")?;
        Ok(Self {
            context,
            state: None,
        })
    }

    /// Replaces the active keymap with the one supplied by the Wayland compositor.
    pub fn install_keymap(&mut self, fd: OwnedFd, size: u32) -> Result<()> {
        ensure!(size > 0, "Wayland supplied an empty XKB keymap");
        ensure!(
            size <= MAX_KEYMAP_BYTES,
            "Wayland XKB keymap exceeds the 1 MiB safety limit"
        );

        let mut file = File::from(fd);
        let mut bytes = Vec::with_capacity(size as usize);
        file.by_ref()
            .take(u64::from(size))
            .read_to_end(&mut bytes)
            .context("could not read the Wayland XKB keymap fd")?;
        ensure!(
            bytes.len() == size as usize,
            "Wayland XKB keymap fd ended before the advertised size"
        );

        let text = String::from_utf8(bytes)
            .context("Wayland supplied an XKB keymap that is not valid UTF-8")?;
        let text = text.trim_end_matches('\0').to_owned();
        let keymap = Keymap::new_from_string(
            &self.context,
            text,
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .context("could not compile the compositor-provided XKB keymap")?;

        self.state = Some(State::new(&keymap));
        Ok(())
    }

    /// Applies the modifier and layout snapshot sent by wl_keyboard.
    pub fn update_modifiers(
        &mut self,
        depressed: u32,
        latched: u32,
        locked: u32,
        group: u32,
    ) {
        if let Some(state) = &mut self.state {
            state.update_mask(
                depressed,
                latched,
                locked,
                0,
                0,
                group,
            );
        }
    }

    /// Converts an evdev keycode (as sent by wl_keyboard) to terminal bytes.
    /// Returns None until a valid keymap is installed or for non-text keys.
    pub fn translate(&self, evdev_keycode: u32) -> Option<Vec<u8>> {
        let keycode = evdev_keycode.checked_add(XKB_EVDEV_OFFSET)?;
        let state = self.state.as_ref()?;
        let keysym = state.key_get_one_sym(keycode);

        let sequence: &[u8] = match keysym {
            xkb::KEY_Return | xkb::KEY_KP_Enter => b"\r",
            xkb::KEY_BackSpace => b"\x7f",
            xkb::KEY_Tab => b"\t",
            xkb::KEY_Escape => b"\x1b",
            xkb::KEY_Up => b"\x1b[A",
            xkb::KEY_Down => b"\x1b[B",
            xkb::KEY_Right => b"\x1b[C",
            xkb::KEY_Left => b"\x1b[D",
            xkb::KEY_Home => b"\x1b[H",
            xkb::KEY_End => b"\x1b[F",
            xkb::KEY_Insert => b"\x1b[2~",
            xkb::KEY_Delete => b"\x1b[3~",
            xkb::KEY_Page_Up => b"\x1b[5~",
            xkb::KEY_Page_Down => b"\x1b[6~",
            _ => {
                let text = state.key_get_utf8(keycode);
                if text.is_empty() {
                    return None;
                }
                return Some(text.into_bytes());
            }
        };

        Some(sequence.to_vec())
    }

    /// Clears the active keymap when the seat loses keyboard capability.
    pub fn clear_keymap(&mut self) {
        self.state = None;
    }

    /// Whether a compositor keymap has been installed.
    pub fn is_ready(&self) -> bool {
        self.state.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_without_a_keymap() {
        let keyboard = XkbKeyboard::new().expect("XKB context should be available");
        assert!(!keyboard.is_ready());
        assert_eq!(keyboard.translate(30), None);
    }

    #[test]
    fn rejects_empty_keymap() {
        let mut keyboard = XkbKeyboard::new().expect("XKB context should be available");
        let file = tempfile::tempfile().expect("temporary file should be available");
        let fd = file.into();
        assert!(keyboard.install_keymap(fd, 0).is_err());
    }
}
