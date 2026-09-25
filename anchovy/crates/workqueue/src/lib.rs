// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Work queues and the processors that drain them. Work items are data;
//! behaviour lives in processors, each owning its state on a dedicated
//! thread. RPC handlers push items and await replies the items carry.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender, TrySendError};

/// Handles one work item at a time on a pool thread. Its state belongs to
/// that thread, so it needs no locking.
pub trait Processor<W> {
    fn process(&mut self, item: W);
}

/// The sending side of a pool's queue. Cheap to clone.
///
/// Each pool thread has its own channel: with one channel shared by every
/// thread, idle threads contend on its waker lock, which costs more than
/// small items take to process.
pub struct Queue<W> {
    shards: Arc<[Sender<W>]>,
    next: Arc<AtomicUsize>,
}

impl<W> Clone for Queue<W> {
    fn clone(&self) -> Self {
        Queue {
            shards: self.shards.clone(),
            next: self.next.clone(),
        }
    }
}

/// Why an item was not queued; the item comes back.
#[derive(Debug, PartialEq, Eq)]
pub enum PushError<W> {
    /// The queue is at capacity: the caller should refuse the work.
    Full(W),
    /// The pool has shut down.
    Closed(W),
}

impl<W> Queue<W> {
    /// Queues `item` on the threads in turn, skipping full ones, or returns
    /// it at once if every thread's queue is full.
    pub fn try_push(&self, mut item: W) -> Result<(), PushError<W>> {
        let first = self.next.fetch_add(1, Ordering::Relaxed);
        for i in 0..self.shards.len() {
            match self.shards[(first + i) % self.shards.len()].try_send(item) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Full(back)) => item = back,
                Err(TrySendError::Disconnected(back)) => return Err(PushError::Closed(back)),
            }
        }
        Err(PushError::Full(item))
    }
}

/// Threads running processors over one queue. Dropping it stops them:
/// each finishes the item in hand, items still queued are dropped (with
/// whatever reply channel they carry), and the threads are joined.
pub struct Pool<W> {
    shutdown: Vec<Sender<()>>,
    threads: Vec<JoinHandle<()>>,
    /// Keeps the queue open for the pool's lifetime, even with no threads.
    _work: Vec<Receiver<W>>,
}

impl<W> Pool<W> {
    /// Starts `threads` threads named `name-0`, `name-1`, …, each building
    /// its processor with `make` and taking items from its share of a queue
    /// of `capacity`.
    pub fn spawn<P>(
        name: &str,
        threads: usize,
        capacity: usize,
        make: impl Fn() -> P + Send + Sync + Clone + 'static,
    ) -> (Queue<W>, Pool<W>)
    where
        W: Send + 'static,
        P: Processor<W>,
    {
        // A pool without threads still has a queue, which fills.
        let shards = threads.max(1);
        let (items, work): (Vec<_>, Vec<_>) = (0..shards)
            .map(|_| crossbeam_channel::bounded(capacity.div_ceil(shards)))
            .unzip();
        let (shutdown, stops): (Vec<_>, Vec<_>) =
            (0..threads).map(|_| crossbeam_channel::bounded(0)).unzip();
        let threads = stops
            .into_iter()
            .enumerate()
            .map(|(i, stop)| {
                let (work, make) = (work[i].clone(), make.clone());
                std::thread::Builder::new()
                    .name(format!("{name}-{i}"))
                    .spawn(move || run(&work, &stop, make()))
                    .expect("thread spawn")
            })
            .collect();
        (
            Queue {
                shards: items.into(),
                next: Arc::new(AtomicUsize::new(0)),
            },
            Pool {
                shutdown,
                threads,
                _work: work,
            },
        )
    }
}

fn run<W, P: Processor<W>>(work: &Receiver<W>, stop: &Receiver<()>, mut processor: P) {
    loop {
        let item = crossbeam_channel::select! {
            recv(work) -> item => match item {
                Ok(item) => item,
                // Every queue handle is gone.
                Err(_) => return,
            },
            // The pool is dropping its sender: stop.
            recv(stop) -> _ => return,
        };
        // A panicking item must not take the thread, and with it every later
        // item, down. Its reply sender is dropped, which its sender sees.
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| processor.process(item)));
    }
}

impl<W> Drop for Pool<W> {
    fn drop(&mut self) {
        self.shutdown.clear();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    /// Counts its items and reports the thread it ran on.
    struct Echo {
        seen: usize,
    }

    impl Processor<(usize, mpsc::Sender<(usize, String, usize)>)> for Echo {
        fn process(&mut self, (n, reply): (usize, mpsc::Sender<(usize, String, usize)>)) {
            assert_ne!(n, 13, "unlucky");
            self.seen += 1;
            let thread = std::thread::current().name().unwrap().to_owned();
            reply.send((n, thread, self.seen)).unwrap();
        }
    }

    #[test]
    fn items_run_on_the_pool_threads() {
        let (queue, pool) = Pool::spawn("echo", 3, 64, || Echo { seen: 0 });
        let (tx, rx) = mpsc::channel();
        for n in 0..20 {
            queue.try_push((n, tx.clone())).unwrap();
        }
        drop(tx);
        let mut results: Vec<_> = rx.iter().collect();
        results.sort();
        // 13 panicked, and the thread that ran it went on to other items.
        assert_eq!(results.len(), 19);
        assert!(
            results
                .iter()
                .all(|(_, thread, _)| thread.starts_with("echo-"))
        );
        // Each thread's count is its own.
        let most = results.iter().map(|(_, _, seen)| *seen).max().unwrap();
        assert!(most < 19);
        drop(pool);
        assert!(matches!(
            queue.try_push((0, mpsc::channel().0)),
            Err(PushError::Closed(_))
        ));
    }

    /// Blocks until told to go on.
    struct Gate(mpsc::Receiver<()>);

    impl Processor<()> for Gate {
        fn process(&mut self, (): ()) {
            let _ = self.0.recv();
        }
    }

    #[test]
    fn a_full_queue_refuses() {
        let (open, gate) = mpsc::channel();
        let gate = std::sync::Arc::new(std::sync::Mutex::new(Some(gate)));
        let (queue, pool) = Pool::spawn("gate", 1, 2, move || {
            Gate(gate.lock().unwrap().take().unwrap())
        });
        // One in hand, two queued, then full.
        let mut pushed = 0;
        while queue.try_push(()).is_ok() {
            pushed += 1;
            assert!(pushed <= 3, "the queue never filled");
        }
        assert!(matches!(queue.try_push(()), Err(PushError::Full(()))));
        drop(open);
        drop(pool);
    }

    /// Signals when it starts an item, then blocks until any item may go on.
    struct Turnstile {
        started: mpsc::Sender<()>,
        go: std::sync::Arc<std::sync::Mutex<mpsc::Receiver<()>>>,
    }

    impl Processor<()> for Turnstile {
        fn process(&mut self, (): ()) {
            self.started.send(()).unwrap();
            let _ = self.go.lock().unwrap().recv();
        }
    }

    #[test]
    fn a_push_skips_full_threads() {
        let (started, starts) = mpsc::channel();
        let (go, gone) = mpsc::channel();
        let gone = std::sync::Arc::new(std::sync::Mutex::new(gone));
        let (queue, pool) = Pool::spawn("turnstile", 3, 3, move || Turnstile {
            started: started.clone(),
            go: gone.clone(),
        });
        // One item in each thread's hand, then one queued behind each.
        for _ in 0..3 {
            queue.try_push(()).unwrap();
        }
        for _ in 0..3 {
            starts.recv().unwrap();
        }
        for _ in 0..3 {
            queue.try_push(()).unwrap();
        }
        assert!(matches!(queue.try_push(()), Err(PushError::Full(()))));
        // One thread moves on to its queued item; wherever the rotation
        // points, the push finds that thread's free slot.
        go.send(()).unwrap();
        starts.recv().unwrap();
        queue.try_push(()).unwrap();
        assert!(matches!(queue.try_push(()), Err(PushError::Full(()))));
        drop(go);
        drop(pool);
    }

    #[test]
    fn a_pool_without_threads_holds_its_queue_open() {
        let (queue, pool) = Pool::spawn("idle", 0, 1, || Echo { seen: 0 });
        let item = || (0, mpsc::channel().0);
        queue.try_push(item()).unwrap();
        assert!(matches!(queue.try_push(item()), Err(PushError::Full(_))));
        drop(pool);
        assert!(matches!(queue.try_push(item()), Err(PushError::Closed(_))));
    }
}
