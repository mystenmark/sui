// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Work queues and the processors that drain them. Work items are data;
//! behaviour lives in processors, each owning its state on a dedicated
//! thread. RPC handlers push items and await replies the items carry.

use std::panic::AssertUnwindSafe;
use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender, TrySendError};

/// Handles one work item at a time on a pool thread. Its state belongs to
/// that thread, so it needs no locking.
pub trait Processor<W> {
    fn process(&mut self, item: W);
}

/// The sending side of a pool's queue. Cheap to clone.
pub struct Queue<W> {
    items: Sender<W>,
}

impl<W> Clone for Queue<W> {
    fn clone(&self) -> Self {
        Queue {
            items: self.items.clone(),
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
    /// Queues `item`, or returns it at once if the queue is full.
    pub fn try_push(&self, item: W) -> Result<(), PushError<W>> {
        self.items.try_send(item).map_err(|e| match e {
            TrySendError::Full(item) => PushError::Full(item),
            TrySendError::Disconnected(item) => PushError::Closed(item),
        })
    }
}

/// Threads running processors over one queue. Dropping it stops them:
/// each finishes the item in hand, items still queued are dropped (with
/// whatever reply channel they carry), and the threads are joined.
pub struct Pool {
    shutdown: Option<Sender<()>>,
    threads: Vec<JoinHandle<()>>,
}

impl Pool {
    /// Starts `threads` threads named `name-0`, `name-1`, …, each building
    /// its processor with `make` and taking items from a queue of
    /// `capacity`.
    pub fn spawn<W, P>(
        name: &str,
        threads: usize,
        capacity: usize,
        make: impl Fn() -> P + Send + Sync + Clone + 'static,
    ) -> (Queue<W>, Pool)
    where
        W: Send + 'static,
        P: Processor<W>,
    {
        let (items, work) = crossbeam_channel::bounded(capacity);
        let (shutdown, stop) = crossbeam_channel::bounded(0);
        let threads = (0..threads)
            .map(|i| {
                let (work, stop, make) = (work.clone(), stop.clone(), make.clone());
                std::thread::Builder::new()
                    .name(format!("{name}-{i}"))
                    .spawn(move || run(&work, &stop, make()))
                    .expect("thread spawn")
            })
            .collect();
        (
            Queue { items },
            Pool {
                shutdown: Some(shutdown),
                threads,
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

impl Drop for Pool {
    fn drop(&mut self) {
        drop(self.shutdown.take());
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
}
