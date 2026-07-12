//! Injecting synthetic keyboard input into specific windows.
//!
//! Wayland only delivers keyboard input to the surface that currently holds a
//! seat's keyboard focus, and focus is tracked *per seat*. To type into a
//! window that is not the one the user is actively working in, we use a second,
//! keyboard-only "injector" seat (created in [`crate::niri::Niri::new`]): we
//! point that seat's keyboard focus at the target surface, forward the keys,
//! then drop its focus again. The real seat is never touched, so the window the
//! user is actually typing in keeps its focus and selection.
//!
//! For robustness across keyboard layouts, each injection uploads a freshly
//! generated xkb keymap to the injector seat in which every keysym we need lives
//! on its own keycode. This is the same trick `wtype` uses: it makes the emitted
//! characters independent of whatever layout (US, German, ...) is currently
//! active on the real seat.

use std::fmt::Write as _;

use smithay::backend::input::KeyState;
use smithay::input::keyboard::{xkb, FilterResult, Keysym};
use smithay::utils::SERIAL_COUNTER;

use crate::niri::State;
use crate::utils::get_monotonic_time;

/// xkb reserves keycodes 0-7; usable keycodes start at 8. We map our synthetic
/// keysyms onto keycodes starting at 9 (leaving 8 unused), up to the xkb maximum
/// of 255.
const FIRST_KEYCODE: u32 = 9;
const MAX_KEYCODE: u32 = 255;

impl State {
    /// Send an ordered mix of typed text and key combinations to the window with
    /// the given id without focusing it.
    ///
    /// Each segment is `text:<literal>` or `key:<combo>`. Text characters are
    /// each sent as a single keypress (newlines as `Return`, tabs as `Tab`;
    /// characters with no keysym are skipped); key segments are pressed as a
    /// chord. Everything is applied in order within a single focus cycle. If any
    /// segment is malformed, nothing is sent.
    pub fn send_into_window(&mut self, id: u64, segments: &[String]) {
        let mut chords: Vec<Vec<Keysym>> = Vec::new();

        for segment in segments {
            let Some((kind, value)) = segment.split_once(':') else {
                warn!("invalid input segment, expected `text:…` or `key:…`: {segment:?}");
                return;
            };

            match kind {
                "text" => {
                    for ch in value.chars() {
                        let keysym = match ch {
                            '\n' | '\r' => Keysym::Return,
                            '\t' => Keysym::Tab,
                            _ => xkb::utf32_to_keysym(ch as u32),
                        };
                        if keysym.raw() == 0 {
                            warn!("skipping character with no keysym: {ch:?}");
                            continue;
                        }
                        chords.push(vec![keysym]);
                    }
                }
                "key" | "keys" => {
                    let Some(chord) = parse_combo(value) else {
                        warn!("could not parse key combination: {value:?}");
                        return;
                    };
                    chords.push(chord);
                }
                other => {
                    warn!("unknown input segment kind {other:?}, expected `text` or `key`");
                    return;
                }
            }
        }

        if chords.is_empty() {
            return;
        }

        self.inject_into_window(id, &chords);
    }

    /// Deliver a sequence of key chords to a window via the injector seat.
    ///
    /// Each chord is a list of keysyms to press together; within a chord the
    /// keys are pressed in order and released in reverse (so `[Control_L, r]`
    /// presses Control before `r` and releases `r` before Control).
    fn inject_into_window(&mut self, id: u64, chords: &[Vec<Keysym>]) {
        let surface = self
            .niri
            .layout
            .windows()
            .find(|(_, m)| m.id().get() == id)
            .map(|(_, m)| m.toplevel().wl_surface().clone());
        let Some(surface) = surface else {
            warn!("cannot inject keys: no window with id {id}");
            return;
        };

        // Assign a keycode to each distinct keysym across all chords.
        let mut order: Vec<Keysym> = Vec::new();
        for chord in chords {
            for keysym in chord {
                if !order.iter().any(|k| k.raw() == keysym.raw()) {
                    order.push(*keysym);
                }
            }
        }

        let capacity = (MAX_KEYCODE - FIRST_KEYCODE + 1) as usize;
        if order.len() > capacity {
            warn!(
                "too many distinct keys to inject ({}), truncating to {capacity}",
                order.len()
            );
            order.truncate(capacity);
        }

        let keycode_of = |keysym: &Keysym| -> Option<u32> {
            order
                .iter()
                .position(|k| k.raw() == keysym.raw())
                .map(|i| FIRST_KEYCODE + i as u32)
        };

        let keymap = build_keymap(&order);

        let keyboard = self
            .niri
            .injector_seat
            .get_keyboard()
            .expect("injector seat always has a keyboard");

        if let Err(err) = keyboard.set_keymap_from_string(self, keymap) {
            warn!("failed to set injector keymap: {err:?}");
            return;
        }

        // Focus the target on the injector seat only; this sends it wl_keyboard
        // enter (with the generated keymap) without touching the real seat.
        keyboard.set_focus(self, Some(surface), SERIAL_COUNTER.next_serial());

        for chord in chords {
            let codes: Vec<u32> = chord.iter().filter_map(&keycode_of).collect();

            for code in codes.iter() {
                self.injector_key(*code, KeyState::Pressed);
            }
            for code in codes.iter().rev() {
                self.injector_key(*code, KeyState::Released);
            }
        }

        // Drop the injector focus so the target goes back to being unfocused
        // (it receives wl_keyboard leave). `keyboard` is an owned handle that
        // does not borrow `self`, so it is still valid here.
        keyboard.set_focus(self, None, SERIAL_COUNTER.next_serial());
    }

    /// Forward a single key press/release on the injector seat to its focused
    /// surface, bypassing niri's keybind handling.
    fn injector_key(&mut self, keycode: u32, state: KeyState) {
        let keyboard = self.niri.injector_seat.get_keyboard().unwrap();
        let time = get_monotonic_time().as_millis() as u32;
        let _ = keyboard.input::<(), _>(
            self,
            xkb::Keycode::new(keycode),
            state,
            SERIAL_COUNTER.next_serial(),
            time,
            // Always forward: this seat exists only for injection, so keys must
            // never be interpreted as niri binds.
            |_, _, _| FilterResult::Forward,
        );
    }
}

/// Build an xkb keymap string that maps each keysym in `keysyms` to its own
/// keycode (starting at [`FIRST_KEYCODE`]), with no modifiers required to
/// produce it. Modifier keysyms (Control_L, Shift_L, ...) still act as modifiers
/// because the included `complete` compat rules interpret them.
fn build_keymap(keysyms: &[Keysym]) -> String {
    let mut keycodes = String::new();
    let mut symbols = String::new();

    for (i, keysym) in keysyms.iter().enumerate() {
        let code = FIRST_KEYCODE + i as u32;
        // keysym_get_name returns a token the xkb parser accepts as input (e.g.
        // "a", "Return", "at", "U00E4"), i.e. it round-trips.
        let name = xkb::keysym_get_name(*keysym);
        let _ = writeln!(keycodes, "        <I{i}> = {code};");
        let _ = writeln!(symbols, "        key <I{i}> {{ [ {name} ] }};");
    }

    format!(
        "xkb_keymap {{\n\
         xkb_keycodes \"(injected)\" {{\n\
         \x20   minimum = 8;\n\
         \x20   maximum = {MAX_KEYCODE};\n\
         {keycodes}\
         }};\n\
         xkb_types \"(injected)\" {{ include \"complete\" }};\n\
         xkb_compat \"(injected)\" {{ include \"complete\" }};\n\
         xkb_symbols \"(injected)\" {{\n\
         {symbols}\
         }};\n\
         }};\n"
    )
}

/// Parse an xkb-style key combination such as `ctrl+shift+r` or `Return` into
/// the list of keysyms to press together (modifiers first, main key last).
fn parse_combo(combo: &str) -> Option<Vec<Keysym>> {
    let parts: Vec<&str> = combo
        .split('+')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let (key, modifiers) = parts.split_last()?;
    let key: &str = key;

    let mut chord = Vec::new();
    for modifier in modifiers {
        let name = match modifier.to_ascii_lowercase().as_str() {
            "ctrl" | "control" | "ctl" => "Control_L",
            "shift" => "Shift_L",
            "alt" | "mod1" | "meta" => "Alt_L",
            "super" | "logo" | "win" | "mod4" => "Super_L",
            "altgr" | "iso_level3_shift" | "mod5" => "ISO_Level3_Shift",
            other => {
                warn!("unknown modifier in key combination: {other:?}");
                return None;
            }
        };
        chord.push(xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS));
    }

    // Main key: try the exact xkb name, then a case-insensitive match, then a
    // single Unicode character.
    let mut keysym = xkb::keysym_from_name(key, xkb::KEYSYM_NO_FLAGS);
    if keysym.raw() == 0 {
        keysym = xkb::keysym_from_name(key, xkb::KEYSYM_CASE_INSENSITIVE);
    }
    if keysym.raw() == 0 {
        let mut chars = key.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            keysym = xkb::utf32_to_keysym(c as u32);
        }
    }
    if keysym.raw() == 0 {
        warn!("unknown key in key combination: {key:?}");
        return None;
    }

    chord.push(keysym);
    Some(chord)
}
