use anyhow::Context;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Accessibility::HWINEVENTHOOK;
use windows::Win32::UI::WindowsAndMessaging::{
    CHILDID_SELF, EVENT_OBJECT_CLOAKED, EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE,
    EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_REORDER, EVENT_OBJECT_SHOW, EVENT_OBJECT_UNCLOAKED,
    EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART, OBJID_CLIENT,
    OBJID_CURSOR, OBJID_WINDOW,
};

use crate::APP_STATE;
use crate::utils::{
    LogIfErr, WM_APP_FOREGROUND, WM_APP_LOCATIONCHANGE, WM_APP_MINIMIZEEND, WM_APP_MINIMIZESTART,
    WM_APP_REORDER, destroy_border_for_window, get_border_for_window, get_foreground_window,
    hide_border_for_window, is_window_visible, post_message_w, send_notify_message_w,
    show_border_for_window,
};

pub extern "system" fn process_win_event(
    _h_win_event_hook: HWINEVENTHOOK,
    _event: u32,
    _hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _dw_event_thread: u32,
    _dwms_event_time: u32,
) {
    // Ignore cursor events
    if _id_object == OBJID_CURSOR.0 {
        return;
    }

    match _event {
        EVENT_OBJECT_LOCATIONCHANGE => {
            if _id_child != CHILDID_SELF as i32 {
                return;
            }

            if let Some(border) = get_border_for_window(_hwnd) {
                send_notify_message_w(border, WM_APP_LOCATIONCHANGE, WPARAM(0), LPARAM(0))
                    .context("EVENT_OBJECT_LOCATIONCHANGE")
                    .log_if_err();
            }
        }
        EVENT_OBJECT_REORDER => {
            // Ignore OBJIDs not needed for window Z-order handling.
            if _id_object != OBJID_CLIENT.0 {
                return;
            }

            // Send reorder messages to all the border windows
            for value in APP_STATE.borders.lock().unwrap().values() {
                let border_window = HWND(*value as _);
                if is_window_visible(border_window) {
                    post_message_w(Some(border_window), WM_APP_REORDER, WPARAM(0), LPARAM(0))
                        .context("EVENT_OBJECT_REORDER")
                        .log_if_err();
                }
            }
        }
        // Neither the HWND passed by this event nor the one returned by GetForegroundWindow() are
        // accurate 100% of the time. I tried finding workarounds without polling, but gave up.
        EVENT_SYSTEM_FOREGROUND => {
            let potential_active_hwnd = get_foreground_window();
            let new_active_hwnd = match !potential_active_hwnd.is_invalid() {
                true => potential_active_hwnd,
                false => _hwnd,
            };
            let old_active_hwnd = HWND(*APP_STATE.active_window.lock().unwrap() as _);

            handle_foreground_event(new_active_hwnd, old_active_hwnd);
        }
        EVENT_OBJECT_SHOW | EVENT_OBJECT_UNCLOAKED => {
            if _id_object == OBJID_WINDOW.0 {
                show_border_for_window(_hwnd);
            }
        }
        EVENT_OBJECT_HIDE | EVENT_OBJECT_CLOAKED => {
            if _id_object == OBJID_WINDOW.0 {
                hide_border_for_window(_hwnd);
            }
        }
        EVENT_SYSTEM_MINIMIZESTART => {
            if let Some(border) = get_border_for_window(_hwnd) {
                post_message_w(Some(border), WM_APP_MINIMIZESTART, WPARAM(0), LPARAM(0))
                    .context("EVENT_SYSTEM_MINIMIZESTART")
                    .log_if_err();
            }
        }
        EVENT_SYSTEM_MINIMIZEEND => {
            if let Some(border) = get_border_for_window(_hwnd) {
                post_message_w(Some(border), WM_APP_MINIMIZEEND, WPARAM(0), LPARAM(0))
                    .context("EVENT_SYSTEM_MINIMIZEEND")
                    .log_if_err();
            }
        }
        EVENT_OBJECT_DESTROY
            if _id_object == OBJID_WINDOW.0 && _id_child == CHILDID_SELF as i32 =>
        {
            destroy_border_for_window(_hwnd);
        }
        _ => {}
    }
}

pub fn handle_foreground_event(new_active_hwnd: HWND, old_active_hwnd: HWND) {
    if new_active_hwnd == old_active_hwnd {
        return;
    }
    *APP_STATE.active_window.lock().unwrap() = new_active_hwnd.0 as isize;

    let borders = APP_STATE.borders.lock().unwrap();

    // Only notify previously active window (to switch to Inactive)
    if let Some(&old_border) = borders.get(&(old_active_hwnd.0 as isize)) {
        let border_window = HWND(old_border as _);
        post_message_w(Some(border_window), WM_APP_FOREGROUND, WPARAM(0), LPARAM(0))
            .context("EVENT_OBJECT_FOCUS old active")
            .log_if_err();
    }

    // Only notify newly active window (to switch to Active)
    if let Some(&new_border) = borders.get(&(new_active_hwnd.0 as isize)) {
        let border_window = HWND(new_border as _);
        post_message_w(Some(border_window), WM_APP_FOREGROUND, WPARAM(0), LPARAM(0))
            .context("EVENT_OBJECT_FOCUS new active")
            .log_if_err();
    }
}
