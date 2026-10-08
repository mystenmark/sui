// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Work queues and the processors that drain them. Work items are data;
//! behaviour lives in processors, each owning its state on a dedicated
//! thread. RPC handlers push items and await replies the items carry.
//!
//! Queues and threads are separate: a [`Worker`] thread runs any number of
//! processors, each draining its own [`Inbox`], and an inbox given to
//! several workers is drained by all of them.
//!
//! A processor declares what it takes and what it emits, and the worker
//! sends what it emits to a [`Sink`], typically the next processor's queue:
//! stages whose types do not match cannot be wired together.

use std::cell::RefCell;
use std::panic::AssertUnwindSafe;
use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Select, Sender, TryRecvError, TrySendError};

/// Handles one work item at a time on a worker thread. Its state belongs to
/// that thread, so it needs no locking.
pub trait Processor {
    type Input: Send + 'static;
    type Output;

    /// What it returns, if anything, goes to the worker's sink for it;
    /// `None` when the item is dealt with here.
    fn process(&mut self, item: Self::Input) -> Option<Self::Output>;
}

/// Where a processor's outputs go.
pub trait Sink<T>: Send {
    fn send(&mut self, item: T);
}

/// A closure, typically the end of a pipeline.
impl<T, F: FnMut(T) + Send> Sink<T> for F {
    fn send(&mut self, item: T) {
        self(item);
    }
}

/// Why a queue did not take an item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Full,
    Closed,
}

/// An item that answers for itself when a queue will not take it: one that
/// carries a reply, say.
pub trait Refuse {
    fn refuse(self, why: Refusal);
}

/// The next processor's queue. Never blocks, so the next processor may run
/// on the same thread; a full queue refuses the item.
impl<T: Refuse + Send> Sink<T> for Queue<T> {
    fn send(&mut self, item: T) {
        match self.try_push(item) {
            Ok(()) => {}
            Err(PushError::Full(item)) => item.refuse(Refusal::Full),
            Err(PushError::Closed(item)) => item.refuse(Refusal::Closed),
        }
    }
}

/// A bounded queue: the sending end, and the receiving end to give to the
/// workers that drain it.
pub fn queue<W>(capacity: usize) -> (Queue<W>, Inbox<W>) {
    let (items, inbox) = crossbeam_channel::bounded(capacity);
    (Queue { items }, Inbox { items: inbox })
}

/// The sending side of a queue. Cheap to clone.
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
    /// Every inbox of the queue is gone.
    Closed(W),
}

impl<W> Queue<W> {
    /// Queues `item`, or returns it at once if the queue is full. Never
    /// blocks, so a processor may push to a queue drained by its own thread.
    pub fn try_push(&self, item: W) -> Result<(), PushError<W>> {
        self.items.try_send(item).map_err(|e| match e {
            TrySendError::Full(item) => PushError::Full(item),
            TrySendError::Disconnected(item) => PushError::Closed(item),
        })
    }
}

/// The receiving side of a queue. Clones share the queue; it closes once
/// every inbox is dropped. An inbox no worker drains fills up.
pub struct Inbox<W> {
    items: Receiver<W>,
}

impl<W> Clone for Inbox<W> {
    fn clone(&self) -> Self {
        Inbox {
            items: self.items.clone(),
        }
    }
}

impl<W> Inbox<W> {
    /// Takes an item, if one is queued: for draining an inbox by hand.
    pub fn try_pop(&self) -> Option<W> {
        self.items.try_recv().ok()
    }
}

/// A thread to be, and the processors it will run.
pub struct Worker {
    name: String,
    recipes: Vec<Box<dyn Unbuilt>>,
}

impl Worker {
    pub fn new(name: impl Into<String>) -> Worker {
        Worker {
            name: name.into(),
            recipes: Vec::new(),
        }
    }

    /// Adds a processor draining `inbox`, built by `make` on the worker's
    /// thread, so that its state need not be `Send`, and sending what it
    /// emits to `sink`. The inbox must carry what the processor takes:
    ///
    /// ```compile_fail,E0271
    /// use workqueue::{Processor, Worker, queue};
    /// struct Takes;
    /// impl Processor for Takes {
    ///     type Input = String;
    ///     type Output = ();
    ///     fn process(&mut self, _: String) -> Option<()> {
    ///         None
    ///     }
    /// }
    /// let (_queue, numbers) = queue::<u32>(1);
    /// let _ = Worker::new("mismatched").run(numbers, || Takes, |_: ()| {});
    /// ```
    #[must_use]
    pub fn run<P: Processor + 'static>(
        mut self,
        inbox: Inbox<P::Input>,
        make: impl FnOnce() -> P + Send + 'static,
        sink: impl Sink<P::Output> + 'static,
    ) -> Worker {
        self.recipes.push(Box::new(Recipe {
            inbox: inbox.items,
            make,
            sink,
        }));
        self
    }

    /// Starts the thread. It waits on all its inboxes at once, takes an item
    /// from a ready one chosen at random, so that none starves, and runs one
    /// item at a time: a slow item delays the thread's other processors.
    pub fn spawn(self) -> WorkerHandle {
        let (stop, stopped) = crossbeam_channel::bounded(0);
        let recipes = self.recipes;
        let thread = std::thread::Builder::new()
            .name(self.name)
            .spawn(move || {
                let slots: Vec<Box<dyn Slot>> = recipes.into_iter().map(Unbuilt::build).collect();
                run(&slots, &stopped);
            })
            .expect("thread spawn");
        WorkerHandle {
            stop: Some(stop),
            thread: Some(thread),
        }
    }
}

/// A running worker. Dropping it stops the thread after the item in hand
/// and joins it; items still queued stay in their queues, and are dropped
/// (with whatever reply channel they carry) when the queues are.
pub struct WorkerHandle {
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A processor not yet built, and its inbox.
trait Unbuilt: Send {
    fn build(self: Box<Self>) -> Box<dyn Slot>;
}

struct Recipe<W, F, K> {
    inbox: Receiver<W>,
    make: F,
    sink: K,
}

impl<P, F, K> Unbuilt for Recipe<P::Input, F, K>
where
    P: Processor + 'static,
    F: FnOnce() -> P + Send,
    K: Sink<P::Output> + 'static,
{
    fn build(self: Box<Self>) -> Box<dyn Slot> {
        Box::new(Built {
            inbox: self.inbox,
            stage: RefCell::new(((self.make)(), self.sink)),
        })
    }
}

/// A processor, its inbox and its sink, on the worker's thread.
struct Built<P: Processor, K> {
    inbox: Receiver<P::Input>,
    // A cell, as the thread's `Select` borrows every inbox for as long as
    // the thread runs.
    stage: RefCell<(P, K)>,
}

enum Step {
    Ran,
    Empty,
    Closed,
}

trait Slot {
    fn register<'a>(&'a self, select: &mut Select<'a>) -> usize;
    fn step(&self) -> Step;
}

impl<P: Processor, K: Sink<P::Output>> Slot for Built<P, K> {
    fn register<'a>(&'a self, select: &mut Select<'a>) -> usize {
        select.recv(&self.inbox)
    }

    fn step(&self) -> Step {
        match self.inbox.try_recv() {
            Ok(item) => {
                let (processor, sink) = &mut *self.stage.borrow_mut();
                // A panicking item must not take the thread, and with it every
                // later item, down. Its reply sender is dropped, which its
                // sender sees.
                let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    if let Some(output) = processor.process(item) {
                        sink.send(output);
                    }
                }));
                Step::Ran
            }
            // Readiness can be spurious, or another worker took the item.
            Err(TryRecvError::Empty) => Step::Empty,
            Err(TryRecvError::Disconnected) => Step::Closed,
        }
    }
}

fn run(slots: &[Box<dyn Slot>], stop: &Receiver<()>) {
    let mut select = Select::new();
    let stop_index = select.recv(stop);
    let indices: Vec<usize> = slots.iter().map(|s| s.register(&mut select)).collect();
    loop {
        let ready = select.ready();
        // The handle never sends: ready means it dropped its sender. Checked
        // after every item too, as busy inboxes are chosen as often as it.
        if ready == stop_index || !matches!(stop.try_recv(), Err(TryRecvError::Empty)) {
            return;
        }
        let slot = indices
            .iter()
            .position(|&i| i == ready)
            .expect("a registered index");
        match slots[slot].step() {
            Step::Ran | Step::Empty => {}
            // No queue can send to it again; stop waiting on it.
            Step::Closed => select.remove(ready),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, mpsc};

    use super::*;

    fn thread_name() -> String {
        std::thread::current().name().unwrap().to_owned()
    }

    /// Ends a pipeline: emitted items are dropped.
    fn discard<T>(_: T) {}

    /// Counts its items and reports the thread it ran on.
    struct Echo {
        seen: usize,
    }

    type Echoed = (usize, String, usize);

    impl Processor for Echo {
        type Input = (usize, mpsc::Sender<Echoed>);
        type Output = ();

        fn process(&mut self, (n, reply): Self::Input) -> Option<()> {
            assert_ne!(n, 13, "unlucky");
            self.seen += 1;
            reply.send((n, thread_name(), self.seen)).unwrap();
            None
        }
    }

    /// Replies with its thread's name, for items of another type than
    /// `Echo`'s.
    struct Name;

    impl Processor for Name {
        type Input = mpsc::Sender<String>;
        type Output = ();

        fn process(&mut self, reply: mpsc::Sender<String>) -> Option<()> {
            reply.send(thread_name()).unwrap();
            None
        }
    }

    #[test]
    fn processors_share_a_thread() {
        let (echo, echo_inbox) = queue(16);
        let (name, name_inbox) = queue(16);
        let _worker = Worker::new("shared")
            .run(echo_inbox, || Echo { seen: 0 }, discard)
            .run(name_inbox, || Name, discard)
            .spawn();
        let (to_echo, from_echo) = mpsc::channel();
        let (to_name, from_name) = mpsc::channel();
        for n in 0..5 {
            echo.try_push((n, to_echo.clone())).unwrap();
            name.try_push(to_name.clone()).unwrap();
        }
        drop((to_echo, to_name));
        let echoed: Vec<_> = from_echo.iter().collect();
        let named: Vec<_> = from_name.iter().collect();
        assert_eq!((echoed.len(), named.len()), (5, 5));
        assert!(echoed.iter().all(|(_, thread, _)| thread == "shared"));
        assert!(named.iter().all(|thread| thread == "shared"));
    }

    /// A value on its way through a pipeline, refused if a queue is full.
    struct Job<T> {
        value: T,
        reply: mpsc::Sender<Result<String, Refusal>>,
    }

    impl<T> Refuse for Job<T> {
        fn refuse(self, why: Refusal) {
            self.reply.send(Err(why)).unwrap();
        }
    }

    /// The first stage: numbers in, text out.
    struct Spell;

    impl Processor for Spell {
        type Input = Job<u32>;
        type Output = Job<String>;

        fn process(&mut self, job: Job<u32>) -> Option<Job<String>> {
            Some(Job {
                value: format!("#{}", job.value),
                reply: job.reply,
            })
        }
    }

    /// The second stage: answers with the text and its thread.
    struct Answer;

    impl Processor for Answer {
        type Input = Job<String>;
        type Output = ();

        fn process(&mut self, job: Job<String>) -> Option<()> {
            job.reply
                .send(Ok(format!("{} on {}", job.value, thread_name())))
                .unwrap();
            None
        }
    }

    fn push(queue: &Queue<Job<u32>>, value: u32, reply: &mpsc::Sender<Result<String, Refusal>>) {
        let job = Job {
            value,
            reply: reply.clone(),
        };
        assert!(queue.try_push(job).is_ok());
    }

    #[test]
    fn one_stage_output_is_the_next_one_input() {
        let (first, first_inbox) = queue(16);
        let (second, second_inbox) = queue(16);
        let _worker = Worker::new("pipeline")
            .run(first_inbox, || Spell, second)
            .run(second_inbox, || Answer, discard)
            .spawn();
        let (reply, replies) = mpsc::channel();
        for value in 0..3 {
            push(&first, value, &reply);
        }
        drop(reply);
        let mut answers: Vec<_> = replies.iter().map(Result::unwrap).collect();
        answers.sort();
        assert_eq!(
            answers,
            ["#0 on pipeline", "#1 on pipeline", "#2 on pipeline"]
        );
    }

    #[test]
    fn a_full_next_queue_refuses_the_item() {
        // No capacity, and its only reader is this very thread: it never
        // takes an item.
        let (first, first_inbox) = queue(16);
        let (second, second_inbox) = queue(0);
        let _worker = Worker::new("refusing")
            .run(first_inbox, || Spell, second)
            .run(second_inbox, || Answer, discard)
            .spawn();
        let (reply, replies) = mpsc::channel();
        push(&first, 7, &reply);
        assert_eq!(replies.recv().unwrap(), Err(Refusal::Full));
    }

    #[test]
    fn an_inbox_drained_by_two_workers() {
        let (queue, inbox) = queue(64);
        let workers: Vec<_> = (0..2)
            .map(|i| {
                Worker::new(format!("echo-{i}"))
                    .run(inbox.clone(), || Echo { seen: 0 }, discard)
                    .spawn()
            })
            .collect();
        drop(inbox);
        let (tx, rx) = mpsc::channel();
        for n in 0..20 {
            queue.try_push((n, tx.clone())).unwrap();
        }
        drop(tx);
        let results: Vec<_> = rx.iter().collect();
        // 13 panicked, and the thread that ran it went on to other items.
        assert_eq!(results.len(), 19);
        // Each thread's count is its own: each counts 1, 2, … over the items
        // it ran, however they were spread.
        let mut counts: BTreeMap<&str, Vec<usize>> = BTreeMap::default();
        for (_, thread, seen) in &results {
            counts.entry(thread).or_default().push(*seen);
        }
        assert!(counts.keys().all(|t| t.starts_with("echo-")));
        for seen in counts.values_mut() {
            seen.sort_unstable();
            assert!(seen.iter().copied().eq(1..=seen.len()), "{seen:?}");
        }
        drop(workers);
        assert!(matches!(
            queue.try_push((0, mpsc::channel().0)),
            Err(PushError::Closed(_))
        ));
    }

    /// Blocks until told to go on.
    struct Gate(mpsc::Receiver<()>);

    impl Processor for Gate {
        type Input = ();
        type Output = ();

        fn process(&mut self, (): ()) -> Option<()> {
            let _ = self.0.recv();
            None
        }
    }

    #[test]
    fn a_full_queue_refuses() {
        let (open, gate) = mpsc::channel();
        let (queue, inbox) = queue(2);
        let worker = Worker::new("gate")
            .run(inbox, move || Gate(gate), discard)
            .spawn();
        // One in hand, two queued, then full.
        let mut pushed = 0;
        while queue.try_push(()).is_ok() {
            pushed += 1;
            assert!(pushed <= 3, "the queue never filled");
        }
        assert!(matches!(queue.try_push(()), Err(PushError::Full(()))));
        drop(open);
        drop(worker);
    }

    #[test]
    fn a_panic_spares_the_thread_and_its_other_processors() {
        let (echo, echo_inbox) = queue(16);
        let (name, name_inbox) = queue(16);
        let _worker = Worker::new("sturdy")
            .run(echo_inbox, || Echo { seen: 0 }, discard)
            .run(name_inbox, || Name, discard)
            .spawn();
        let (to_echo, from_echo) = mpsc::channel();
        let (to_name, from_name) = mpsc::channel();
        echo.try_push((13, to_echo.clone())).unwrap();
        echo.try_push((14, to_echo)).unwrap();
        name.try_push(to_name).unwrap();
        assert_eq!(from_echo.recv().unwrap().0, 14);
        assert_eq!(from_name.recv().unwrap(), "sturdy");
    }

    #[test]
    fn an_undrained_inbox_fills_and_its_queue_closes_with_it() {
        let (queue, inbox) = queue(1);
        queue.try_push(()).unwrap();
        assert!(matches!(queue.try_push(()), Err(PushError::Full(()))));
        assert_eq!(inbox.try_pop(), Some(()));
        drop(inbox);
        assert!(matches!(queue.try_push(()), Err(PushError::Closed(()))));
    }

    /// Records being dropped, which happens on its thread as the thread ends.
    struct Flag(Arc<AtomicBool>);

    impl Processor for Flag {
        type Input = ();
        type Output = ();

        fn process(&mut self, (): ()) -> Option<()> {
            None
        }
    }

    impl Drop for Flag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn dropping_the_handle_joins_the_thread() {
        let dropped = Arc::new(AtomicBool::new(false));
        let (_queue, inbox) = queue::<()>(1);
        let flag = Arc::new(Mutex::new(Some(Flag(dropped.clone()))));
        let worker = Worker::new("joined")
            .run(inbox, move || flag.lock().unwrap().take().unwrap(), discard)
            .spawn();
        drop(worker);
        assert!(dropped.load(Ordering::SeqCst));
    }
}
