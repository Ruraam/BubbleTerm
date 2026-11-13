//! XKB keymap handling for native Wayland keyboard input.
//!
//! The compositor owns the active layout and modifier state. BubbleTerm loads
//! the keymap fd it sends and applies modifier snapshots from wl_keyboard;
//! it never assumes a US layout or updates modifier state from raw key events.

use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io::Read,
    os::fd::OwnedFd,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use xkbcommon::xkb::{self, Context as XkbContext, Keymap, State};

const MAX_KEYMAP_BYTES: u32 = 1024 * 1024;
const XKB_EVDEV_OFFSET: u32 = 8;

/// Stateful translator for the compositor-provided XKB keymap.
pub struct XkbKeyboard {
    context: XkbContext,
    keymap: Option<Keymap>,
    state: Option<State>,
    _compose_table: Option<xkb::compose::Table>,
    compose_state: Option<xkb::compose::State>,
}

impl XkbKeyboard {
    /// Creates an XKB context. A keymap is installed after wl_keyboard sends it.
    pub fn new() -> Result<Self> {
        let context = XkbContext::new(xkb::CONTEXT_NO_FLAGS);
        let locale = std::env::var_os("LC_ALL")
            .filter(|value| !value.is_empty())
            .or_else(|| std::env::var_os("LC_CTYPE").filter(|value| !value.is_empty()))
            .or_else(|| std::env::var_os("LANG").filter(|value| !value.is_empty()))
            .unwrap_or_else(|| OsString::from("C"));
        let compose_table = xkb::compose::Table::new_from_locale(
            &context,
            OsStr::new(&locale),
            xkb::compose::COMPILE_NO_FLAGS,
        )
        .ok();
        let compose_state = compose_table
            .as_ref()
            .map(|table| xkb::compose::State::new(table, xkb::compose::STATE_NO_FLAGS));

        Ok(Self {
            context,
            keymap: None,
            state: None,
            _compose_table: compose_table,
            compose_state,
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

        self.reset_compose();
        self.state = Some(State::new(&keymap));
        self.keymap = Some(keymap);
        Ok(())
    }

    /// Applies the modifier and layout snapshot sent by wl_keyboard.
    pub fn update_modifiers(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        if let Some(state) = &mut self.state {
            state.update_mask(depressed, latched, locked, 0, 0, group);
        }
    }

    /// Converts an evdev keycode (as sent by wl_keyboard) to terminal bytes.
    /// Returns None until a valid keymap is installed or for non-text keys.
    pub fn translate(&mut self, evdev_keycode: u32, application_cursor: bool) -> Option<Vec<u8>> {
        let keycode = xkb::Keycode::new(evdev_keycode.checked_add(XKB_EVDEV_OFFSET)?);
        let state = self.state.as_ref()?;
        let keysym = state.key_get_one_sym(keycode);

        let sequence: &[u8] = match keysym {
            xkb::Keysym::Return | xkb::Keysym::KP_Enter => b"\r",
            xkb::Keysym::BackSpace => b"\x7f",
            xkb::Keysym::Tab => b"\t",
            xkb::Keysym::Escape => b"\x1b",
            xkb::Keysym::Up => {
                if application_cursor {
                    b"\x1bOA"
                } else {
                    b"\x1b[A"
                }
            }
            xkb::Keysym::Down => {
                if application_cursor {
                    b"\x1bOB"
                } else {
                    b"\x1b[B"
                }
            }
            xkb::Keysym::Right => {
                if application_cursor {
                    b"\x1bOC"
                } else {
                    b"\x1b[C"
                }
            }
            xkb::Keysym::Left => {
                if application_cursor {
                    b"\x1bOD"
                } else {
                    b"\x1b[D"
                }
            }
            xkb::Keysym::Home => b"\x1b[H",
            xkb::Keysym::End => b"\x1b[F",
            xkb::Keysym::Insert => b"\x1b[2~",
            xkb::Keysym::Delete => b"\x1b[3~",
            xkb::Keysym::Page_Up => b"\x1b[5~",
            xkb::Keysym::Page_Down => b"\x1b[6~",
            _ => {
                let text = state.key_get_utf8(keycode);
                return self.translate_text(keysym, text);
            }
        };

        Some(sequence.to_vec())
    }

    /// Clears compositor modifier state when keyboard focus is lost.
    ///
    /// The compositor will send a fresh modifier snapshot after focus is
    /// entered again. Clearing here prevents a stale Shift/Ctrl/Alt/Super
    /// state from affecting input while the keyboard is unfocused.
    pub fn clear_modifiers(&mut self) {
        self.reset_compose();
        if let Some(state) = &mut self.state {
            state.update_mask(0, 0, 0, 0, 0, 0);
        }
    }

    /// Clears the active keymap when the seat loses keyboard capability.
    pub fn clear_keymap(&mut self) {
        self.reset_compose();
        self.state = None;
        self.keymap = None;
    }

    /// Translates printable keysyms through the locale Compose state machine.
    ///
    /// Dead keys and Multi_key sequences are withheld while composing. A
    /// completed sequence emits its composed UTF-8 string. If no Compose
    /// sequence matches, ordinary XKB text is returned unchanged.
    fn translate_text(&mut self, keysym: xkb::Keysym, fallback: String) -> Option<Vec<u8>> {
        let Some(compose) = self.compose_state.as_mut() else {
            return (!fallback.is_empty()).then(|| fallback.into_bytes());
        };
        compose.feed(keysym);
        match compose.status() {
            xkb::compose::Status::Composing => None,
            xkb::compose::Status::Composed => {
                let output = compose.utf8().map(String::into_bytes);
                compose.reset();
                output
            }
            xkb::compose::Status::Cancelled => {
                compose.reset();
                (!fallback.is_empty()).then(|| fallback.into_bytes())
            }
            xkb::compose::Status::Nothing => (!fallback.is_empty()).then(|| fallback.into_bytes()),
        }
    }

    /// Discards any partial dead-key or Compose sequence.
    fn reset_compose(&mut self) {
        if let Some(compose) = &mut self.compose_state {
            compose.reset();
        }
    }

    /// Whether the active XKB keymap marks this evdev key as repeatable.
    pub fn is_repeatable(&self, evdev_keycode: u32) -> bool {
        let Some(keycode) = evdev_keycode
            .checked_add(XKB_EVDEV_OFFSET)
            .map(xkb::Keycode::new)
        else {
            return false;
        };
        self.keymap
            .as_ref()
            .is_some_and(|keymap| keymap.key_repeats(keycode))
    }

    /// Whether a compositor keymap has been installed.
    pub fn is_ready(&self) -> bool {
        self.state.is_some()
    }
}

/// Client-side implementation of the repeat timing advertised by wl_keyboard.
#[derive(Debug, Default)]
pub struct KeyRepeat {
    rate: u32,
    delay: Duration,
    active_key: Option<u32>,
    next_at: Option<Instant>,
}

impl KeyRepeat {
    /// Updates the compositor-provided repeat rate and initial delay.
    /// A zero rate disables repeat; invalid negative values are treated as disabled.
    pub fn configure(&mut self, rate: i32, delay_ms: i32, now: Instant) {
        self.rate = u32::try_from(rate).unwrap_or(0);
        self.delay = Duration::from_millis(u64::try_from(delay_ms).unwrap_or(0));
        self.next_at = if self.rate > 0 && self.active_key.is_some() {
            Some(now + self.delay)
        } else {
            None
        };
        if self.rate == 0 {
            self.active_key = None;
        }
    }

    /// Starts repeating a pressed key only when XKB says it is repeatable.
    pub fn press(&mut self, key: u32, repeatable: bool, now: Instant) {
        if repeatable && self.rate > 0 {
            self.active_key = Some(key);
            self.next_at = Some(now + self.delay);
        }
        // A non-repeatable key (for example Shift or Ctrl) must not cancel
        // an already-held repeatable key. Its modifier state is applied by
        // XKB and will affect subsequent repeat translations.
    }

    /// Stops repeating when the active physical key is released.
    pub fn release(&mut self, key: u32) {
        if self.active_key == Some(key) {
            self.clear();
        }
    }

    /// Clears repeat state on focus loss, keymap replacement, or seat removal.
    pub fn clear(&mut self) {
        self.active_key = None;
        self.next_at = None;
    }

    /// Returns one due key repeat and schedules the next interval without catch-up bursts.
    pub fn take_due(&mut self, now: Instant) -> Option<u32> {
        let key = self.active_key?;
        let deadline = self.next_at?;
        if now < deadline {
            return None;
        }

        let interval = Duration::from_nanos(1_000_000_000_u64 / u64::from(self.rate).max(1));
        self.next_at = Some(now + interval.max(Duration::from_nanos(1)));
        Some(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, time::Instant};

    fn install_test_keymap(keyboard: &mut XkbKeyboard, layout: &str) {
        let keymap = Keymap::new_from_names(
            &keyboard.context,
            &"",
            &"",
            &layout,
            &"",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("test XKB keymap should compile");
        let contents = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        let mut file = tempfile::tempfile().expect("temporary keymap file should be available");
        file.write_all(contents.as_bytes())
            .expect("test keymap should be written");
        let fd = file.into();
        keyboard
            .install_keymap(fd, contents.len() as u32)
            .expect("test keymap should install");
    }

    fn keyboard_with_compose_table(contents: &str) -> XkbKeyboard {
        let mut keyboard = XkbKeyboard::new().expect("XKB context should be available");
        let table = xkb::compose::Table::new_from_buffer(
            &keyboard.context,
            contents,
            "C",
            xkb::compose::FORMAT_TEXT_V1,
            xkb::compose::COMPILE_NO_FLAGS,
        )
        .expect("test Compose table should compile");
        let state = xkb::compose::State::new(&table, xkb::compose::STATE_NO_FLAGS);
        keyboard._compose_table = Some(table);
        keyboard.compose_state = Some(state);
        keyboard
    }

    const TEST_COMPOSE_TABLE: &str = r#"
<dead_acute> <e> : "é" U00E9
<Multi_key> <o> <c> : "©" U00A9
"#;

    fn keyboard_with_layouts(layouts: &str) -> XkbKeyboard {
        let mut keyboard = XkbKeyboard::new().expect("XKB context should be available");
        let keymap = Keymap::new_from_names(
            &keyboard.context,
            &"",
            &"",
            &layouts,
            &"",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("test XKB keymap should compile");
        keyboard.state = Some(State::new(&keymap));
        keyboard.keymap = Some(keymap);
        keyboard
    }

    #[test]
    fn dead_acute_composes_with_e() {
        let mut keyboard = keyboard_with_compose_table(TEST_COMPOSE_TABLE);

        assert_eq!(
            keyboard.translate_text(xkb::Keysym::dead_acute, String::new()),
            None
        );
        assert_eq!(
            keyboard.translate_text(xkb::Keysym::e, "e".to_owned()),
            Some("é".as_bytes().to_vec())
        );
    }

    #[test]
    fn multi_key_sequence_emits_composed_unicode() {
        let mut keyboard = keyboard_with_compose_table(TEST_COMPOSE_TABLE);

        assert_eq!(
            keyboard.translate_text(xkb::Keysym::Multi_key, String::new()),
            None
        );
        assert_eq!(
            keyboard.translate_text(xkb::Keysym::o, "o".to_owned()),
            None
        );
        assert_eq!(
            keyboard.translate_text(xkb::Keysym::c, "c".to_owned()),
            Some("©".as_bytes().to_vec())
        );
    }

    #[test]
    fn cancelled_compose_sequence_falls_back_and_resets() {
        let mut keyboard = keyboard_with_compose_table(TEST_COMPOSE_TABLE);

        assert_eq!(
            keyboard.translate_text(xkb::Keysym::dead_acute, String::new()),
            None
        );
        assert_eq!(
            keyboard.translate_text(xkb::Keysym::x, "x".to_owned()),
            Some(b"x".to_vec())
        );
        assert_eq!(
            keyboard.translate_text(xkb::Keysym::e, "e".to_owned()),
            Some(b"e".to_vec())
        );
    }

    #[test]
    fn focus_loss_resets_an_incomplete_compose_sequence() {
        let mut keyboard = keyboard_with_compose_table(TEST_COMPOSE_TABLE);

        assert_eq!(
            keyboard.translate_text(xkb::Keysym::dead_acute, String::new()),
            None
        );

        keyboard.clear_modifiers();

        assert_eq!(
            keyboard.translate_text(xkb::Keysym::e, "e".to_owned()),
            Some(b"e".to_vec())
        );
    }

    #[test]
    fn resetting_partial_multi_key_sequence_prevents_stale_composition() {
        let mut keyboard = keyboard_with_compose_table(TEST_COMPOSE_TABLE);

        assert_eq!(
            keyboard.translate_text(xkb::Keysym::Multi_key, String::new()),
            None
        );
        assert_eq!(
            keyboard.translate_text(xkb::Keysym::o, "o".to_owned()),
            None
        );

        // Focus loss or a keymap transition must discard the partial sequence.
        keyboard.reset_compose();

        assert_eq!(
            keyboard.translate_text(xkb::Keysym::c, "c".to_owned()),
            Some(b"c".to_vec())
        );
    }

    #[test]
    fn compositor_group_snapshot_switches_active_layout() {
        let mut keyboard = keyboard_with_layouts("us,de");

        // Linux evdev KEY_Y (21) maps to Y in US and Z in German QWERTZ.
        assert_eq!(keyboard.translate(21, false), Some(b"y".to_vec()));

        keyboard.update_modifiers(0, 0, 0, 1);

        assert_eq!(keyboard.translate(21, false), Some(b"z".to_vec()));
    }

    #[test]
    fn altgr_modifier_snapshot_selects_third_level_unicode_symbol() {
        let mut keyboard = keyboard_with_layouts("de");
        let keymap = keyboard
            .keymap
            .as_ref()
            .expect("keymap should be installed");
        let mod5 = keymap.mod_get_index("Mod5");
        assert_ne!(mod5, xkb::MOD_INVALID);
        let altgr_mask = 1_u32 << mod5;

        // On the German layout, AltGr+E produces the Euro sign.
        keyboard.update_modifiers(altgr_mask, 0, 0, 0);

        assert_eq!(keyboard.translate(18, false), Some("€".as_bytes().to_vec()));
    }

    #[test]
    fn replacing_keymap_resets_partial_compose_and_uses_new_layout() {
        let mut keyboard = keyboard_with_compose_table(TEST_COMPOSE_TABLE);
        install_test_keymap(&mut keyboard, "us");

        assert_eq!(
            keyboard.translate_text(xkb::Keysym::dead_acute, String::new()),
            None
        );

        install_test_keymap(&mut keyboard, "de");

        // The pending dead key must not compose with input from the new map.
        assert_eq!(
            keyboard.translate_text(xkb::Keysym::e, "e".to_owned()),
            Some(b"e".to_vec())
        );
        // Linux evdev KEY_Y (21) is Z on the replacement German layout.
        assert_eq!(keyboard.translate(21, false), Some(b"z".to_vec()));
    }

    #[test]
    fn ordinary_unicode_text_is_preserved() {
        let mut keyboard = keyboard_with_compose_table(TEST_COMPOSE_TABLE);

        assert_eq!(
            keyboard.translate_text(xkb::Keysym::eacute, "é".to_owned()),
            Some("é".as_bytes().to_vec())
        );
    }

    #[test]
    fn clear_keymap_discards_compose_and_disables_translation() {
        let mut keyboard = keyboard_with_compose_table(TEST_COMPOSE_TABLE);

        assert_eq!(
            keyboard.translate_text(xkb::Keysym::dead_acute, String::new()),
            None
        );
        keyboard.clear_keymap();

        assert!(!keyboard.is_ready());
        assert_eq!(keyboard.translate(30, false), None);
        assert_eq!(
            keyboard.translate_text(xkb::Keysym::e, "e".to_owned()),
            Some(b"e".to_vec())
        );
    }

    #[test]
    fn key_repeat_obeys_delay_rate_and_release() {
        let start = Instant::now();
        let mut repeat = KeyRepeat::default();
        repeat.configure(20, 500, start);
        repeat.press(30, true, start);

        assert_eq!(repeat.take_due(start + Duration::from_millis(499)), None);
        assert_eq!(
            repeat.take_due(start + Duration::from_millis(500)),
            Some(30)
        );
        assert_eq!(repeat.take_due(start + Duration::from_millis(549)), None);
        assert_eq!(
            repeat.take_due(start + Duration::from_millis(550)),
            Some(30)
        );

        repeat.release(30);
        assert_eq!(repeat.take_due(start + Duration::from_secs(2)), None);
    }

    #[test]
    fn key_repeat_ignores_non_repeatable_keys_and_zero_rate() {
        let start = Instant::now();
        let mut repeat = KeyRepeat::default();
        repeat.configure(25, 300, start);
        repeat.press(42, false, start);
        assert_eq!(repeat.take_due(start + Duration::from_secs(1)), None);

        repeat.press(30, true, start);
        repeat.configure(0, 300, start);
        assert_eq!(repeat.take_due(start + Duration::from_secs(1)), None);
    }

    #[test]
    fn latest_repeatable_key_owns_repeat_until_released() {
        let start = Instant::now();
        let mut repeat = KeyRepeat::default();
        repeat.configure(20, 300, start);
        repeat.press(30, true, start);
        repeat.press(31, true, start + Duration::from_millis(100));

        // Releasing the older key must not cancel the newer key's repeat.
        repeat.release(30);
        assert_eq!(
            repeat.take_due(start + Duration::from_millis(400)),
            Some(31)
        );

        // Releasing the active key stops repeat; an older held key is not
        // resurrected because only the most recently pressed key repeats.
        repeat.release(31);
        assert_eq!(repeat.take_due(start + Duration::from_secs(1)), None);
    }

    #[test]
    fn modifier_press_does_not_cancel_an_active_repeat() {
        let start = Instant::now();
        let mut repeat = KeyRepeat::default();
        repeat.configure(20, 400, start);
        repeat.press(30, true, start);
        repeat.press(42, false, start + Duration::from_millis(100));

        assert_eq!(
            repeat.take_due(start + Duration::from_millis(400)),
            Some(30)
        );
        repeat.release(42);
        assert_eq!(
            repeat.take_due(start + Duration::from_millis(450)),
            Some(30)
        );
    }

    #[test]
    fn reconfiguring_repeat_restarts_the_initial_delay() {
        let start = Instant::now();
        let reconfigured_at = start + Duration::from_millis(100);
        let mut repeat = KeyRepeat::default();
        repeat.configure(20, 500, start);
        repeat.press(30, true, start);

        assert_eq!(repeat.take_due(start + Duration::from_millis(499)), None);

        // A compositor update while the key is held starts a fresh delay.
        repeat.configure(10, 250, reconfigured_at);
        assert_eq!(
            repeat.take_due(reconfigured_at + Duration::from_millis(249)),
            None
        );
        assert_eq!(
            repeat.take_due(reconfigured_at + Duration::from_millis(250)),
            Some(30)
        );
        assert_eq!(
            repeat.take_due(reconfigured_at + Duration::from_millis(349)),
            None
        );
        assert_eq!(
            repeat.take_due(reconfigured_at + Duration::from_millis(350)),
            Some(30)
        );
    }

    #[test]
    fn negative_repeat_rate_disables_and_clears_active_key() {
        let start = Instant::now();
        let mut repeat = KeyRepeat::default();
        repeat.configure(20, 300, start);
        repeat.press(30, true, start);

        repeat.configure(-1, 100, start + Duration::from_millis(50));

        assert_eq!(repeat.take_due(start + Duration::from_secs(1)), None);

        // Re-enabling repeat must not resurrect the key cleared by disable.
        repeat.configure(20, 300, start + Duration::from_secs(1));
        assert_eq!(repeat.take_due(start + Duration::from_secs(2)), None);
    }

    #[test]
    fn releasing_an_unrelated_key_preserves_active_repeat() {
        let start = Instant::now();
        let mut repeat = KeyRepeat::default();
        repeat.configure(20, 300, start);
        repeat.press(30, true, start);
        repeat.release(31);

        assert_eq!(
            repeat.take_due(start + Duration::from_millis(300)),
            Some(30)
        );
    }

    #[test]
    fn negative_repeat_delay_is_clamped_to_zero() {
        let start = Instant::now();
        let mut repeat = KeyRepeat::default();
        repeat.configure(20, -1, start);
        repeat.press(30, true, start);

        assert_eq!(repeat.take_due(start), Some(30));
    }

    #[test]
    fn repeat_configuration_without_active_key_does_not_schedule_input() {
        let start = Instant::now();
        let mut repeat = KeyRepeat::default();
        repeat.configure(20, 0, start);

        assert_eq!(repeat.take_due(start + Duration::from_secs(1)), None);

        repeat.press(30, true, start + Duration::from_secs(1));
        assert_eq!(repeat.take_due(start + Duration::from_secs(1)), Some(30));
        assert_eq!(repeat.take_due(start + Duration::from_secs(2)), Some(30));
    }

    #[test]
    fn starts_without_a_keymap() {
        let mut keyboard = XkbKeyboard::new().expect("XKB context should be available");
        assert!(!keyboard.is_ready());
        assert_eq!(keyboard.translate(30, false), None);
    }

    #[test]
    fn rejects_empty_keymap() {
        let mut keyboard = XkbKeyboard::new().expect("XKB context should be available");
        let file = tempfile::tempfile().expect("temporary file should be available");
        let fd = file.into();
        assert!(keyboard.install_keymap(fd, 0).is_err());
    }
}
