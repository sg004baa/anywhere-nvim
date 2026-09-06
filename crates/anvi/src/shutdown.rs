//! Session-end notifications must reach the HWND procedure: synchronous
//! SendMessage delivery bypasses winit's queued-message hook.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::Sender;

use anvi_core::SpawnPolicy;
use anyhow::bail;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_SHUTTINGDOWN, WM_ENDSESSION, WM_NCDESTROY, WM_QUERYENDSESSION,
};

use crate::controller::Cmd;

const SUBCLASS_ID: usize = 1;

pub fn system_shutting_down() -> bool {
    // SAFETY: GetSystemMetrics has no pointer or thread-affinity requirements.
    unsafe { GetSystemMetrics(SM_SHUTTINGDOWN) != 0 }
}

struct HookData {
    hwnd: Cell<Option<HWND>>,
    policy: Arc<SpawnPolicy>,
    tx: Sender<Cmd>,
}

/// Main-thread-only guard, dropped before the Window. The subclass owns a
/// separate Rc so even unexpected window destruction cannot leave a dangling
/// callback pointer. WM_NCDESTROY and Drop release that reference exactly once.
pub struct SessionEndHook(Rc<HookData>);

impl SessionEndHook {
    pub fn install(hwnd: isize, policy: Arc<SpawnPolicy>, tx: Sender<Cmd>) -> anyhow::Result<Self> {
        let hwnd = HWND(hwnd as *mut _);
        let data = Rc::new(HookData {
            hwnd: Cell::new(Some(hwnd)),
            policy,
            tx,
        });
        let callback_data = Rc::into_raw(Rc::clone(&data));
        // SAFETY: called on the HWND's creating thread. callback_data stays
        // alive until removal or WM_NCDESTROY; the guard cannot cross threads.
        if !unsafe {
            SetWindowSubclass(
                hwnd,
                Some(session_end_proc),
                SUBCLASS_ID,
                callback_data as usize,
            )
        }
        .as_bool()
        {
            // SAFETY: installation failed, so no callback owns this reference.
            unsafe { drop(Rc::from_raw(callback_data)) };
            bail!("failed to install the session-end window subclass");
        }
        Ok(Self(data))
    }
}

impl Drop for SessionEndHook {
    fn drop(&mut self) {
        let Some(hwnd) = self.0.hwnd.get() else {
            return;
        };
        // SAFETY: guard and HWND are main-thread-only; the Window outlives us.
        if unsafe { RemoveWindowSubclass(hwnd, Some(session_end_proc), SUBCLASS_ID) }.as_bool() {
            self.0.hwnd.set(None);
            // SAFETY: this is the subclass's extra reference, not the guard's.
            unsafe { drop(Rc::from_raw(Rc::as_ptr(&self.0))) };
        } else {
            // Keep the callback's reference alive until WM_NCDESTROY rather
            // than freeing data a still-installed callback could access.
            tracing::warn!("could not remove the session-end window subclass");
        }
    }
}

unsafe extern "system" fn session_end_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    data: usize,
) -> LRESULT {
    // SAFETY: SetWindowSubclass owns an Rc reference until removal/destruction.
    let state = unsafe { &*(data as *const HookData) };
    match message {
        WM_QUERYENDSESSION => {
            state.policy.query_end_session();
            let _ = state.tx.send(Cmd::LifecycleChanged);
            tracing::info!("Windows session shutdown queried; nvim spawning paused");
            // No RPC, child termination, or wait in this callback.
            LRESULT(1)
        }
        WM_ENDSESSION => {
            if wparam.0 != 0 {
                state.policy.exit();
                let _ = state.tx.send(Cmd::LifecycleChanged);
                tracing::info!("Windows session shutdown confirmed");
            } else {
                state.policy.cancel_end_session();
                let _ = state.tx.send(Cmd::LifecycleChanged);
                tracing::info!("Windows session shutdown cancelled");
            }
            LRESULT(0)
        }
        WM_NCDESTROY => {
            state.hwnd.set(None);
            // SAFETY: this HWND is being destroyed on its owner thread. No
            // subsequent callback can use its data after NCDESTROY returns.
            unsafe {
                let _ = RemoveWindowSubclass(hwnd, Some(session_end_proc), SUBCLASS_ID);
                drop(Rc::from_raw(data as *const HookData));
                DefSubclassProc(hwnd, message, wparam, lparam)
            }
        }
        // SAFETY: forward all unrelated messages along the subclass chain.
        _ => unsafe { DefSubclassProc(hwnd, message, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, SendMessageW, WINDOW_EX_STYLE, WS_POPUP,
    };
    use windows::core::w;

    struct TestWindow(HWND);

    impl TestWindow {
        fn new() -> Self {
            // SAFETY: STATIC is a built-in window class. The hidden test HWND
            // and every operation on it stay on this test's thread.
            Self(unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    w!("STATIC"),
                    w!("anvi-session-end-test"),
                    WS_POPUP,
                    0,
                    0,
                    1,
                    1,
                    None,
                    None,
                    None,
                    None,
                )
                .expect("create the isolated test window")
            })
        }

        fn send(&self, message: u32, confirmed: bool) -> LRESULT {
            // SAFETY: this synchronous delivery reaches WndProc without a
            // message pump, unlike PostMessage-based regression tests.
            unsafe { SendMessageW(self.0, message, Some(WPARAM(usize::from(confirmed))), None) }
        }
    }

    impl Drop for TestWindow {
        fn drop(&mut self) {
            // SAFETY: the test owns this HWND on its creating thread.
            unsafe { DestroyWindow(self.0).expect("destroy the test window") };
        }
    }

    #[test]
    fn synchronous_session_messages_pause_resume_and_permanently_exit() {
        let window = TestWindow::new();
        let policy = Arc::new(SpawnPolicy::default());
        let (tx, _rx) = std::sync::mpsc::channel();
        let _hook = SessionEndHook::install(window.0.0 as isize, Arc::clone(&policy), tx).unwrap();

        assert_eq!(window.send(WM_QUERYENDSESSION, false), LRESULT(1));
        assert!(policy.check().is_err());
        assert!(!policy.is_exiting());
        window.send(WM_ENDSESSION, false);
        assert!(policy.check().is_ok());
        assert_eq!(window.send(WM_QUERYENDSESSION, false), LRESULT(1));
        window.send(WM_ENDSESSION, true);
        assert!(policy.is_exiting());
        window.send(WM_ENDSESSION, false);
        assert!(
            policy.check().is_err(),
            "late cancellation cannot undo exit"
        );
    }

    #[test]
    fn removing_the_hook_stops_notifications_before_the_window_is_dropped() {
        let window = TestWindow::new();
        let policy = Arc::new(SpawnPolicy::default());
        let (tx, _rx) = std::sync::mpsc::channel();
        let hook = SessionEndHook::install(window.0.0 as isize, Arc::clone(&policy), tx).unwrap();
        drop(hook);
        window.send(WM_QUERYENDSESSION, false);
        assert!(policy.check().is_ok());

        // WM_NCDESTROY may arrive first (e.g. an external window owner). Dropping
        // the guard afterward must neither double-free nor retain the callback.
        let (tx, _rx) = std::sync::mpsc::channel();
        let hook = SessionEndHook::install(window.0.0 as isize, Arc::clone(&policy), tx).unwrap();
        drop(window);
        drop(hook);
        assert!(policy.check().is_ok());
    }
}
