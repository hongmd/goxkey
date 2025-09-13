mod config;
mod hotkey;
mod input;
mod platform;
mod scripting;
mod ui;

use std::thread;

use druid::{AppLauncher, ExtEventSink, Target, WindowDesc};
use input::{rebuild_keyboard_layout_map, HOTKEY_MATCHING_CIRCUIT_BREAK, INPUT_STATE};
use log::debug;
use once_cell::sync::OnceCell;
use platform::{
    add_app_change_callback, ensure_accessibility_permission, run_event_listener, send_backspace,
    send_string, EventTapType, Handle, KeyModifier, PressedKey, KEY_DELETE, KEY_ENTER, KEY_ESCAPE,
    KEY_SPACE, KEY_TAB, RAW_KEY_GLOBE,
};

use crate::{
    input::{HOTKEY_MATCHING, HOTKEY_MODIFIERS},
    platform::{RAW_ARROW_DOWN, RAW_ARROW_LEFT, RAW_ARROW_RIGHT, RAW_ARROW_UP},
};
use ui::{UIDataAdapter, UPDATE_UI};

static UI_EVENT_SINK: OnceCell<ExtEventSink> = OnceCell::new();
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

fn do_transform_keys(handle: Handle, is_delete: bool) -> bool {
    let mut input_state = INPUT_STATE.lock().unwrap();
    if let Ok((output, transform_result)) = input_state.transform_keys() {
        debug!("Transformed: {:?}", output);
        if input_state.should_send_keyboard_event(&output) || is_delete {
            // This is a workaround for Firefox, where macOS's Accessibility API cannot work.
            // We cannot get the selected text in the address bar, so we will go with another
            // hacky way: Always send a space and delete it immediately. This will dismiss the
            // current pre-selected URL and fix the double character issue.
            if input_state.should_dismiss_selection_if_needed() {
                _ = send_string(handle, " ");
                _ = send_backspace(handle, 1);
            }

            let backspace_count = input_state.get_backspace_count(is_delete);
            debug!("Backspace count: {}", backspace_count);
            _ = send_backspace(handle, backspace_count);
            _ = send_string(handle, &output);
            debug!("Sent: {:?}", output);
            input_state.replace(output);
            if transform_result.letter_modification_removed
                || transform_result.tone_mark_removed
            {
                input_state.stop_tracking();
            }
            return true;
        }
    }
    false
}

fn do_restore_word(handle: Handle) {
    let mut input_state = INPUT_STATE.lock().unwrap();
    let backspace_count = input_state.get_backspace_count(true);
    debug!("Backspace count: {}", backspace_count);
    _ = send_backspace(handle, backspace_count);
    let typing_buffer = input_state.get_typing_buffer().to_string();
    _ = send_string(handle, &typing_buffer);
    debug!("Sent: {:?}", typing_buffer);
    input_state.replace(typing_buffer);
}

fn do_macro_replace(handle: Handle, target: &String) {
    let mut input_state = INPUT_STATE.lock().unwrap();
    let backspace_count = input_state.get_backspace_count(true);
    debug!("Backspace count: {}", backspace_count);
    _ = send_backspace(handle, backspace_count);
    _ = send_string(handle, target);
    debug!("Sent: {:?}", target);
    input_state.replace(target.to_owned());
}

fn toggle_vietnamese() {
    INPUT_STATE.lock().unwrap().toggle_vietnamese();
    if let Some(event_sink) = UI_EVENT_SINK.get() {
        _ = event_sink.submit_command(UPDATE_UI, (), Target::Auto);
    }
}

fn auto_toggle_vietnamese() {
    let mut input_state = INPUT_STATE.lock().unwrap();
    if !input_state.is_auto_toggle_enabled() {
        return;
    }
    let has_change = input_state.update_active_app().is_some();
    if !has_change {
        return;
    }
    if let Some(event_sink) = UI_EVENT_SINK.get() {
        _ = event_sink.submit_command(UPDATE_UI, (), Target::Auto);
    }
}

fn event_handler(
    handle: Handle,
    event_type: EventTapType,
    pressed_key: Option<PressedKey>,
    modifiers: KeyModifier,
) -> bool {
    let mut input_state = INPUT_STATE.lock().unwrap();
    let mut hotkey_modifiers = HOTKEY_MODIFIERS.lock().unwrap();
    let mut hotkey_matching = HOTKEY_MATCHING.lock().unwrap();
    let mut hotkey_matching_circuit_break = HOTKEY_MATCHING_CIRCUIT_BREAK.lock().unwrap();
    let pressed_key_code = pressed_key.and_then(|p| match p {
        PressedKey::Char(c) => Some(c),
        _ => None,
    });

    if event_type == EventTapType::FlagsChanged {
        if modifiers.is_empty() {
            // Modifier keys are released
            if *hotkey_matching && !*hotkey_matching_circuit_break {
                drop(input_state); // release lock before calling toggle_vietnamese
                toggle_vietnamese();
                input_state = INPUT_STATE.lock().unwrap(); // re-acquire
                hotkey_modifiers = HOTKEY_MODIFIERS.lock().unwrap();
                hotkey_matching = HOTKEY_MATCHING.lock().unwrap();
                hotkey_matching_circuit_break = HOTKEY_MATCHING_CIRCUIT_BREAK.lock().unwrap();
            }
            *hotkey_modifiers = KeyModifier::MODIFIER_NONE;
            *hotkey_matching = false;
            *hotkey_matching_circuit_break = false;
        } else {
            hotkey_modifiers.set(modifiers, true);
        }
    }

    let is_hotkey_matched = input_state
        .get_hotkey()
        .is_match(*hotkey_modifiers, pressed_key_code);
    if *hotkey_matching && !is_hotkey_matched {
        *hotkey_matching_circuit_break = true;
    }
    *hotkey_matching = is_hotkey_matched;

    match pressed_key {
        Some(pressed_key) => {
            match pressed_key {
                PressedKey::Raw(raw_keycode) => {
                    if raw_keycode == RAW_KEY_GLOBE {
                        drop(input_state);
                        toggle_vietnamese();
                        return true;
                    }
                    if raw_keycode == RAW_ARROW_UP || raw_keycode == RAW_ARROW_DOWN {
                        input_state.new_word();
                    }
                    if raw_keycode == RAW_ARROW_LEFT || raw_keycode == RAW_ARROW_RIGHT {
                        // TODO: Implement a better cursor tracking on each word here
                        input_state.new_word();
                    }
                }
                PressedKey::Char(keycode) => {
                    if input_state.is_enabled() {
                        match keycode {
                            KEY_ENTER | KEY_TAB | KEY_SPACE | KEY_ESCAPE => {
                                let is_valid_word = vi::validation::is_valid_word(
                                    input_state.get_displaying_word(),
                                );
                                let is_allowed_word = input_state
                                    .is_allowed_word(input_state.get_displaying_word());
                                let is_transformed_word = !input_state
                                    .get_typing_buffer()
                                    .eq(input_state.get_displaying_word());
                                if is_transformed_word && !is_valid_word && !is_allowed_word {
                                    drop(input_state);
                                    do_restore_word(handle);
                                    input_state = INPUT_STATE.lock().unwrap();
                                }

                                if input_state.previous_word_is_stop_tracking_words() {
                                    input_state.clear_previous_word();
                                }

                                if keycode == KEY_TAB || keycode == KEY_SPACE {
                                    if let Some(macro_target) = input_state.get_macro_target().cloned() {
                                        debug!("Macro: {}", macro_target);
                                        drop(input_state);
                                        do_macro_replace(handle, &macro_target);
                                        input_state = INPUT_STATE.lock().unwrap();
                                    }
                                }

                                input_state.new_word();
                            }
                            KEY_DELETE => {
                                if !modifiers.is_empty() && !modifiers.is_shift() {
                                    input_state.new_word();
                                } else {
                                    input_state.pop();
                                }
                            }
                            c => {
                                if "()[]{}<>/\\!@#$%^&*-_=+|~`,.;'\"/".contains(c)
                                    || (c.is_numeric() && modifiers.is_shift())
                                {
                                    // If special characters detected, dismiss the current tracking word
                                    if c.is_numeric() {
                                        input_state.push(c);
                                    }
                                    input_state.new_word();
                                } else {
                                    // Otherwise, process the character
                                    if modifiers.is_super() || modifiers.is_alt() {
                                        input_state.new_word();
                                    } else if input_state.is_tracking() {
                                        input_state.push(
                                            if modifiers.is_shift() || modifiers.is_capslock() {
                                                c.to_ascii_uppercase()
                                            } else {
                                                c
                                            },
                                        );
                                        drop(input_state);
                                        let ret = do_transform_keys(handle, false);
                                        return ret;
                                    }
                                }
                            }
                        }
                    } else {
                        match keycode {
                            KEY_ENTER | KEY_TAB | KEY_SPACE | KEY_ESCAPE => {
                                input_state.new_word();
                            }
                            _ => {
                                if !modifiers.is_empty() {
                                    input_state.new_word();
                                }
                            }
                        }
                    }
                }
            }
        }
        None => {
            let previous_modifiers = input_state.get_previous_modifiers();
            if previous_modifiers.is_empty() {
                if modifiers.is_control() {
                    if !input_state.get_typing_buffer().is_empty() {
                        drop(input_state);
                        do_restore_word(handle);
                        input_state = INPUT_STATE.lock().unwrap();
                    }
                    input_state.set_temporary_disabled();
                }
                if modifiers.is_super() || event_type == EventTapType::Other {
                    input_state.new_word();
                }
            }
        }
    }
    false
}

fn main() {
    let app_title = format!("gõkey v{APP_VERSION}");
    env_logger::init();
    if !ensure_accessibility_permission() {
        // Show the Accessibility Permission Request screen
        let win = WindowDesc::new(ui::permission_request_ui_builder())
            .title(app_title)
            .window_size((500.0, 360.0))
            .resizable(false);
        let app = AppLauncher::with_window(win);
        _ = app.launch(());
    } else {
        // Start the GõKey application
        rebuild_keyboard_layout_map();
        let win = WindowDesc::new(ui::main_ui_builder())
            .title(app_title)
            .window_size((ui::WINDOW_WIDTH, ui::WINDOW_HEIGHT))
            .set_position(ui::center_window_position())
            .set_always_on_top(true)
            .resizable(false);
        let app = AppLauncher::with_window(win);
        let event_sink = app.get_external_handle();
        _ = UI_EVENT_SINK.set(event_sink);
        thread::spawn(|| {
            run_event_listener(&event_handler);
        });
        add_app_change_callback(|| {
            auto_toggle_vietnamese();
        });
        _ = app.launch(UIDataAdapter::new());
    }
}
