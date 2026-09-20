use std::collections::HashMap;
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::thread;
use std::time::Duration;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};

use crate::post_message_w;
use crate::utils::WM_APP_ANIMATE;

struct TickerState {
    targets: HashMap<isize, u64>,
    stop: bool,
}

struct CentralAnimTicker {
    state: Arc<(Mutex<TickerState>, Condvar)>,
}

impl CentralAnimTicker {
    fn new() -> Self {
        let state = Arc::new((
            Mutex::new(TickerState {
                targets: HashMap::new(),
                stop: false,
            }),
            Condvar::new(),
        ));

        let state_clone = Arc::clone(&state);
        thread::spawn(move || {
            let (lock, cvar) = &*state_clone;
            loop {
                let mut state = lock.lock().unwrap();
                while state.targets.is_empty() && !state.stop {
                    state = cvar.wait(state).unwrap();
                }

                if state.stop {
                    break;
                }

                let min_interval = state.targets.values().copied().min().unwrap_or(16).max(1);
                let hwnds: Vec<isize> = state.targets.keys().copied().collect();
                drop(state);

                for hwnd_isize in hwnds {
                    let hwnd = HWND(hwnd_isize as _);
                    let _ = post_message_w(Some(hwnd), WM_APP_ANIMATE, WPARAM(0), LPARAM(0));
                }

                thread::sleep(Duration::from_millis(min_interval));
            }
        });

        Self { state }
    }

    fn register(&self, hwnd: HWND, interval_ms: u64) {
        let (lock, cvar) = &*self.state;
        let mut state = lock.lock().unwrap();
        state.targets.insert(hwnd.0 as isize, interval_ms);
        cvar.notify_one();
    }

    fn unregister(&self, hwnd: HWND) {
        let (lock, _) = &*self.state;
        let mut state = lock.lock().unwrap();
        state.targets.remove(&(hwnd.0 as isize));
    }
}

static CENTRAL_TICKER: LazyLock<CentralAnimTicker> = LazyLock::new(CentralAnimTicker::new);

#[derive(Debug)]
pub struct AnimationTimer {
    hwnd: HWND,
}

impl AnimationTimer {
    pub fn new(hwnd: HWND, interval_ms: u64) -> Self {
        CENTRAL_TICKER.register(hwnd, interval_ms);
        Self { hwnd }
    }
}

impl Drop for AnimationTimer {
    fn drop(&mut self) {
        CENTRAL_TICKER.unregister(self.hwnd);
    }
}
