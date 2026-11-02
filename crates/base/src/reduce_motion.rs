//! Honors the operating system's reduced-motion preference.
//!
//! Every Base transition, spring, presence and reveal consults
//! [`App::reduce_motion`], but GPUI never reads the platform's setting into
//! that flag: it stays `false` until something sets it. [`init`] reads the
//! setting when Base initializes, writes it into the flag and keeps following
//! it for the life of the application, so an application inherits the user's
//! choice, and later changes to it, by calling `gpui_base::init` or
//! `gpui_component::init`.
//!
//! The setting is read and followed on the platforms below; everywhere else
//! the flag is left alone.
//!
//! - macOS: `NSWorkspace.accessibilityDisplayShouldReduceMotion`, the
//!   "Reduce motion" switch under Accessibility → Display, re-read when the
//!   workspace posts `NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification`.
//! - Windows: `SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION)`, the
//!   "Animation effects" switch under Accessibility → Visual effects, whose
//!   off state asks for reduced motion, re-read when Windows broadcasts
//!   `WM_SETTINGCHANGE` for `SPI_SETCLIENTAREAANIMATION`.
//! - Linux: the `reduced-motion` key of the `org.freedesktop.appearance`
//!   namespace of the XDG desktop portal's Settings interface, which GNOME
//!   and KDE back with their own animation switches. The portal answers over
//!   D-Bus, so the reading lands a moment after `init` returns, and Base keeps
//!   following the portal's change signal.
//!
//! A change reaches the flag through a GPUI task, never from inside the
//! platform's callback: the observer only signals a channel, and the task
//! re-reads the setting on its next turn of the foreground executor.
//!
//! An application owns the flag once it sets it. Base writes the
//! flag only while it still holds what Base last wrote (or GPUI's initial
//! `false`), so an application that calls [`App::set_reduce_motion`] after
//! `init` is never overridden by a later reading, and one that wants to follow
//! the system again calls [`apply_system_reduce_motion`].
//!
//! Under GPUI's test scheduler the platform is never consulted, whichever
//! crate's tests are running: a probe that answers from another thread would
//! break the scheduler's determinism, and a test wanting reduced motion sets
//! the flag itself.

use gpui::{App, Global};

/// What Base last wrote into [`App::set_reduce_motion`], and whether it is
/// already listening for the platform to change its mind.
#[derive(Default)]
struct SystemReduceMotion {
    applied: Option<bool>,
    following: bool,
}

impl Global for SystemReduceMotion {}

/// Applies the system's reduced-motion preference during `gpui_base::init`.
pub(crate) fn init(cx: &mut App) {
    apply_system_reduce_motion(cx);
}

/// Reads the operating system's reduced-motion preference into
/// [`App::set_reduce_motion`].
///
/// `gpui_base::init` calls this once, and from then on Base follows the
/// preference on its own; calling it again re-reads the preference at once.
///
/// The application wins over the system: when the flag no longer holds what
/// Base last wrote, the application set it, and this call leaves it alone.
/// Where the platform cannot say (wasm, an unsupported desktop, a Linux
/// session without the portal) the flag is left as it is, and under GPUI's
/// test scheduler the platform is not asked at all.
pub fn apply_system_reduce_motion(cx: &mut App) {
    if is_test_scheduler(cx) {
        return;
    }
    apply_preference(platform::read(), cx);
    let state = cx.default_global::<SystemReduceMotion>();
    if !state.following {
        state.following = true;
        platform::follow(cx);
    }
}

/// Whether the application runs on GPUI's deterministic test scheduler,
/// which must not be woken by a platform answering from its own thread.
fn is_test_scheduler(cx: &App) -> bool {
    cx.background_executor()
        .scheduler_executor()
        .scheduler()
        .as_test()
        .is_some()
}

/// Writes one reading of the system preference into the flag, unless the
/// application has taken the flag over since the reading Base last applied.
fn apply_preference(preference: Option<bool>, cx: &mut App) {
    let Some(reduce) = preference else {
        return;
    };
    let applied = cx
        .try_global::<SystemReduceMotion>()
        .and_then(|state| state.applied);
    if cx.reduce_motion() != applied.unwrap_or(false) {
        return;
    }
    cx.set_reduce_motion(reduce);
    cx.default_global::<SystemReduceMotion>().applied = Some(reduce);
}

/// Applies `read` each time `changes` is signalled, for as long as a sender
/// lives. A signal already waiting means a re-read is coming, so a capacity
/// of one coalesces a burst of changes into one reading.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn follow_changes(changes: smol::channel::Receiver<()>, read: fn() -> Option<bool>, cx: &mut App) {
    cx.spawn(async move |cx| {
        while changes.recv().await.is_ok() {
            cx.update(|cx| apply_preference(read(), cx));
        }
    })
    .detach();
}

#[cfg(target_os = "macos")]
mod platform {
    use block2::RcBlock;
    use gpui::App;
    use objc2_app_kit::{NSWorkspace, NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification};

    pub(super) fn read() -> Option<bool> {
        Some(NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion())
    }

    /// AppKit posts the notification on the main thread from inside its own
    /// call stack, where `App` may already be borrowed, so the block only
    /// signals [`super::follow_changes`].
    pub(super) fn follow(cx: &mut App) {
        let (changed, changes) = smol::channel::bounded(1);
        let block = RcBlock::new(move |_| {
            _ = changed.try_send(());
        });
        // SAFETY: the name is AppKit's own constant, no sender object is
        // filtered on, and the block only sends on a channel, so it may run
        // on whichever queue posts the notification.
        unsafe {
            NSWorkspace::sharedWorkspace()
                .notificationCenter()
                .addObserverForName_object_queue_usingBlock(
                    Some(NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification),
                    None,
                    None,
                    &block,
                );
        }
        // The notification center holds the registration until it is
        // removed, and Base follows for the life of the application, so the
        // returned token is not kept.
        super::follow_changes(changes, read, cx);
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use std::sync::OnceLock;

    use gpui::App;
    use smol::channel::Sender;
    use windows::{
        Win32::{
            Foundation::{BOOL, HWND, LPARAM, LRESULT, WPARAM},
            UI::WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, RegisterClassW, SPI_GETCLIENTAREAANIMATION,
                SPI_SETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
                SystemParametersInfoW, WINDOW_STYLE, WM_SETTINGCHANGE, WNDCLASSW, WS_EX_TOOLWINDOW,
            },
        },
        core::w,
    };

    /// Where the listener window's procedure signals a change. A process runs
    /// one GPUI application, so one listener serves it.
    static CHANGED: OnceLock<Sender<()>> = OnceLock::new();

    pub(super) fn read() -> Option<bool> {
        let mut animations_enabled = BOOL(0);
        // SAFETY: SPI_GETCLIENTAREAANIMATION writes one BOOL through `pvParam`
        // and ignores `uiParam`; the BOOL outlives the call.
        unsafe {
            SystemParametersInfoW(
                SPI_GETCLIENTAREAANIMATION,
                0,
                Some((&mut animations_enabled as *mut BOOL).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        }
        .ok()?;
        Some(!animations_enabled.as_bool())
    }

    /// Windows broadcasts `WM_SETTINGCHANGE` to top-level windows only, so a
    /// message-only window never hears it. The listener is a hidden tool
    /// window, like GPUI's own dispatcher window, created on the main thread,
    /// whose message loop GPUI already runs.
    pub(super) fn follow(cx: &mut App) {
        let (changed, changes) = smol::channel::bounded(1);
        if CHANGED.set(changed).is_err() {
            return;
        }
        let class = WNDCLASSW {
            lpfnWndProc: Some(listener_procedure),
            lpszClassName: w!("GpuiBase::ReduceMotionListener"),
            ..Default::default()
        };
        // SAFETY: the class names a procedure that lives for the whole
        // program and a static class name; a hidden, zero-sized top-level
        // window takes no parent, menu or creation data.
        let created = unsafe {
            RegisterClassW(&class);
            CreateWindowExW(
                WS_EX_TOOLWINDOW,
                class.lpszClassName,
                None,
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                None,
                None,
                None,
                None,
            )
        };
        if created.is_ok() {
            super::follow_changes(changes, read, cx);
        }
    }

    unsafe extern "system" fn listener_procedure(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        // `SystemParametersInfoW` broadcasts the action it changed as
        // `wparam`; other settings changes leave the preference alone.
        if msg == WM_SETTINGCHANGE
            && wparam.0 == SPI_SETCLIENTAREAANIMATION.0 as usize
            && let Some(changed) = CHANGED.get()
        {
            _ = changed.try_send(());
        }
        // SAFETY: the arguments are the ones Windows passed this procedure.
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use ashpd::desktop::settings::{ReducedMotion, Settings};
    use futures::StreamExt as _;
    use gpui::App;

    /// The portal answers asynchronously; [`follow`] applies the first reading.
    pub(super) fn read() -> Option<bool> {
        None
    }

    pub(super) fn follow(cx: &mut App) {
        cx.spawn(async move |cx| {
            let settings = Settings::new().await.ok()?;
            let current = settings.reduced_motion().await.ok()?;
            cx.update(|cx| super::apply_preference(Some(reduces(current)), cx));
            let mut changes = settings.receive_reduced_motion_changed().await.ok()?;
            while let Some(preference) = changes.next().await {
                cx.update(|cx| super::apply_preference(Some(reduces(preference)), cx));
            }
            Some(())
        })
        .detach();
    }

    fn reduces(preference: ReducedMotion) -> bool {
        matches!(preference, ReducedMotion::ReducedMotion)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
mod platform {
    use gpui::App;

    pub(super) fn read() -> Option<bool> {
        None
    }

    pub(super) fn follow(_cx: &mut App) {}
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;

    use super::{apply_preference, apply_system_reduce_motion};

    #[gpui::test]
    fn a_system_preference_for_reduced_motion_sets_the_flag(cx: &mut TestAppContext) {
        cx.update(|cx| {
            apply_preference(Some(true), cx);
            assert!(cx.reduce_motion());
        });
    }

    #[gpui::test]
    fn an_unknown_system_preference_leaves_the_flag_alone(cx: &mut TestAppContext) {
        cx.update(|cx| {
            apply_preference(None, cx);
            assert!(!cx.reduce_motion());

            cx.set_reduce_motion(true);
            apply_preference(None, cx);
            assert!(cx.reduce_motion());
        });
    }

    #[gpui::test]
    fn the_system_drives_the_flag_until_the_application_sets_it(cx: &mut TestAppContext) {
        cx.update(|cx| {
            apply_preference(Some(true), cx);
            apply_preference(Some(false), cx);
            assert!(!cx.reduce_motion());
            apply_preference(Some(true), cx);
            assert!(cx.reduce_motion());

            cx.set_reduce_motion(false);
            apply_preference(Some(true), cx);
            assert!(!cx.reduce_motion());
        });
    }

    #[gpui::test]
    fn the_test_scheduler_is_never_asked_for_the_platform_preference(cx: &mut TestAppContext) {
        cx.update(|cx| {
            apply_system_reduce_motion(cx);
            assert!(!cx.reduce_motion());
        });
        cx.run_until_parked();
        cx.update(|cx| assert!(!cx.reduce_motion()));
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[gpui::test]
    fn a_signalled_change_is_read_until_the_application_sets_the_flag(cx: &mut TestAppContext) {
        use std::sync::atomic::{AtomicBool, Ordering};

        static SYSTEM: AtomicBool = AtomicBool::new(false);
        fn read() -> Option<bool> {
            Some(SYSTEM.load(Ordering::SeqCst))
        }

        let (changed, changes) = smol::channel::bounded(1);
        cx.update(|cx| super::follow_changes(changes, read, cx));

        SYSTEM.store(true, Ordering::SeqCst);
        changed.try_send(()).unwrap();
        cx.run_until_parked();
        cx.update(|cx| assert!(cx.reduce_motion()));

        cx.update(|cx| cx.set_reduce_motion(false));
        changed.try_send(()).unwrap();
        cx.run_until_parked();
        cx.update(|cx| assert!(!cx.reduce_motion()));
    }

    #[gpui::test]
    fn a_flag_the_application_set_before_the_first_reading_is_kept(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.set_reduce_motion(true);
            apply_preference(Some(false), cx);
            assert!(cx.reduce_motion());
        });
    }
}
