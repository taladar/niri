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
//! Rather than uploading a synthetic keymap to the injector seat (which smithay
//! would broadcast to *every* client bound to that seat, corrupting the keyboard
//! state of unrelated windows and racing with the user's own typing), we look up
//! each keysym in the injector seat's *current* keymap and send the matching
//! keycodes and modifiers. The keymap is never changed, so no other client is
//! disturbed. The trade-off is that a keysym which the active keymap cannot
//! produce (e.g. a character not present in any of the configured layouts) is
//! skipped rather than typed.

use std::collections::HashMap;

use smithay::backend::input::KeyState;
use smithay::input::keyboard::{xkb, FilterResult, Keysym};
use smithay::utils::SERIAL_COUNTER;

use crate::niri::State;
use crate::utils::get_monotonic_time;

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

        let keyboard = self
            .niri
            .injector_seat
            .get_keyboard()
            .expect("injector seat always has a keyboard");

        // Translate the requested keysyms into (keycode, state) events against the injector's
        // current keymap, without modifying it. `plan` holds one event list per chord.
        let plan: Vec<Vec<(u32, KeyState)>> = keyboard.with_xkb_state(self, |ctx| {
            let guard = ctx.xkb().lock().unwrap();
            // SAFETY: `keymap`/`state` are only used within this closure and are not retained.
            let keymap = unsafe { guard.keymap() };
            let layout = unsafe { guard.state() }.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE);
            build_plan(keymap, layout, chords)
        });

        if plan.is_empty() {
            return;
        }

        // Focus the target on the injector seat only; this sends it wl_keyboard enter without
        // touching the real seat.
        keyboard.set_focus(self, Some(surface), SERIAL_COUNTER.next_serial());

        for chord in &plan {
            for (code, state) in chord {
                self.injector_key(*code, *state);
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

/// Turn each chord of keysyms into an ordered list of (keycode, press/release)
/// events that reproduce those keysyms under `keymap` in the given `layout`.
///
/// A chord whose keysyms cannot all be produced by the current keymap is skipped
/// (with a warning) rather than aborting the whole injection.
fn build_plan(keymap: &xkb::Keymap, layout: u32, chords: &[Vec<Keysym>]) -> Vec<Vec<(u32, KeyState)>> {
    let mod_keycodes = modifier_keycodes(keymap);

    let mut plan = Vec::new();
    'chords: for chord in chords {
        // Resolve every keysym to a base keycode plus the modifiers needed to reach its level.
        let mut resolved: Vec<(u32, xkb::ModMask)> = Vec::new();
        for keysym in chord {
            let Some(entry) = resolve_keysym(keymap, layout, *keysym) else {
                warn!(
                    "skipping chord: keysym {:?} is not typable in the current keymap",
                    xkb::keysym_get_name(*keysym)
                );
                continue 'chords;
            };
            resolved.push(entry);
        }

        // Collect the modifier keycodes required across the whole chord.
        let mut mod_codes: Vec<u32> = Vec::new();
        for (_, mask) in &resolved {
            for bit in 0..u32::BITS {
                if mask & (1 << bit) == 0 {
                    continue;
                }
                let Some(&code) = mod_keycodes.get(&bit) else {
                    warn!("skipping chord: no key produces a required modifier (bit {bit})");
                    continue 'chords;
                };
                if !mod_codes.contains(&code) {
                    mod_codes.push(code);
                }
            }
        }

        // Press modifiers, then keys; release keys, then modifiers (each in reverse).
        let mut events = Vec::new();
        for code in &mod_codes {
            events.push((*code, KeyState::Pressed));
        }
        for (code, _) in &resolved {
            events.push((*code, KeyState::Pressed));
        }
        for (code, _) in resolved.iter().rev() {
            events.push((*code, KeyState::Released));
        }
        for code in mod_codes.iter().rev() {
            events.push((*code, KeyState::Released));
        }
        plan.push(events);
    }

    plan
}

/// Find a keycode in `layout` that produces `keysym`, together with the modifier
/// mask needed to reach the level it sits on (the mask with the fewest modifiers
/// is preferred).
fn resolve_keysym(keymap: &xkb::Keymap, layout: u32, keysym: Keysym) -> Option<(u32, xkb::ModMask)> {
    let min = keymap.min_keycode().raw();
    let max = keymap.max_keycode().raw();

    for raw in min..=max {
        let keycode = xkb::Keycode::new(raw);
        let num_levels = keymap.num_levels_for_key(keycode, layout);
        for level in 0..num_levels {
            let syms = keymap.key_get_syms_by_level(keycode, layout, level);
            if syms.len() != 1 || syms[0].raw() != keysym.raw() {
                continue;
            }

            let mut masks = [xkb::ModMask::default(); 16];
            let num_masks = keymap.key_get_mods_for_level(keycode, layout, level, &mut masks);
            let mask = masks[..num_masks]
                .iter()
                .copied()
                .min_by_key(|m| m.count_ones())
                .unwrap_or(0);
            return Some((raw, mask));
        }
    }

    None
}

/// Build a map from real-modifier bit index to a keycode that activates it, by
/// probing every key against a scratch xkb state. The Lock (Caps Lock) modifier
/// is skipped so injection never toggles it.
fn modifier_keycodes(keymap: &xkb::Keymap) -> HashMap<u32, u32> {
    let min = keymap.min_keycode().raw();
    let max = keymap.max_keycode().raw();

    let lock_bit = (0..keymap.num_mods())
        .find(|&idx| keymap.mod_get_name(idx).eq_ignore_ascii_case("lock"));

    let mut map = HashMap::new();
    let mut state = xkb::State::new(keymap);
    for raw in min..=max {
        let keycode = xkb::Keycode::new(raw);
        state.update_key(keycode, xkb::KeyDirection::Down);
        let mask = state.serialize_mods(xkb::STATE_MODS_DEPRESSED);
        state.update_key(keycode, xkb::KeyDirection::Up);

        // Only take keys that toggle exactly one modifier, so we know what pressing them does.
        if mask.count_ones() != 1 {
            continue;
        }
        let bit = mask.trailing_zeros();
        if Some(bit) == lock_bit {
            continue;
        }
        map.entry(bit).or_insert(raw);
    }

    map
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
