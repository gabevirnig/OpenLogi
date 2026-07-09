//! Binding-map construction: overlay the stored per-device (and per-app)
//! bindings on top of the built-in defaults.
//!
//! Keyed by `config_key` (`Option<&str>`) rather than any UI device record so
//! both the agent and the GUI can build the effective map from a [`Config`].

use std::collections::BTreeMap;

use openlogi_core::binding::{
    Action, Binding, ButtonId, GestureDirection, default_binding, default_gesture_binding,
};
use openlogi_core::config::Config;

/// Effective per-button single-action map for the device `config_key`, with
/// `app_bundle`'s per-app overlay applied. Unset buttons fall back to
/// [`default_binding`].
///
/// This is the map the OS hook and the HID++ button-press path consume, so a
/// `Binding::Gesture` is projected to its `click_action()` — the gesture
/// button's per-direction swipes are dispatched via the separate
/// [`gesture_bindings_for`] map, not here.
#[must_use]
pub fn bindings_for(
    config: &Config,
    config_key: Option<&str>,
    app_bundle: Option<&str>,
) -> BTreeMap<ButtonId, Action> {
    let stored = config_key
        .map(|key| config.effective_bindings(key, app_bundle))
        .unwrap_or_default();
    let mut bindings: BTreeMap<ButtonId, Action> = ButtonId::ALL
        .iter()
        .copied()
        .map(|b| (b, default_binding(b)))
        .collect();
    for (k, binding) in stored {
        // A gesture binding with no explicit `Click` has no opinion on the
        // plain-press action, so leave the button's default seed in place rather
        // than clobbering it with the `Action::None` that `click_action()` would
        // project. (An explicit `Single(Action::None)` — a user-disabled button —
        // still overrides, as it should.)
        if binding.is_gesture() && binding.direction_action(GestureDirection::Click).is_none() {
            continue;
        }
        bindings.insert(k, binding.click_action());
    }
    bindings
}

/// Effective gesture bindings for the device `config_key`'s dedicated HID++
/// gesture button. Unset directions fall back to [`default_gesture_binding`].
#[must_use]
pub fn gesture_bindings_for(
    config: &Config,
    config_key: Option<&str>,
) -> BTreeMap<GestureDirection, Action> {
    // The dedicated HID++ gesture button (CID 0x00c3) only drives capture while
    // it is itself in gesture mode. When the user turns it off (demoting it to a
    // single action), return an empty map so the gesture watcher dispatches
    // nothing and stops diverting it — otherwise the always-seeded defaults would
    // keep it firing. Other buttons being gesture buttons is independent: they go
    // through the OS hook ([`oshook_gestures_for`]), not this map.
    let is_gesture = config_key.is_some_and(|key| config.is_gesture_button(key, ButtonId::GestureButton));
    if !is_gesture {
        return BTreeMap::new();
    }
    let stored = config_key
        .map(|key| config.gesture_bindings_for(key))
        .unwrap_or_default();
    let mut bindings: BTreeMap<GestureDirection, Action> = GestureDirection::ALL
        .iter()
        .copied()
        .map(|d| (d, default_gesture_binding(d)))
        .collect();
    for (k, v) in stored {
        bindings.insert(k, v);
    }
    bindings
}

/// Per-direction maps for *every* OS-hook gesture button (Middle/Back/Forward in
/// gesture mode) on `config_key`, with `app_bundle`'s per-app overlay applied,
/// for the OS hook to resolve a hold+swipe. Any number of these can be gesture
/// buttons at once, and each dispatches independently (the OS-hook callback keys
/// its per-hold accumulator on the pressed [`ButtonId`]).
///
/// Unlike [`gesture_bindings_for`] (the dedicated HID++ gesture button, which
/// seeds every direction from [`default_gesture_binding`] at projection time),
/// this returns each button's raw stored map. In practice that map is already
/// fully populated — [`Config::enable_gesture`] seeds all five directions via
/// [`Binding::fill_gesture_defaults`] when a button is turned on — so only a
/// hand-edited sparse map leaves a direction unbound, in which case that swipe
/// simply does nothing. The dedicated gesture button is intentionally excluded:
/// it never reaches the OS hook (it's captured over HID++), so it has no entry
/// here.
///
/// A per-app override of a gesture button turns it into a [`Binding::Single`]
/// for that app, so it stops being a gesture button there and falls through to
/// the single-action path (which applies the override) — mirroring how a single
/// binding is overridden per app.
#[must_use]
pub fn oshook_gestures_for(
    config: &Config,
    config_key: Option<&str>,
    app_bundle: Option<&str>,
) -> BTreeMap<ButtonId, BTreeMap<GestureDirection, Action>> {
    let Some(key) = config_key else {
        return BTreeMap::new();
    };
    // Collect every OS-hook button (Middle/Back/Forward) whose *effective* binding
    // is a gesture map. The dedicated HID++ gesture button is captured over HID++,
    // not here, so it is never in this set. A per-app override replaces a button
    // with a `Single`, dropping it from the gesture set for that app (it then
    // falls through to the single-action path, which applies the override).
    let effective = config.effective_bindings(key, app_bundle);
    ButtonId::ALL
        .into_iter()
        .filter(|button| button.is_os_hook_button())
        .filter_map(|button| match effective.get(&button) {
            Some(Binding::Gesture(map)) => Some((button, map.clone())),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_less_gesture_keeps_default_click_in_projection() {
        // A gesture binding with no explicit `Click` (a migrated sparse v1 map or
        // a hand-edited config) must not project to `Action::None` and silently
        // disable the button — the button's default click survives.
        let mut cfg = Config::default();
        let mut map = BTreeMap::new();
        map.insert(GestureDirection::Up, Action::Copy);
        cfg.set_binding("2b042", ButtonId::GestureButton, Binding::Gesture(map));

        let projected = bindings_for(&cfg, Some("2b042"), None);
        assert_eq!(
            projected.get(&ButtonId::GestureButton),
            Some(&default_binding(ButtonId::GestureButton)),
            "a Click-less gesture must keep the default click, not None"
        );
    }

    #[test]
    fn explicit_gesture_click_overrides_default_in_projection() {
        // A gesture binding that DOES define `Click` projects that action.
        let mut cfg = Config::default();
        let mut map = BTreeMap::new();
        map.insert(GestureDirection::Click, Action::Paste);
        cfg.set_binding("2b042", ButtonId::GestureButton, Binding::Gesture(map));

        let projected = bindings_for(&cfg, Some("2b042"), None);
        assert_eq!(
            projected.get(&ButtonId::GestureButton),
            Some(&Action::Paste)
        );
    }

    #[test]
    fn oshook_gestures_collects_every_os_hook_gesture_button() {
        let mut cfg = Config::default();
        // Two OS-hook buttons in gesture mode — BOTH included (no single-owner
        // lock), each with its raw map preserved.
        cfg.set_binding(
            "2b042",
            ButtonId::Back,
            Binding::Gesture(BTreeMap::from([(GestureDirection::Up, Action::Copy)])),
        );
        cfg.set_binding(
            "2b042",
            ButtonId::Forward,
            Binding::Gesture(BTreeMap::from([(GestureDirection::Down, Action::Paste)])),
        );
        // A single-mode Middle — excluded (not a gesture button).
        cfg.set_binding("2b042", ButtonId::MiddleClick, Action::MiddleClick.into());
        // The dedicated HID++ gesture button — excluded (it never reaches the
        // OS hook, so it must not appear in the hook's gesture map).
        cfg.set_binding(
            "2b042",
            ButtonId::GestureButton,
            Binding::Gesture(BTreeMap::from([(
                GestureDirection::Up,
                Action::MissionControl,
            )])),
        );

        let oshook = oshook_gestures_for(&cfg, Some("2b042"), None);
        assert_eq!(oshook.len(), 2, "both gesture-mode OS-hook buttons belong here");
        assert_eq!(
            oshook.get(&ButtonId::Back),
            Some(&BTreeMap::from([(GestureDirection::Up, Action::Copy)]))
        );
        assert_eq!(
            oshook.get(&ButtonId::Forward),
            Some(&BTreeMap::from([(GestureDirection::Down, Action::Paste)]))
        );
        assert!(!oshook.contains_key(&ButtonId::MiddleClick));
        assert!(!oshook.contains_key(&ButtonId::GestureButton));
    }

    #[test]
    fn per_app_override_drops_one_button_but_keeps_the_others() {
        // Back and Forward both gesture globally...
        let mut cfg = Config::default();
        cfg.enable_gesture("2b042", ButtonId::Back);
        cfg.enable_gesture("2b042", ButtonId::Forward);
        let global = oshook_gestures_for(&cfg, Some("2b042"), None);
        assert!(global.contains_key(&ButtonId::Back), "Back gestures globally");
        assert!(global.contains_key(&ButtonId::Forward));

        // ...but a per-app override of Back makes it a single action in that app,
        // so it drops out of the gesture set there — while Forward keeps gesturing.
        cfg.set_per_app_binding(
            "2b042",
            "com.apple.Safari",
            ButtonId::Back,
            Some(Action::NextTab),
        );
        let scoped = oshook_gestures_for(&cfg, Some("2b042"), Some("com.apple.Safari"));
        assert!(
            !scoped.contains_key(&ButtonId::Back),
            "a per-app override of a gesture button removes only it"
        );
        assert!(
            scoped.contains_key(&ButtonId::Forward),
            "the other gesture button is unaffected"
        );
        // Other apps are unaffected — Back still gestures.
        assert!(
            oshook_gestures_for(&cfg, Some("2b042"), Some("com.other.App"))
                .contains_key(&ButtonId::Back)
        );
    }

    #[test]
    fn gesture_bindings_silent_when_hidpp_button_is_off() {
        let mut cfg = Config::default();
        // Default device: the dedicated HID++ gesture button is on, so its defaults are seeded.
        let defaults = gesture_bindings_for(&cfg, Some("2b042"));
        assert_eq!(
            defaults.get(&GestureDirection::Up),
            Some(&default_gesture_binding(GestureDirection::Up)),
            "the HID++ gesture button gestures by default"
        );

        // Turning the HID++ gesture button off makes it go silent, so the watcher
        // dispatches nothing for 0x00c3 — independent of other buttons gesturing.
        cfg.enable_gesture("2b042", ButtonId::Back);
        cfg.disable_gesture("2b042", ButtonId::GestureButton);
        assert!(
            gesture_bindings_for(&cfg, Some("2b042")).is_empty(),
            "HID++ gesture button must dispatch nothing once it is turned off"
        );
    }
}
