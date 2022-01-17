// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::{
    future::Future,
    sync::Arc,
    task::{Context, Poll},
    thread::{self, Thread},
};

use futures::{
    pin_mut,
    task::{waker_ref, ArcWake},
};

thread_local!(static THREAD_WAKER: Arc<Waker> = Arc::new(Waker(thread::current())));

#[repr(transparent)]
struct Waker(Thread);

impl ArcWake for Waker {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.0.unpark()
    }
}

/// Run a future to completion on the current thread.
///
/// This method will block the current thread until `f` completes. Useful when you need to call an
/// async fn from a non-async context.
pub fn block_on<F: Future>(f: F) -> F::Output {
    pin_mut!(f);

    THREAD_WAKER.with(|thread_waker| {
        let waker = waker_ref(thread_waker);
        let mut cx = Context::from_waker(&waker);

        loop {
            if let Poll::Ready(t) = f.as_mut().poll(&mut cx) {
                return t;
            }
            thread::park();
        }
    })
}

#[cfg(test)]
mod test {
    use std::{
        future::Future,
        pin::Pin,
        sync::{
            mpsc::{channel, Sender},
            Arc,
        },
        task::{Context, Poll, Waker},
        thread,
        time::Duration,
    };

    use super::*;
    use crate::SpinLock;

    struct TimerState {
        fired: bool,
        waker: Option<Waker>,
    }
    struct Timer {
        state: Arc<SpinLock<TimerState>>,
    }

    impl Future for Timer {
        type Output = ();

        fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
            let mut state = self.state.lock();
            if state.fired {
                return Poll::Ready(());
            }

            state.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }

    fn start_timer(dur: Duration, notify: Option<Sender<()>>) -> Timer {
        let state = Arc::new(SpinLock::new(TimerState {
            fired: false,
            waker: None,
        }));

        let thread_state = Arc::clone(&state);
        thread::spawn(move || {
            thread::sleep(dur);
            let mut ts = thread_state.lock();
            ts.fired = true;
            if let Some(waker) = ts.waker.take() {
                waker.wake();
            }
            drop(ts);

            if let Some(tx) = notify {
                tx.send(()).expect("Failed to send completion notification");
            }
        });

        Timer { state }
    }

    #[test]
    fn it_works() {
        block_on(start_timer(Duration::from_millis(100), None));
    }

    #[test]
    fn nested() {
        async fn inner() {
            block_on(start_timer(Duration::from_millis(100), None));
        }

        block_on(inner());
    }

    #[test]
    fn ready_before_poll() {
        let (tx, rx) = channel();

        let timer = start_timer(Duration::from_millis(50), Some(tx));

        rx.recv()
            .expect("Failed to receive completion notification");

        // We know the timer has already fired so the poll should complete immediately.
        block_on(timer);
    }
}
