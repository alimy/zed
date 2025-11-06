use std::{
    cell::RefCell,
    thread::{ThreadId, current},
    time::{Duration, Instant},
};

use flume::Sender;
use util::ResultExt;
use windows::{
    System::Threading::{
        ThreadPool, ThreadPoolTimer, TimerElapsedHandler, WorkItemHandler, WorkItemPriority,
    },
    Win32::{
        Foundation::{LPARAM, WPARAM},
        UI::WindowsAndMessaging::PostMessageW,
    },
};

use crate::{
    HWND, PlatformDispatcher, RunnableVariant, SafeHwnd, TaskLabel, TaskTiming,
    WM_GPUI_TASK_DISPATCHED_ON_MAIN_THREAD,
};

thread_local! {
    static THREAD_TIMINGS: RefCell<Box<crate::platform::TaskTimings>> = RefCell::new(crate::platform::TaskTimings::boxed());
}

pub(crate) struct WindowsDispatcher {
    main_sender: Sender<RunnableVariant>,
    main_thread_id: ThreadId,
    platform_window_handle: SafeHwnd,
    validation_number: usize,
}

impl WindowsDispatcher {
    pub(crate) fn new(
        main_sender: Sender<RunnableVariant>,
        platform_window_handle: HWND,
        validation_number: usize,
    ) -> Self {
        let main_thread_id = current().id();
        let platform_window_handle = platform_window_handle.into();

        WindowsDispatcher {
            main_sender,
            main_thread_id,
            platform_window_handle,
            validation_number,
        }
    }

    fn dispatch_on_threadpool(&self, runnable: RunnableVariant) {
        let handler = {
            let mut task_wrapper = Some(runnable);
            WorkItemHandler::new(move |_| {
                Self::execute_runnable(task_wrapper.take().unwrap());
                Ok(())
            })
        };
        ThreadPool::RunWithPriorityAsync(&handler, WorkItemPriority::High).log_err();
    }

    fn dispatch_on_threadpool_after(&self, runnable: RunnableVariant, duration: Duration) {
        let handler = {
            let mut task_wrapper = Some(runnable);
            TimerElapsedHandler::new(move |_| {
                Self::execute_runnable(task_wrapper.take().unwrap());
                Ok(())
            })
        };
        ThreadPoolTimer::CreateTimer(&handler, duration.into()).log_err();
    }

    #[inline(always)]
    pub(crate) fn execute_runnable(runnable: RunnableVariant) {
        let start = Instant::now();

        let location = match runnable {
            RunnableVariant::Meta(runnable) => {
                let location = runnable.metadata().location;
                runnable.run();
                location
            }
            RunnableVariant::Compat(runnable) => {
                runnable.run();
                core::panic::Location::caller()
            }
        };

        let end = Instant::now();
        let timing = TaskTiming {
            location,
            start,
            end,
        };

        THREAD_TIMINGS.with_borrow_mut(|timings| {
            if let Some(last_timing) = timings.iter_mut().rev().next() {
                if last_timing.location == timing.location {
                    last_timing.end = timing.end;
                    return;
                }
            }

            timings.push_back(timing);
        });
    }
}

impl PlatformDispatcher for WindowsDispatcher {
    fn get_current_thread_timings(&self) -> Vec<crate::TaskTiming> {
        THREAD_TIMINGS.with_borrow(|timings| {
            let mut vec = Vec::with_capacity(timings.len());

            let (s1, s2) = timings.as_slices();
            vec.extend_from_slice(s1);
            vec.extend_from_slice(s2);
            vec
        })
    }

    fn is_main_thread(&self) -> bool {
        current().id() == self.main_thread_id
    }

    fn dispatch(&self, runnable: RunnableVariant, label: Option<TaskLabel>) {
        self.dispatch_on_threadpool(runnable);
        if let Some(label) = label {
            log::debug!("TaskLabel: {label:?}");
        }
    }

    fn dispatch_on_main_thread(&self, runnable: RunnableVariant) {
        match self.main_sender.send(runnable) {
            Ok(_) => unsafe {
                PostMessageW(
                    Some(self.platform_window_handle.as_raw()),
                    WM_GPUI_TASK_DISPATCHED_ON_MAIN_THREAD,
                    WPARAM(self.validation_number),
                    LPARAM(0),
                )
                .log_err();
            },
            Err(runnable) => {
                // NOTE: Runnable may wrap a Future that is !Send.
                //
                // This is usually safe because we only poll it on the main thread.
                // However if the send fails, we know that:
                // 1. main_receiver has been dropped (which implies the app is shutting down)
                // 2. we are on a background thread.
                // It is not safe to drop something !Send on the wrong thread, and
                // the app will exit soon anyway, so we must forget the runnable.
                std::mem::forget(runnable);
            }
        }
    }

    fn dispatch_after(&self, duration: Duration, runnable: RunnableVariant) {
        self.dispatch_on_threadpool_after(runnable, duration);
    }
}
