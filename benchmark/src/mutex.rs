// Copyright 2016 Amanieu d'Antras
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

mod args;
use crate::args::ArgRange;

#[cfg(any(windows, unix))]
use std::cell::UnsafeCell;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use futures::{
    channel::mpsc::channel,
    executor::{block_on, ThreadPool},
    SinkExt, StreamExt,
};

struct Barrier {
    mu: posh::Mutex<usize>,
    cv: posh::Condvar,
}

impl Barrier {
    fn new(count: usize) -> Barrier {
        Barrier {
            mu: posh::Mutex::new(count),
            cv: posh::Condvar::new(),
        }
    }

    async fn wait(&self) {
        let mut count = self.mu.lock().await;
        let oldcount = *count;
        *count -= 1;
        drop(count);

        if oldcount == 1 {
            self.cv.notify_all();
        } else {
            let mut count = self.mu.read_lock().await;
            while *count > 0 {
                count = self.cv.wait_read(count).await;
            }
        }
    }
}

trait Mutex<T> {
    fn new(v: T) -> Self;
    fn lock<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut T) -> R;
    fn name() -> &'static str;
}

impl<T> Mutex<T> for std::sync::Mutex<T> {
    fn new(v: T) -> Self {
        Self::new(v)
    }
    fn lock<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut T) -> R,
    {
        f(&mut *self.lock().unwrap())
    }
    fn name() -> &'static str {
        "std::sync::Mutex"
    }
}

impl<T> Mutex<T> for parking_lot::Mutex<T> {
    fn new(v: T) -> Self {
        Self::new(v)
    }
    fn lock<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut T) -> R,
    {
        f(&mut *self.lock())
    }
    fn name() -> &'static str {
        "parking_lot::Mutex"
    }
}

#[cfg(not(windows))]
type SrwLock<T> = std::sync::Mutex<T>;

#[cfg(windows)]
use winapi::um::synchapi;
#[cfg(windows)]
struct SrwLock<T>(UnsafeCell<T>, UnsafeCell<synchapi::SRWLOCK>);
#[cfg(windows)]
unsafe impl<T> Sync for SrwLock<T> {}
#[cfg(windows)]
unsafe impl<T: Send> Send for SrwLock<T> {}
#[cfg(windows)]
impl<T> Mutex<T> for SrwLock<T> {
    fn new(v: T) -> Self {
        let mut h: synchapi::SRWLOCK = synchapi::SRWLOCK {
            Ptr: std::ptr::null_mut(),
        };

        unsafe {
            synchapi::InitializeSRWLock(&mut h);
        }
        SrwLock(UnsafeCell::new(v), UnsafeCell::new(h))
    }
    fn lock<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut T) -> R,
    {
        unsafe {
            synchapi::AcquireSRWLockExclusive(self.1.get());
            let res = f(&mut *self.0.get());
            synchapi::ReleaseSRWLockExclusive(self.1.get());
            res
        }
    }
    fn name() -> &'static str {
        "winapi_srwlock"
    }
}

#[cfg(not(unix))]
type PthreadMutex<T> = std::sync::Mutex<T>;

#[cfg(unix)]
struct PthreadMutex<T>(UnsafeCell<T>, UnsafeCell<libc::pthread_mutex_t>);
#[cfg(unix)]
unsafe impl<T> Sync for PthreadMutex<T> {}
#[cfg(unix)]
impl<T> Mutex<T> for PthreadMutex<T> {
    fn new(v: T) -> Self {
        PthreadMutex(
            UnsafeCell::new(v),
            UnsafeCell::new(libc::PTHREAD_MUTEX_INITIALIZER),
        )
    }
    fn lock<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut T) -> R,
    {
        unsafe {
            libc::pthread_mutex_lock(self.1.get());
            let res = f(&mut *self.0.get());
            libc::pthread_mutex_unlock(self.1.get());
            res
        }
    }
    fn name() -> &'static str {
        "pthread_mutex_t"
    }
}
#[cfg(unix)]
impl<T> Drop for PthreadMutex<T> {
    fn drop(&mut self) {
        unsafe {
            libc::pthread_mutex_destroy(self.1.get());
        }
    }
}

// We have to use a macro instead of generic because we can't have async traits without GATs.
macro_rules! async_mutex {
    ($mod:ident, $ty:ty, $name:literal) => {
        mod $mod {
            use std::{
                sync::{
                    atomic::{AtomicBool, Ordering},
                    Arc,
                },
                thread,
                time::Duration,
            };

            use futures::{
                channel::mpsc::channel,
                executor::{block_on, ThreadPool},
                SinkExt, StreamExt,
            };

            use super::Barrier;

            #[inline]
            async fn with_lock<F, R>(mu: &$ty, f: F) -> R
            where
                F: FnOnce(&mut f64) -> R,
            {
                f(&mut *mu.lock().await)
            }

            fn run_benchmark(
                num_tasks: usize,
                work_per_critical_section: usize,
                work_between_critical_sections: usize,
                seconds_per_test: usize,
                ex: ThreadPool,
            ) -> Vec<usize> {
                let lock = Arc::new(([0u8; 300], <$ty>::new(0.0), [0u8; 300]));
                let barrier = Arc::new(Barrier::new(num_tasks));
                let keep_going = Arc::new(AtomicBool::new(true));
                let (tx, rx) = channel(num_tasks);
                for _ in 0..num_tasks {
                    let lock = lock.clone();
                    let barrier = barrier.clone();
                    let keep_going = keep_going.clone();
                    let mut tx = tx.clone();
                    ex.spawn_ok(async move {
                        let mut local_value = 0.0;
                        let mut value = 0.0;
                        let mut iterations = 0usize;
                        barrier.wait().await;
                        while keep_going.load(Ordering::Relaxed) {
                            with_lock(&lock.1, |shared_value| {
                                for _ in 0..work_per_critical_section {
                                    *shared_value += value;
                                    *shared_value *= 1.01;
                                    value = *shared_value;
                                }
                            })
                            .await;
                            for _ in 0..work_between_critical_sections {
                                local_value += value;
                                local_value *= 1.01;
                                value = local_value;
                            }
                            iterations += 1;
                        }
                        tx.send((iterations, value)).await.unwrap();
                    });
                }

                thread::sleep(Duration::from_secs(seconds_per_test as u64));
                keep_going.store(false, Ordering::Relaxed);

                drop(tx);
                block_on(rx.map(|x| x.0).collect())
            }

            pub fn run_benchmark_iterations(
                num_tasks: usize,
                work_per_critical_section: usize,
                work_between_critical_sections: usize,
                seconds_per_test: usize,
                test_iterations: usize,
                ex: ThreadPool,
            ) {
                let mut data = vec![];
                for _ in 0..test_iterations {
                    let run_data = run_benchmark(
                        num_tasks,
                        work_per_critical_section,
                        work_between_critical_sections,
                        seconds_per_test,
                        ex.clone(),
                    );
                    data.extend_from_slice(&run_data);
                }

                let total = data.iter().fold(0f64, |a, b| a + *b as f64);
                let average = total / data.len() as f64;
                let variance = data
                    .iter()
                    .fold(0f64, |a, b| a + ((*b as f64 - average).powi(2)))
                    / data.len() as f64;
                data.sort();

                let k_hz = 1.0 / seconds_per_test as f64 / 1000.0;
                println!(
                    "{:20} | {:10.3} kHz | {:10.3} kHz | {:10.3} kHz | {:10.3} kHz",
                    $name,
                    total * k_hz,
                    average * k_hz,
                    data[data.len() / 2] as f64 * k_hz,
                    variance.sqrt() * k_hz
                );
            }
        }
    };
}

async_mutex!(posh_bench, posh::Mutex<f64>, "posh::Mutex");
async_mutex!(tokio_bench, tokio::sync::Mutex<f64>, "tokio::Mutex");
async_mutex!(async_std_bench, async_std::sync::Mutex<f64>, "async_std::Mutex");

fn run_benchmark<M: Mutex<f64> + Send + Sync + 'static>(
    num_tasks: usize,
    work_per_critical_section: usize,
    work_between_critical_sections: usize,
    seconds_per_test: usize,
    ex: ThreadPool,
) -> Vec<usize> {
    let lock = Arc::new(([0u8; 300], M::new(0.0), [0u8; 300]));
    let barrier = Arc::new(Barrier::new(num_tasks));
    let keep_going = Arc::new(AtomicBool::new(true));
    let (tx, rx) = channel(num_tasks);
    for _ in 0..num_tasks {
        let lock = lock.clone();
        let barrier = barrier.clone();
        let keep_going = keep_going.clone();
        let mut tx = tx.clone();
        ex.spawn_ok(async move {
            let mut local_value = 0.0;
            let mut value = 0.0;
            let mut iterations = 0usize;
            barrier.wait().await;
            while keep_going.load(Ordering::Relaxed) {
                lock.1.lock(|shared_value| {
                    for _ in 0..work_per_critical_section {
                        *shared_value += value;
                        *shared_value *= 1.01;
                        value = *shared_value;
                    }
                });
                for _ in 0..work_between_critical_sections {
                    local_value += value;
                    local_value *= 1.01;
                    value = local_value;
                }
                iterations += 1;
            }
            tx.send((iterations, value)).await.unwrap();
        });
    }

    thread::sleep(Duration::from_secs(seconds_per_test as u64));
    keep_going.store(false, Ordering::Relaxed);

    drop(tx);
    block_on(rx.map(|x| x.0).collect())
}

fn run_benchmark_iterations<M: Mutex<f64> + Send + Sync + 'static>(
    num_tasks: usize,
    work_per_critical_section: usize,
    work_between_critical_sections: usize,
    seconds_per_test: usize,
    test_iterations: usize,
    ex: ThreadPool,
) {
    let mut data = vec![];
    for _ in 0..test_iterations {
        let run_data = run_benchmark::<M>(
            num_tasks,
            work_per_critical_section,
            work_between_critical_sections,
            seconds_per_test,
            ex.clone(),
        );
        data.extend_from_slice(&run_data);
    }

    let total = data.iter().fold(0f64, |a, b| a + *b as f64);
    let average = total / data.len() as f64;
    let variance = data
        .iter()
        .fold(0f64, |a, b| a + ((*b as f64 - average).powi(2)))
        / data.len() as f64;
    data.sort();

    let k_hz = 1.0 / seconds_per_test as f64 / 1000.0;
    println!(
        "{:20} | {:10.3} kHz | {:10.3} kHz | {:10.3} kHz | {:10.3} kHz",
        M::name(),
        total * k_hz,
        average * k_hz,
        data[data.len() / 2] as f64 * k_hz,
        variance.sqrt() * k_hz
    );
}

fn run_all(
    args: &[ArgRange],
    first: &mut bool,
    num_tasks: usize,
    work_per_critical_section: usize,
    work_between_critical_sections: usize,
    seconds_per_test: usize,
    test_iterations: usize,
) {
    if num_tasks == 0 {
        return;
    }
    if *first || !args[0].is_single() {
        println!("- Running with {} tasks", num_tasks);
    }
    if *first || !args[1].is_single() || !args[2].is_single() {
        println!(
            "- {} iterations inside lock, {} iterations outside lock",
            work_per_critical_section, work_between_critical_sections
        );
    }
    if *first || !args[3].is_single() {
        println!("- {} seconds per test", seconds_per_test);
    }
    *first = false;

    println!(
        "{:^20} | {:^14} | {:^14} | {:^14} | {:^14}",
        "name", "total", "average", "median", "std.dev."
    );

    let ex = ThreadPool::builder().pool_size(num_tasks).create().unwrap();

    tokio_bench::run_benchmark_iterations(
        num_tasks,
        work_per_critical_section,
        work_between_critical_sections,
        seconds_per_test,
        test_iterations,
        ex.clone(),
    );

    posh_bench::run_benchmark_iterations(
        num_tasks,
        work_per_critical_section,
        work_between_critical_sections,
        seconds_per_test,
        test_iterations,
        ex.clone(),
    );

    async_std_bench::run_benchmark_iterations(
        num_tasks,
        work_per_critical_section,
        work_between_critical_sections,
        seconds_per_test,
        test_iterations,
        ex.clone(),
    );

    run_benchmark_iterations::<parking_lot::Mutex<f64>>(
        num_tasks,
        work_per_critical_section,
        work_between_critical_sections,
        seconds_per_test,
        test_iterations,
        ex.clone(),
    );

    run_benchmark_iterations::<std::sync::Mutex<f64>>(
        num_tasks,
        work_per_critical_section,
        work_between_critical_sections,
        seconds_per_test,
        test_iterations,
        ex.clone(),
    );
    if cfg!(windows) {
        run_benchmark_iterations::<SrwLock<f64>>(
            num_tasks,
            work_per_critical_section,
            work_between_critical_sections,
            seconds_per_test,
            test_iterations,
            ex.clone(),
        );
    }
    if cfg!(unix) {
        run_benchmark_iterations::<PthreadMutex<f64>>(
            num_tasks,
            work_per_critical_section,
            work_between_critical_sections,
            seconds_per_test,
            test_iterations,
            ex.clone(),
        );
    }
}

fn main() {
    let args = args::parse(&[
        "numTasks",
        "workPerCriticalSection",
        "workBetweenCriticalSections",
        "secondsPerTest",
        "testIterations",
    ]);
    let mut first = true;
    for num_tasks in args[0] {
        for work_per_critical_section in args[1] {
            for work_between_critical_sections in args[2] {
                for seconds_per_test in args[3] {
                    for test_iterations in args[4] {
                        run_all(
                            &args,
                            &mut first,
                            num_tasks,
                            work_per_critical_section,
                            work_between_critical_sections,
                            seconds_per_test,
                            test_iterations,
                        );
                    }
                }
            }
        }
    }
}
