//! Putting transcribed text into the focused application.
//!
//! There are two ways to do this and the right one depends on the application,
//! which is precisely why this lives in the compositor: only niri knows both
//! whether the focused surface speaks `text-input-v3` and what the active XKB
//! keymap is.
//!
//! - Applications with an active text-input get the text through the protocol.
//!   Provisional text becomes *preedit* — the underlined, not-yet-real text
//!   phones show while you dictate — so when whisper revises a phrase the
//!   preedit is simply replaced. Nothing is ever typed and then taken back.
//!
//! - Everything else (Electron, games, XWayland) gets synthesised key events.
//!   There is no preedit to revise here, so only settled text is sent and the
//!   provisional text stays in the on-screen indicator. Append-only: this path
//!   never retracts what it has already typed, because by then the cursor may
//!   be somewhere else entirely.
//!
//! The keymap is the reason tools outside the compositor struggle here. They
//! have to guess the layout, which is why they break on AltGr and dead keys.
//! We ask xkb what each key actually produces and invert that, so the mapping
//! is right by construction on any layout.

use std::collections::HashMap;

use smithay::backend::input::KeyState;
use smithay::input::keyboard::{xkb, FilterResult, KeyboardHandle, Keycode};
use smithay::utils::SERIAL_COUNTER;
use smithay::wayland::text_input::TextInputSeat;

use crate::niri::State;
use crate::utils::get_monotonic_time;

/// A piece of transcribed text on its way to the focused application.
#[derive(Debug, Clone)]
pub enum Injection {
    /// Text whisper may still revise. Shown as preedit where that is possible
    /// and otherwise withheld until it settles.
    Preedit(String),
    /// Text the transcriber has settled on.
    Commit(String),
}

/// Which route a piece of text actually took, for logging and so the caller
/// knows whether provisional text made it to the application or needs to be
/// shown in the indicator instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Delivered through text-input-v3.
    TextInput,
    /// Typed as synthesised key events.
    Keyboard,
    /// Nothing was sent: provisional text with no text-input to show it in.
    Withheld,
}

/// How to produce one character on the current keymap.
#[derive(Debug, Clone, Copy)]
struct KeyPlan {
    keycode: Keycode,
    shift: bool,
    level3: bool,
}

/// The modifier keys we know how to press, located in the current keymap.
#[derive(Debug, Clone, Copy, Default)]
struct ModKeys {
    shift: Option<Keycode>,
    level3: Option<Keycode>,
}

impl State {
    /// Send text to the focused application, choosing the route it can accept.
    pub fn inject_text(&mut self, injection: Injection) -> Delivery {
        // Cloning the seat keeps the text-input borrow off `self`, which the
        // keyboard fallback needs mutably.
        let seat = self.niri.seat.clone();
        let text_input = seat.text_input();

        let mut delivered = false;
        text_input.with_active_text_input(|ti, _surface| {
            match &injection {
                Injection::Preedit(text) => {
                    // Cursor at the end, which is where a speaker is.
                    let end = text.len() as i32;
                    ti.preedit_string(Some(text.clone()), end, end);
                }
                Injection::Commit(text) => {
                    // Clear the preedit in the same batch as the commit, or the
                    // provisional text is left behind alongside the real thing.
                    ti.preedit_string(None, 0, 0);
                    ti.commit_string(Some(text.clone()));
                }
            }
            delivered = true;
        });

        if delivered {
            text_input.done(false);
            return Delivery::TextInput;
        }

        match injection {
            // Nothing to revise into, so hold provisional text back rather than
            // type words that are about to change.
            Injection::Preedit(_) => Delivery::Withheld,
            Injection::Commit(text) => {
                self.type_text(&text);
                Delivery::Keyboard
            }
        }
    }

    /// Type a string as synthesised key events on the current keymap.
    pub fn type_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }

        let Some(keyboard) = self.niri.seat.get_keyboard() else {
            warn!("no keyboard on seat, cannot type text");
            return;
        };

        // Build the character map once for the whole string. It is a few
        // hundred xkb lookups, far cheaper than getting cache invalidation
        // wrong when the user switches layout mid-sentence.
        let (map, mods) = keyboard.with_xkb_state(self, |ctx| {
            let xkb_guard = ctx.xkb().lock().unwrap();
            // SAFETY: the keymap reference is confined to this scope, so no
            // ref-count outlives the guard.
            let keymap = unsafe { xkb_guard.keymap() };
            let layout = xkb_guard.active_layout().0;
            (
                build_char_map(keymap, layout),
                find_mod_keys(keymap, layout),
            )
        });

        let mut missing = String::new();

        for ch in text.chars() {
            let Some(plan) = map.get(&ch) else {
                missing.push(ch);
                continue;
            };

            let shift = plan.shift.then_some(mods.shift).flatten();
            let level3 = plan.level3.then_some(mods.level3).flatten();

            // A character needing a modifier we cannot find would come out as
            // the wrong one, which is worse than coming out as nothing.
            if (plan.shift && shift.is_none()) || (plan.level3 && level3.is_none()) {
                missing.push(ch);
                continue;
            }

            for kc in [shift, level3].into_iter().flatten() {
                self.send_key(&keyboard, kc, KeyState::Pressed);
            }
            self.send_key(&keyboard, plan.keycode, KeyState::Pressed);
            self.send_key(&keyboard, plan.keycode, KeyState::Released);
            for kc in [level3, shift].into_iter().flatten().rev() {
                self.send_key(&keyboard, kc, KeyState::Released);
            }
        }

        if !missing.is_empty() {
            warn!("could not type {missing:?} on the current keymap");
        }
    }

    /// Send one key event straight to the focused client.
    ///
    /// The filter always forwards, so dictated text can never fire a keybind —
    /// saying "super" should not switch workspaces. Going through `input`
    /// rather than `input_forward` keeps the compositor's own XKB state in
    /// step and emits the `modifiers` event clients rely on to see a shifted
    /// character as shifted.
    fn send_key(&mut self, keyboard: &KeyboardHandle<Self>, keycode: Keycode, state: KeyState) {
        let serial = SERIAL_COUNTER.next_serial();
        let time = get_monotonic_time().as_millis() as u32;
        keyboard.input::<(), _>(self, keycode, state, serial, time, |_, _, _| {
            FilterResult::Forward
        });
    }
}

/// Invert the keymap: for every character it can produce, remember which key
/// and modifiers produce it.
///
/// Asking xkb what a key produces and reversing that is what makes this
/// layout-correct without special-casing anything.
///
/// Keys are considered in keycode order, plainest modifier combination first.
/// Order matters more than it looks: `#` is shift+3 on the main block, but it
/// is *also* the unmodified keysym of the `XF86NumericPound` media key. Taking
/// the unmodified spelling first would pick the media key, and pressing that
/// types nothing and may fire a media action instead. Lowest keycode wins, so
/// the main typing block is always preferred.
fn build_char_map(keymap: &xkb::Keymap, layout: xkb::LayoutIndex) -> HashMap<char, KeyPlan> {
    let mut map = HashMap::new();

    let shift_mask: xkb::ModMask = mod_mask(keymap, xkb::MOD_NAME_SHIFT);
    let level3_mask: xkb::ModMask = mod_mask(keymap, "Mod5");

    let combos: [(xkb::ModMask, bool, bool); 4] = [
        (0, false, false),
        (shift_mask, true, false),
        (level3_mask, false, true),
        (shift_mask | level3_mask, true, true),
    ];

    // One xkb state per modifier combination, reused across every key.
    let states: Vec<(xkb::State, bool, bool)> = combos
        .into_iter()
        .filter(|&(mask, shift, level3)| {
            // A modifier this keymap does not have makes its combinations
            // unreachable, so drop them rather than mis-map a character onto
            // an unmodified key.
            !((shift && shift_mask == 0) || (level3 && level3_mask == 0)) && (mask != 0 || !shift)
        })
        .map(|(mask, shift, level3)| {
            let mut state = xkb::State::new(keymap);
            state.update_mask(mask, 0, 0, 0, 0, layout);
            (state, shift, level3)
        })
        .collect();

    for raw in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
        let keycode = Keycode::new(raw);

        if is_vendor_key(keymap, keycode, layout) {
            continue;
        }

        for (state, shift, level3) in &states {
            let sym = state.key_get_one_sym(keycode);
            let Some(ch) = char::from_u32(xkb::keysym_to_utf32(sym)) else {
                continue;
            };
            // Control characters are not text. Return and Tab arrive as their
            // own keysyms and stay, because dictation does emit newlines.
            if ch.is_control() && ch != '\n' && ch != '\t' {
                continue;
            }
            map.entry(ch).or_insert(KeyPlan {
                keycode,
                shift: *shift,
                level3: *level3,
            });
        }
    }

    map
}

/// Whether a key is a vendor special key (`XF86*`) rather than a text key.
///
/// These carry unicode mappings that make them look typeable, but pressing one
/// runs whatever the key means — adjusting volume, opening a browser — instead
/// of producing a character.
fn is_vendor_key(keymap: &xkb::Keymap, keycode: Keycode, layout: xkb::LayoutIndex) -> bool {
    keymap
        .key_get_syms_by_level(keycode, layout, 0)
        .iter()
        .any(|sym| matches!(sym.raw(), 0x1008_0000..=0x1008_FFFF))
}

/// Find the keys that press the modifiers we need.
fn find_mod_keys(keymap: &xkb::Keymap, layout: xkb::LayoutIndex) -> ModKeys {
    let mut keys = ModKeys::default();

    for raw in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
        let keycode = Keycode::new(raw);
        for sym in keymap.key_get_syms_by_level(keycode, layout, 0) {
            match sym.raw() {
                xkb::keysyms::KEY_Shift_L | xkb::keysyms::KEY_Shift_R if keys.shift.is_none() => {
                    keys.shift = Some(keycode);
                }
                xkb::keysyms::KEY_ISO_Level3_Shift | xkb::keysyms::KEY_Mode_switch
                    if keys.level3.is_none() =>
                {
                    keys.level3 = Some(keycode);
                }
                _ => {}
            }
        }
    }

    keys
}

/// The bit mask for a named modifier, or 0 if this keymap has no such thing.
fn mod_mask(keymap: &xkb::Keymap, name: &str) -> xkb::ModMask {
    let idx = keymap.mod_get_index(name);
    if idx == xkb::MOD_INVALID {
        0
    } else {
        1 << idx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keymap_for(layout: &str) -> xkb::Keymap {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            layout,
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("xkeyboard-config should provide this layout")
    }

    #[test]
    fn inverts_a_us_keymap() {
        let keymap = keymap_for("us");
        let map = build_char_map(&keymap, 0);
        let mods = find_mod_keys(&keymap, 0);

        assert!(mods.shift.is_some(), "no shift key found");

        for ch in "abxyz0189".chars() {
            let plan = map.get(&ch).unwrap_or_else(|| panic!("no plan for {ch:?}"));
            assert!(!plan.shift, "{ch:?} should not need shift");
        }
        for ch in "ABXYZ!@#?".chars() {
            let plan = map.get(&ch).unwrap_or_else(|| panic!("no plan for {ch:?}"));
            assert!(plan.shift, "{ch:?} should need shift");
        }
        // The characters dictation actually produces all the time.
        for ch in " .,'-".chars() {
            assert!(map.contains_key(&ch), "no plan for {ch:?}");
        }
    }

    #[test]
    fn a_case_pair_shares_one_key() {
        let keymap = keymap_for("us");
        let map = build_char_map(&keymap, 0);

        assert_eq!(map[&'a'].keycode, map[&'A'].keycode);
        assert_eq!(map[&'1'].keycode, map[&'!'].keycode);
    }

    /// The point of inverting the live keymap rather than assuming US: a
    /// character sits on a different key depending on the layout.
    #[test]
    fn follows_the_layout() {
        let us = build_char_map(&keymap_for("us"), 0);
        let de = build_char_map(&keymap_for("de"), 0);

        // y and z swap between these two layouts.
        assert_ne!(us[&'y'].keycode, de[&'y'].keycode);
        // And German reaches characters US simply does not have.
        assert!(de.contains_key(&'ü'), "de keymap should produce u-umlaut");
    }

    /// Regression: `#` is shift+3 on the main block, but also the unmodified
    /// keysym of the XF86NumericPound media key. Picking the media key would
    /// type nothing and might fire a media action.
    #[test]
    fn prefers_the_main_block_over_media_keys() {
        let keymap = keymap_for("us");
        let map = build_char_map(&keymap, 0);

        assert_eq!(map[&'#'].keycode, map[&'3'].keycode);
        assert!(map[&'#'].shift);
    }
}
