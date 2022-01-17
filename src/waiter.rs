// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::{
    future::Future,
    mem,
    pin::Pin,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
    task::{Context, Poll, Waker},
};

use intrusive_collections::{
    intrusive_adapter,
    linked_list::{AtomicLink, LinkedList},
};

use crate::spin::SpinLock;

#[derive(Clone, Copy)]
pub enum Kind {
    Shared,
    Exclusive,
}

enum State {
    Init,
    Waiting(Waker),
    Woken,
    Finished,
}

// Indicates the queue to which the waiter belongs. It is the responsibility of the Mutex and
// Condvar implementations to update this value when adding/removing a Waiter from their respective
// waiter lists.
#[repr(u8)]
#[derive(Debug, Eq, PartialEq)]
pub enum WaitingFor {
    // The waiter is either not linked into  a waiter list or it is linked into a temporary list.
    None = 0,
    // The waiter is linked into the Mutex's waiter list.
    Mutex = 1,
    // The waiter is linked into the Condvar's waiter list.
    Condvar = 2,
}

pub trait Cancel {
    fn cancel(&self, waiter: &Waiter, wake_next: bool);
}

// Represents a thread currently blocked on a Condvar or on acquiring a Mutex.
pub struct Waiter {
    link: AtomicLink,
    state: SpinLock<State>,
    kind: Kind,
    waiting_for: AtomicU8,
}

impl Waiter {
    // Create a new, initialized Waiter.
    //
    // `kind` should indicate whether this waiter represent a thread that is waiting for a shared
    // lock or an exclusive lock.
    //
    // `cancel` is the function that is called when a `WaitFuture` (returned by the `wait()`
    // function) is dropped before it can complete. `cancel_data` is used as the first parameter of
    // the `cancel` function. The second parameter is the `Waiter` that was canceled and the third
    // parameter indicates whether the `WaitFuture` was dropped after it was woken (but before it
    // was polled to completion). A value of `false` for the third parameter may already be stale
    // by the time the cancel function runs and so does not guarantee that the waiter was not woken.
    // In this case, implementations should still check if the Waiter was woken. However, a value of
    // `true` guarantees that the waiter was already woken up so no additional checks are necessary.
    // In this case, the cancel implementation should wake up the next waiter in its wait list, if
    // any.
    //
    // `waiting_for` indicates the waiter list to which this `Waiter` will be added. See the
    // documentation of the `WaitingFor` enum for the meaning of the different values.
    pub fn new(
        kind: Kind,
        waiting_for: WaitingFor,
    ) -> Waiter {
        Waiter {
            link: AtomicLink::new(),
            state: SpinLock::new(State::Init),
            kind,
            waiting_for: AtomicU8::new(waiting_for as u8),
        }
    }

    // The kind of lock that this `Waiter` is waiting to acquire.
    pub fn kind(&self) -> Kind {
        self.kind
    }

    // Returns true if this `Waiter` is currently linked into a waiter list.
    pub fn is_linked(&self) -> bool {
        self.link.is_linked()
    }

    // Indicates the waiter list to which this `Waiter` belongs.
    pub fn is_waiting_for(&self) -> WaitingFor {
        match self.waiting_for.load(Ordering::Acquire) {
            0 => WaitingFor::None,
            1 => WaitingFor::Mutex,
            2 => WaitingFor::Condvar,
            v => panic!("Unknown value for `WaitingFor`: {}", v),
        }
    }

    // Change the waiter list to which this `Waiter` belongs. This will panic if called when the
    // `Waiter` is still linked into a waiter list.
    pub fn set_waiting_for(&self, waiting_for: WaitingFor) {
        self.waiting_for.store(waiting_for as u8, Ordering::Release);
    }

    // Reset the Waiter back to its initial state. Panics if this `Waiter` is still linked into a
    // waiter list.
    pub fn reset(&self, waiting_for: WaitingFor) {
        debug_assert!(!self.is_linked(), "Cannot reset `Waiter` while linked");
        self.set_waiting_for(waiting_for);

        let mut state = self.state.lock();
        if let State::Waiting(waker) = mem::replace(&mut *state, State::Init) {
            mem::drop(state);
            mem::drop(waker);
        }
    }

    // Wait until woken up by another thread.
    pub fn wait<'w, 'c, C: Cancel>(&'w self, owner: &'c C) -> WaitFuture<'w, 'c, C> {
        WaitFuture { waiter: self, owner }
    }

    // Wake up the thread associated with this `Waiter`. Panics if `waiting_for()` does not return
    // `WaitingFor::None` or if `is_linked()` returns true.
    pub fn wake(&self) {
        debug_assert!(!self.is_linked(), "Cannot wake `Waiter` while linked");
        debug_assert_eq!(self.is_waiting_for(), WaitingFor::None);

        let mut state = self.state.lock();

        if let State::Waiting(waker) = mem::replace(&mut *state, State::Woken) {
            mem::drop(state);
            waker.wake();
        }
    }
}

pub struct WaitFuture<'w, 'c, C: Cancel> {
    waiter: &'w Waiter,
    owner: &'c C,
}

impl<'w, 'c, C: Cancel> Future for WaitFuture<'w, 'c, C> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let mut state = self.waiter.state.lock();

        match &*state {
            State::Init => {
                *state = State::Waiting(cx.waker().clone());

                Poll::Pending
            }
            State::Waiting(w) if !w.will_wake(cx.waker()) => {
                let old_waker = mem::replace(&mut *state, State::Waiting(cx.waker().clone()));
                mem::drop(state);
                mem::drop(old_waker);

                Poll::Pending
            }
            State::Waiting(_) => Poll::Pending,
            State::Woken => {
                *state = State::Finished;
                Poll::Ready(())
            }
            State::Finished => {
                panic!("Future polled after returning Poll::Ready");
            }
        }
    }
}

impl<'w, 'c, C: Cancel> Drop for WaitFuture<'w, 'c, C> {
    fn drop(&mut self) {
        let state = self.waiter.state.lock();

        match *state {
            State::Finished => {}
            State::Woken => {
                mem::drop(state);

                // We were woken but not polled.  Wake up the next waiter.
                self.owner.cancel(self.waiter, true);
            }
            _ => {
                mem::drop(state);

                // Not woken.  No need to wake up any waiters.
                self.owner.cancel(self.waiter, false);
            }
        }
    }
}

intrusive_adapter!(pub WaiterAdapter = Arc<Waiter>: Waiter { link: AtomicLink });

pub type WaiterList = LinkedList<WaiterAdapter>;
