use std::sync::mpsc::{Sender, channel};
use std::sync::{LazyLock, Mutex};
use std::thread;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, PM_NOREMOVE, PeekMessageW, PostThreadMessageW,
    TranslateMessage, WM_USER,
};

use crate::APP_STATE;
use crate::config::WindowRule;
use crate::utils::WM_APP_CREATE_BORDER;
use crate::window_border::WindowBorder;

pub enum BorderCommand {
    Create {
        tracking_window: isize,
        window_rule: WindowRule,
    },
}

pub struct BorderManager {
    thread_id: u32,
    sender: Mutex<Sender<BorderCommand>>,
}

pub static BORDER_MANAGER: LazyLock<BorderManager> = LazyLock::new(BorderManager::new);

impl Default for BorderManager {
    fn default() -> Self {
        Self::new()
    }
}

impl BorderManager {
    pub fn new() -> Self {
        let (sender, receiver) = channel();
        let (ready_tx, ready_rx) = channel();

        thread::spawn(move || {
            let thread_id = unsafe { GetCurrentThreadId() };
            ready_tx.send(thread_id).unwrap();

            // Force message queue creation for this thread
            unsafe {
                let mut msg = MSG::default();
                let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
            }

            unsafe {
                let mut message = MSG::default();
                while GetMessageW(&mut message, None, 0, 0).as_bool() {
                    if message.hwnd.is_invalid() && message.message == WM_APP_CREATE_BORDER {
                        while let Ok(cmd) = receiver.try_recv() {
                            match cmd {
                                BorderCommand::Create {
                                    tracking_window,
                                    window_rule,
                                } => {
                                    create_border_internal(HWND(tracking_window as _), window_rule);
                                }
                            }
                        }
                    } else {
                        let _ = TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
            }
        });

        let thread_id = ready_rx.recv().unwrap();
        Self {
            thread_id,
            sender: Mutex::new(sender),
        }
    }

    pub fn create_border(&self, tracking_window: HWND, window_rule: WindowRule) {
        if let Ok(sender) = self.sender.lock() {
            let _ = sender.send(BorderCommand::Create {
                tracking_window: tracking_window.0 as isize,
                window_rule,
            });
            unsafe {
                let _ =
                    PostThreadMessageW(self.thread_id, WM_APP_CREATE_BORDER, WPARAM(0), LPARAM(0));
            }
        }
    }
}

fn create_border_internal(tracking_window: HWND, window_rule: WindowRule) {
    let tracking_window_isize = tracking_window.0 as isize;

    let mut borders_hashmap = APP_STATE.borders.lock().unwrap();
    if borders_hashmap.contains_key(&tracking_window_isize) {
        return;
    }

    debug!("creating border for: {tracking_window:?}");

    let mut border_box = match WindowBorder::new(tracking_window) {
        Ok(border) => border,
        Err(err) => {
            error!("could not create window border for {tracking_window:?}: {err:#}");
            return;
        }
    };

    let border_hwnd = border_box.border_window.0;
    borders_hashmap.insert(tracking_window_isize, border_hwnd.0 as isize);
    drop(borders_hashmap);

    if let Err(err) = border_box.init(window_rule) {
        error!("could not initialize border: {err:#}");
        APP_STATE
            .borders
            .lock()
            .unwrap()
            .remove(&tracking_window_isize);
    } else {
        border_box.is_raw_transferred = true;
        // Transfer ownership of border_box to GWLP_USERDATA (cleaned up on WM_NCDESTROY)
        let _ = Box::into_raw(border_box);
    }
}
