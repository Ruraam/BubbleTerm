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
const MAX_KEY_REPEAT_RATE: u32 = 1_000;
const XKB_EVDEV_OFFSET: u32 = 8;

/// Returns the xterm modifier parameter (1 means no modifiers).
fn xterm_modifier_parameter(state: &xkb::State) -> u8 {
    let mut modifier = 1;
    for (name, value) in [("Shift", 1), ("Mod1", 2), ("Control", 4), ("Mod4", 8)] {
        if state.mod_name_is_active(name, xkb::STATE_MODS_EFFECTIVE) {
            modifier += value;
        }
    }
    modifier
}

/// Builds modifier-aware cursor and navigation key sequences.
fn navigation_key_sequence(
    keysym: xkb::Keysym,
    state: &xkb::State,
    application_cursor: bool,
) -> Option<Vec<u8>> {
    let modifier = xterm_modifier_parameter(state);
    let (number, final_byte, tilde_form) = match keysym {
        xkb::Keysym::Up => (1, b'A', false),
        xkb::Keysym::Down => (1, b'B', false),
        xkb::Keysym::Right => (1, b'C', false),
        xkb::Keysym::Left => (1, b'D', false),
        xkb::Keysym::Home => (1, b'H', false),
        xkb::Keysym::End => (1, b'F', false),
        xkb::Keysym::Insert => (2, 0, true),
        xkb::Keysym::Delete => (3, 0, true),
        xkb::Keysym::Page_Up => (5, 0, true),
        xkb::Keysym::Page_Down => (6, 0, true),
        _ => return None,
    };

    if modifier == 1 {
        return Some(match keysym {
            xkb::Keysym::Up if application_cursor => b"\x1bOA".to_vec(),
            xkb::Keysym::Down if application_cursor => b"\x1bOB".to_vec(),
            xkb::Keysym::Right if application_cursor => b"\x1bOC".to_vec(),
            xkb::Keysym::Left if application_cursor => b"\x1bOD".to_vec(),
            _ if tilde_form => format!("\x1b[{number}~").into_bytes(),
            _ => vec![0x1b, b'[', final_byte],
        });
    }

    Some(if tilde_form {
        format!("\x1b[{number};{modifier}~").into_bytes()
    } else {
        format!("\x1b[1;{modifier}{}", final_byte as char).into_bytes()
    })
}

/// Builds xterm-compatible function-key sequences, including modifier parameters.
fn function_key_sequence(keysym: xkb::Keysym, state: &xkb::State) -> Option<Vec<u8>> {
    let (number, final_byte, tilde_form) = match keysym {
        xkb::Keysym::F1 => (1, b'P', false),
        xkb::Keysym::F2 => (1, b'Q', false),
        xkb::Keysym::F3 => (1, b'R', false),
        xkb::Keysym::F4 => (1, b'S', false),
        xkb::Keysym::F5 => (15, 0, true),
        xkb::Keysym::F6 => (17, 0, true),
        xkb::Keysym::F7 => (18, 0, true),
        xkb::Keysym::F8 => (19, 0, true),
        xkb::Keysym::F9 => (20, 0, true),
        xkb::Keysym::F10 => (21, 0, true),
        xkb::Keysym::F11 => (23, 0, true),
        xkb::Keysym::F12 => (24, 0, true),
        _ => return None,
    };

    let mut modifier = 1;
    for (name, value) in [("Shift", 1), ("Mod1", 2), ("Control", 4), ("Mod4", 8)] {
        if state.mod_name_is_active(name, xkb::STATE_MODS_EFFECTIVE) {
            modifier += value;
        }
    }

    if modifier == 1 {
        return Some(if tilde_form {
            format!("\x1b[{number}~").into_bytes()
        } else {
            vec![0x1b, b'O', final_byte]
        });
    }

    Some(if tilde_form {
        format!("\x1b[{number};{modifier}~").into_bytes()
    } else {
        format!("\x1b[{number};{modifier}{}", final_byte as char).into_bytes()
    })
}

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

        if let Some(sequence) = navigation_key_sequence(keysym, state, application_cursor) {
            return Some(sequence);
        }

        if let Some(sequence) = function_key_sequence(keysym, state) {
            return Some(sequence);
        }

        let sequence: &[u8] = match keysym {
            xkb::Keysym::Return | xkb::Keysym::KP_Enter => b"\r",
            xkb::Keysym::BackSpace => b"\x7f",
            xkb::Keysym::Tab => b"\t",
            xkb::Keysym::Escape => b"\x1b",
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
        // Treat malformed compositor values as disabled or cap extreme rates.
        // This prevents a zero-length repeat interval and a busy event loop.
        self.rate = u32::try_from(rate).unwrap_or(0).min(MAX_KEY_REPEAT_RATE);
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
    use std::{
        io::{Seek, SeekFrom, Write},
        time::Instant,
    };

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
        file.seek(SeekFrom::Start(0))
            .expect("test keymap file should rewind");
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
    fn modifier_snapshots_apply_depressed_latched_and_locked_masks() {
        let mut keyboard = keyboard_with_layouts("us");
        let keymap = keyboard
            .keymap
            .as_ref()
            .expect("keymap should be installed");
        let shift = keymap.mod_get_index("Shift");
        assert_ne!(shift, xkb::MOD_INVALID);
        let shift_mask = 1_u32 << shift;

        // A depressed modifier affects the current key state.
        keyboard.update_modifiers(shift_mask, 0, 0, 0);
        assert_eq!(keyboard.translate(30, false), Some(b"A".to_vec()));

        // A fresh compositor snapshot replaces the previous depressed state.
        keyboard.update_modifiers(0, 0, 0, 0);
        assert_eq!(keyboard.translate(30, false), Some(b"a".to_vec()));

        // Latched and locked modifiers also contribute to the effective mask.
        keyboard.update_modifiers(0, shift_mask, 0, 0);
        assert_eq!(keyboard.translate(30, false), Some(b"A".to_vec()));
        keyboard.update_modifiers(0, 0, shift_mask, 0);
        assert_eq!(keyboard.translate(30, false), Some(b"A".to_vec()));

        // Clearing the locked mask must restore the unmodified symbol.
        keyboard.update_modifiers(0, 0, 0, 0);
        assert_eq!(keyboard.translate(30, false), Some(b"a".to_vec()));
    }

    #[test]
    fn group_snapshot_change_preserves_an_active_compose_sequence() {
        let mut keyboard = keyboard_with_compose_table(TEST_COMPOSE_TABLE);
        install_test_keymap(&mut keyboard, "us,de");

        assert_eq!(
            keyboard.translate_text(xkb::Keysym::dead_acute, String::new()),
            None
        );

        // The compositor can report a layout change between two key events.
        keyboard.update_modifiers(0, 0, 0, 1);

        assert_eq!(
            keyboard.translate_text(xkb::Keysym::e, "e".to_owned()),
            Some("é".as_bytes().to_vec())
        );
    }

    #[test]
    fn shift_and_altgr_snapshot_selects_the_combined_level() {
        let mut keyboard = keyboard_with_layouts("de");
        let keymap = keyboard
            .keymap
            .as_ref()
            .expect("keymap should be installed");
        let shift = keymap.mod_get_index("Shift");
        let mod5 = keymap.mod_get_index("Mod5");
        assert_ne!(shift, xkb::MOD_INVALID);
        assert_ne!(mod5, xkb::MOD_INVALID);

        let combined_mask = (1_u32 << shift) | (1_u32 << mod5);
        keyboard.update_modifiers(combined_mask, 0, 0, 0);

        // Shift must not cancel AltGr: German AltGr+Shift+E remains Euro.
        assert_eq!(keyboard.translate(18, false), Some("€".as_bytes().to_vec()));
    }

    #[test]
    fn changing_group_reinterprets_an_active_altgr_snapshot() {
        let mut keyboard = keyboard_with_layouts("us,de");
        let keymap = keyboard
            .keymap
            .as_ref()
            .expect("keymap should be installed");
        let mod5 = keymap.mod_get_index("Mod5");
        assert_ne!(mod5, xkb::MOD_INVALID);
        let altgr_mask = 1_u32 << mod5;

        keyboard.update_modifiers(altgr_mask, 0, 0, 1);
        assert_eq!(keyboard.translate(18, false), Some("€".as_bytes().to_vec()));

        // The compositor's next snapshot changes only the group; AltGr stays active.
        keyboard.update_modifiers(altgr_mask, 0, 0, 0);
        assert_eq!(keyboard.translate(18, false), Some(b"e".to_vec()));
    }

    #[test]
    fn focus_reset_clears_shift_altgr_and_locked_modifier_state() {
        let mut keyboard = keyboard_with_layouts("de");
        let keymap = keyboard
            .keymap
            .as_ref()
            .expect("keymap should be installed");
        let shift = keymap.mod_get_index("Shift");
        let mod5 = keymap.mod_get_index("Mod5");
        assert_ne!(shift, xkb::MOD_INVALID);
        assert_ne!(mod5, xkb::MOD_INVALID);

        let combined_mask = (1_u32 << shift) | (1_u32 << mod5);
        keyboard.update_modifiers(combined_mask, 0, 0, 0);
        assert_eq!(keyboard.translate(18, false), Some("€".as_bytes().to_vec()));

        keyboard.clear_modifiers();

        assert_eq!(keyboard.translate(18, false), Some(b"e".to_vec()));
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
    fn excessive_repeat_rate_is_capped_to_a_safe_interval() {
        let start = Instant::now();
        let mut repeat = KeyRepeat::default();
        repeat.configure(i32::MAX, 0, start);
        repeat.press(30, true, start);

        assert_eq!(repeat.take_due(start), Some(30));
        assert_eq!(repeat.take_due(start + Duration::from_micros(999)), None);
        assert_eq!(repeat.take_due(start + Duration::from_millis(1)), Some(30));
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
    fn special_keys_emit_expected_terminal_sequences() {
        let mut keyboard = keyboard_with_layouts("us");
        let cases: &[(u32, &[u8])] = &[
            (28, b"\r"),       // Enter
            (96, b"\r"),       // Keypad Enter
            (14, b"\x7f"),     // Backspace
            (15, b"\t"),       // Tab
            (1, b"\x1b"),      // Escape
            (102, b"\x1b[H"),  // Home
            (107, b"\x1b[F"),  // End
            (110, b"\x1b[2~"), // Insert
            (111, b"\x1b[3~"), // Delete
            (104, b"\x1b[5~"), // Page Up
            (109, b"\x1b[6~"), // Page Down
        ];

        for (evdev_keycode, expected) in cases {
            assert_eq!(
                keyboard.translate(*evdev_keycode, false).as_deref(),
                Some(*expected),
                "unexpected sequence for evdev keycode {evdev_keycode}"
            );
        }
    }

    #[test]
    fn arrow_keys_respect_application_cursor_mode() {
        let mut keyboard = keyboard_with_layouts("us");
        let cases: &[(u32, &[u8], &[u8])] = &[
            (103, b"\x1b[A", b"\x1bOA"), // Up
            (108, b"\x1b[B", b"\x1bOB"), // Down
            (106, b"\x1b[C", b"\x1bOC"), // Right
            (105, b"\x1b[D", b"\x1bOD"), // Left
        ];

        for (evdev_keycode, normal, application) in cases {
            assert_eq!(
                keyboard.translate(*evdev_keycode, false).as_deref(),
                Some(*normal),
                "unexpected normal cursor sequence for evdev keycode {evdev_keycode}"
            );
            assert_eq!(
                keyboard.translate(*evdev_keycode, true).as_deref(),
                Some(*application),
                "unexpected application cursor sequence for evdev keycode {evdev_keycode}"
            );
        }
    }

    #[test]
    fn function_keys_emit_expected_xterm_sequences() {
        let mut keyboard = keyboard_with_layouts("us");
        let cases: &[(u32, &[u8])] = &[
            (59, b"\x1bOP"),   // F1
            (60, b"\x1bOQ"),   // F2
            (61, b"\x1bOR"),   // F3
            (62, b"\x1bOS"),   // F4
            (63, b"\x1b[15~"), // F5
            (64, b"\x1b[17~"), // F6
            (65, b"\x1b[18~"), // F7
            (66, b"\x1b[19~"), // F8
            (67, b"\x1b[20~"), // F9
            (68, b"\x1b[21~"), // F10
            (87, b"\x1b[23~"), // F11
            (88, b"\x1b[24~"), // F12
        ];

        for (evdev_keycode, expected) in cases {
            assert_eq!(
                keyboard.translate(*evdev_keycode, false).as_deref(),
                Some(*expected),
                "unexpected function-key sequence for evdev keycode {evdev_keycode}"
            );
        }
    }

    #[test]
    fn navigation_keys_encode_shift_ctrl_alt_and_super_modifiers() {
        let mut keyboard = keyboard_with_layouts("us");
        let keymap = keyboard
            .keymap
            .as_ref()
            .expect("keymap should be installed");
        let modifier_mask = |name: &str| {
            let index = keymap.mod_get_index(name);
            assert_ne!(index, xkb::MOD_INVALID, "missing {name} modifier");
            1_u32 << index
        };
        let shift = modifier_mask("Shift");
        let alt = modifier_mask("Mod1");
        let control = modifier_mask("Control");
        let super_key = modifier_mask("Mod4");

        let cases = [
            (shift, b"\x1b[1;2A".as_slice()),
            (alt, b"\x1b[1;3A".as_slice()),
            (control, b"\x1b[1;5A".as_slice()),
            (super_key, b"\x1b[1;9A".as_slice()),
            (shift | alt | control, b"\x1b[1;8A".as_slice()),
        ];

        for (mask, expected) in cases {
            keyboard.update_modifiers(mask, 0, 0, 0);
            assert_eq!(keyboard.translate(103, false).as_deref(), Some(expected));
        }

        keyboard.update_modifiers(shift | control, 0, 0, 0);
        assert_eq!(
            keyboard.translate(110, false).as_deref(),
            Some(b"\x1b[2;6~".as_slice())
        );
        assert_eq!(
            keyboard.translate(102, false).as_deref(),
            Some(b"\x1b[1;6H".as_slice())
        );
    }

    #[test]
    fn function_keys_encode_shift_ctrl_alt_and_super_modifiers() {
        let mut keyboard = keyboard_with_layouts("us");
        let keymap = keyboard
            .keymap
            .as_ref()
            .expect("keymap should be installed");
        let modifier_mask = |name: &str| {
            let index = keymap.mod_get_index(name);
            assert_ne!(index, xkb::MOD_INVALID, "missing {name} modifier");
            1_u32 << index
        };
        let shift = modifier_mask("Shift");
        let alt = modifier_mask("Mod1");
        let control = modifier_mask("Control");
        let super_key = modifier_mask("Mod4");

        let cases = [
            (shift, b"\x1b[1;2P".as_slice()),
            (alt, b"\x1b[1;3P".as_slice()),
            (control, b"\x1b[1;5P".as_slice()),
            (super_key, b"\x1b[1;9P".as_slice()),
            (shift | alt | control, b"\x1b[1;8P".as_slice()),
        ];

        for (mask, expected) in cases {
            keyboard.update_modifiers(mask, 0, 0, 0);
            assert_eq!(keyboard.translate(59, false).as_deref(), Some(expected));
        }

        keyboard.update_modifiers(shift | control, 0, 0, 0);
        assert_eq!(
            keyboard.translate(63, false).as_deref(),
            Some(b"\x1b[15;6~".as_slice())
        );
    }

    #[test]
    fn translation_rejects_evdev_keycode_overflow() {
        let mut keyboard = keyboard_with_layouts("us");

        assert_eq!(keyboard.translate(u32::MAX, false), None);
        assert!(!keyboard.is_repeatable(u32::MAX));
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
