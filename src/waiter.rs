// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::{
    future::Future,
    marker::PhantomPinned,
    mem,
    pin::Pin,
    task::{Context, Poll, Waker},
};

use intrusive_collections::{
    intrusive_adapter,
    linked_list::{AtomicLink, LinkedList},
    UnsafeRef,
};

use crate::{cpu_relax, spin::SpinLock};

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
    Canceled,
}

struct Inner {
    state: State,
    cancel: fn(*const (), &Waiter, bool),
    owner: *const (),
    waiting_for: WaitingFor,
}

unsafe impl Send for Inner {}

// Indicates the queue to which the waiter belongs. It is the responsibility of the Mutex and
// Condvar implementations to update this value when adding/removing a Waiter from their respective
// waiter lists.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum WaitingFor {
    // The waiter is either not linked into  a waiter list or it is linked into a temporary list.
    None,
    // The waiter is linked into the Mutex's waiter list.
    Mutex,
    // The waiter is linked into the Condvar's waiter list.
    Condvar,
}

// Represents a thread currently blocked on a Condvar or on acquiring a Mutex.
pub struct Waiter {
    inner: SpinLock<Inner>,
    link: AtomicLink,
    kind: Kind,
    _pinned: PhantomPinned,
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
    //
    // Safety: Callers must ensure that `owner` outlives this `Waiter`.
    pub unsafe fn new(
        kind: Kind,
        waiting_for: WaitingFor,
        cancel: fn(*const (), &Waiter, bool),
        owner: *const (),
    ) -> Waiter {
        Waiter {
            inner: SpinLock::new(Inner {
                state: State::Init,
                cancel,
                owner,
                waiting_for,
            }),
            link: AtomicLink::new(),
            kind,
            _pinned: PhantomPinned,
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
        self.inner.lock().waiting_for
    }

    // Change the waiter list to which this `Waiter` belongs.
    pub fn set_waiting_for(&self, waiting_for: WaitingFor) {
        self.inner.lock().waiting_for = waiting_for;
    }

    pub fn cancel(waiter: UnsafeRef<Waiter>) {
        debug_assert!(!waiter.is_linked(), "Cannot cancel `Waiter` while linked");
        waiter.inner.lock().state = State::Finished;
    }

    // Wake up the thread associated with this `Waiter`. Panics if `waiting_for()` does not return
    // `WaitingFor::None` or if `is_linked()` returns true.
    pub fn wake(waiter: UnsafeRef<Waiter>) {
        debug_assert!(!waiter.is_linked(), "Cannot wake `Waiter` while linked");
        debug_assert_eq!(waiter.is_waiting_for(), WaitingFor::None);

        let mut inner = waiter.inner.lock();

        match mem::replace(&mut inner.state, State::Woken) {
            State::Waiting(waker) => {
                drop(inner);
                waker.wake();
            }
            State::Canceled => {
                inner.state = State::Finished;
            }
            _ => {}
        }
    }
}

impl Future for Waiter {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let mut inner = self.inner.lock();

        match &inner.state {
            State::Init => {
                inner.state = State::Waiting(cx.waker().clone());

                Poll::Pending
            }
            State::Waiting(w) if !w.will_wake(cx.waker()) => {
                let old_waker = mem::replace(&mut inner.state, State::Waiting(cx.waker().clone()));
                drop(inner);
                drop(old_waker);

                Poll::Pending
            }
            State::Waiting(_) => Poll::Pending,
            State::Woken => {
                inner.state = State::Finished;
                Poll::Ready(())
            }
            State::Canceled | State::Finished => {
                panic!("Future polled after returning Poll::Ready or being canceled");
            }
        }
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        let mut inner = self.inner.lock();

        match &inner.state {
            State::Finished => {}
            State::Woken => {
                // We were woken but not polled. Go directly to `State::Finished` since nothing is
                // going to poll, wake, or cancel us now.
                inner.state = State::Finished;
                let cancel = inner.cancel;
                let owner = inner.owner;
                drop(inner);

                // Since we were woken but didn't get a chance to run, wake up the next waiter.
                cancel(owner, self, true);

                inner = self.inner.lock();
            }
            State::Canceled => panic!("Waiter dropped twice"),
            _ => {
                inner.state = State::Canceled;
                let cancel = inner.cancel;
                let owner = inner.owner;
                drop(inner);

                // Not woken.  No need to wake up any waiters.
                cancel(owner, self, false);

                inner = self.inner.lock();
            }
        }

        // Wait until we reach `State::Finished`. This guarantees that the UnsafeRef that we handed
        // to the linked list has been dropped.
        let mut spin_count = 0;
        while !matches!(inner.state, State::Finished) {
            // If we haven't yet reached `State::Finished`, then that means that the `Waiter` was
            // transferred to a temporary list in another task in preparation of being woken up.
            // Wait until that task calls `Waiter::wake`, which will update the state to
            // `State::Finished`.
            drop(inner);
            cpu_relax(1 << spin_count);
            if spin_count < 7 {
                spin_count += 1;
            }
            inner = self.inner.lock();
        }
    }
}

intrusive_adapter!(pub WaiterAdapter = UnsafeRef<Waiter>: Waiter { link: AtomicLink });

pub type WaiterList = LinkedList<WaiterAdapter>;
