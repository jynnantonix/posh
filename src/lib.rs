// Copyright 2022 Chirantan Ekbote
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![cfg_attr(all(not(feature = "std"), not(test)), no_std)]

use core::{
    future::Future,
    hint,
    pin::Pin,
    task::{Context, Poll},
};

#[cfg(any(feature = "std", test))]
mod block_on;
mod cv;
mod mu;
mod spin;
mod waiter;

#[cfg(any(feature = "std", test))]
pub use block_on::block_on;
pub use cv::Condvar;
pub use mu::{Mutex, MutexGuard, MutexReadGuard};
pub use spin::{SpinLock, SpinLockGuard};

#[inline]
pub(crate) fn cpu_relax(iterations: usize) {
    for _ in 0..iterations {
        hint::spin_loop();
    }
}

pub(crate) fn yield_now() -> YieldNow {
    YieldNow { polled: false }
}

pub(crate) struct YieldNow {
    polled: bool,
}

impl Future for YieldNow {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.polled {
            return Poll::Ready(());
        }

        self.polled = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}
