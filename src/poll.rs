use {sys, Evented, Token};
use event::{self, Ready, Event, PollOpt};
use std::{fmt, io, ptr, usize};
use std::cell::UnsafeCell;
use std::{ops, isize};
use std::sync::{Arc, Mutex, Condvar};
use std::sync::atomic::{self, AtomicUsize, AtomicPtr, AtomicBool};
use std::sync::atomic::Ordering::{self, Acquire, Release, AcqRel, Relaxed};
use std::time::{Duration, Instant};

// Poll is backed by two readiness queues. The first is a system readiness queue
// represented by `sys::Selector`. The system readiness queue handles
// notifications provided by the system, such as TCP and UDP. The second
// readiness queue is implemented in user space by `ReadinessQueue`. It provides
// a way to implement purely userspace `Evented` types.
//
// `ReadinessQueue` is is backed by a MPSC queue that supports reuse of linked
// list nodes. This significantly reduces the number of required allocations.
// Each `Registration` / `SetReadiness` pair allocates a single readiness node
// that is used for the lifetime of the registration.
//
// The readiness node also includes a single atomic variable, `state` that
// tracks most of the state associated with the registration. This includes the
// current readiness, interest, poll options, and internal state. When the node
// state is mutated, it is queued in the MPSC channel. A call to
// `ReadinessQueue::poll` will dequeue and process nodes. The node state can
// still be mutated while it is queued in the channel for processing.
// Intermediate state values do not matter as long as the final state is
// included in the call to `poll`. This is the eventually consistent nature of
// the readiness queue.
//
// The MPSC queue is a modified version of the intrusive MPSC node based queue
// described by 1024cores [1].
//
// The first modification is that two markers are used instead of a single
// `stub`. The second marker is a `sleep_marker` which is used to signal to
// producers that the consumer is going to sleep. This sleep_marker is only used
// when the queue is empty, implying that the only node in the queue is
// `end_marker`.
//
// The second modification is an `until` argument passed to the dequeue
// function. When `poll` encounters a level-triggered node, the node will be
// immediately pushed back into the queue. In order to avoid an infinite loop,
// `poll` before pushing the node, the pointer is saved off and then passed
// again as the `until` argument. If the next node to pop is `until`, then
// `Dequeue::Empty` is returned.
//
// [1] http://www.1024cores.net/home/lock-free-algorithms/queues/intrusive-mpsc-node-based-queue


/// Polls for readiness events on all registered values.
///
/// The `Poll` type acts as an interface allowing a program to wait on a set of
/// IO handles until one or more become "ready" to be operated on. An IO handle
/// is considered ready to operate on when the given operation can complete
/// without blocking.
///
/// To use `Poll`, an IO handle must first be registered with the `Poll`
/// instance using the `register()` handle. An `Ready` representing the
/// program's interest in the socket is specified as well as an arbitrary
/// `Token` which is used to identify the IO handle in the future.
///
/// ## Edge-triggered and level-triggered
///
/// An IO handle registration may request edge-triggered notifications or
/// level-triggered notifications. This is done by specifying the `PollOpt`
/// argument to `register()` and `reregister()`.
///
/// ## Portability
///
/// Cross platform portability is provided for Mio's TCP & UDP implementations.
///
/// ## Examples
///
/// ```no_run
/// use mio::*;
/// use mio::tcp::*;
///
/// // Construct a new `Poll` handle as well as the `Events` we'll store into
/// let poll = Poll::new().unwrap();
/// let mut events = Events::with_capacity(1024);
///
/// // Connect the stream
/// let stream = TcpStream::connect(&"173.194.33.80:80".parse().unwrap()).unwrap();
///
/// // Register the stream with `Poll`
/// poll.register(&stream, Token(0), Ready::all(), PollOpt::edge()).unwrap();
///
/// // Wait for the socket to become ready
/// poll.poll(&mut events, None).unwrap();
/// ```
pub struct Poll {
    // Platform specific IO selector
    selector: sys::Selector,

    // Custom readiness queue
    readiness_queue: ReadinessQueue,

    // Use an atomic to first check if a full lock will be required. This is a
    // fast-path check for single threaded cases avoiding the extra syscall
    lock_state: AtomicUsize,

    // Sequences concurrent calls to `Poll::poll`
    lock: Mutex<()>,

    // Wakeup the next waiter
    condvar: Condvar,
}

/// Handle to a Poll registration. Used for registering custom types for event
/// notifications.
pub struct Registration {
    inner: RegistrationInner,
}

unsafe impl Send for Registration {}
unsafe impl Sync for Registration {}

/// Used to update readiness for an associated `Registration`. `SetReadiness`
/// is `Sync` which allows it to be updated across threads.
#[derive(Clone)]
pub struct SetReadiness {
    inner: RegistrationInner,
}

unsafe impl Send for SetReadiness {}
unsafe impl Sync for SetReadiness {}

struct RegistrationInner {
    // ARC pointer to the Poll's readiness queue
    queue: ReadinessQueue,

    // Unsafe pointer to the registration's node. The node is ref counted. This
    // cannot "simply" be tracked by an Arc because `Poll::poll` has an implicit
    // handle though it isn't stored anywhere. In other words, `Poll::poll`
    // needs to decrement the ref count before the node is freed.
    node: *mut ReadinessNode,
}

#[derive(Clone)]
struct ReadinessQueue {
    inner: Arc<UnsafeCell<ReadinessQueueInner>>,
}

unsafe impl Send for ReadinessQueue {}
unsafe impl Sync for ReadinessQueue {}

struct ReadinessQueueInner {
    // Used to wake up `Poll` when readiness is set in another thread.
    awakener: sys::Awakener,

    // Head of the MPSC queue used to signal readiness to `Poll::poll`.
    head_readiness: AtomicPtr<ReadinessNode>,

    // Tail of the readiness queue.
    //
    // Only accessed by Poll::poll. Coordination will be handled by the poll fn
    tail_readiness: UnsafeCell<*mut ReadinessNode>,

    // Fake readiness node used to punctuate the end of the readiness queue.
    // Before attempting to read from the queue, this node is inserted in order
    // to partition the queue between nodes that are "owned" by the dequeue end
    // and nodes that will be pushed on by producers.
    end_marker: Box<ReadinessNode>,

    // Similar to `end_marker`, but this node signals to producers that `Poll`
    // has gone to sleep and must be woken up.
    sleep_marker: Box<ReadinessNode>,
}

/// Node shared by a `Registration` / `SetReadiness` pair as well as the node
/// queued into the MPSC channel.
struct ReadinessNode {
    // Node state, see struct docs for `ReadinessState`
    //
    // This variable is the primary point of coordination between all the
    // various threads concurrently accessing the node.
    state: AtomicState,

    // The registration token cannot fit into the `state` variable, so it is
    // broken out here. In order to atomically update both the state and token
    // we have to jump through a few hoops.
    //
    // First, `state` includes `token_read_pos` and `token_write_pos`. These can
    // either be 0, 1, or 2 which represent a token slot. `token_write_pos` is
    // the token slot that contains the most up to date registration token.
    // `token_read_pos` is the token slot that `poll` is currently reading from.
    //
    // When a call to `update` includes a different token than the one currently
    // associated with the registration (token_write_pos), first an unused token
    // slot is found. The unused slot is the one not represented by
    // `token_read_pos` OR `token_write_pos`. The new token is written to this
    // slot, then `state` is updated with the new `token_write_pos` value. This
    // requires that there is only a *single* concurrent call to `update`.
    //
    // When `poll` reads a node state, it checks that `token_read_pos` matches
    // `token_write_pos`. If they do not match, then it atomically updates
    // `state` such that `token_read_pos` is set to `token_write_pos`. It will
    // then read the token at the newly updated `token_read_pos`.
    token_0: UnsafeCell<Token>,
    token_1: UnsafeCell<Token>,
    token_2: UnsafeCell<Token>,

    // Used when the node is queued in the readiness linked list. Accessing
    // this field requires winning the "queue" lock
    next_readiness: AtomicPtr<ReadinessNode>,

    // Ensures that there is only one concurrent call to `update`.
    //
    // Each call to `update` will attempt to swap `update_lock` from `false` to
    // `true`. If the CAS succeeds, the thread has obtained the update lock. If
    // the CAS fails, then the `update` call returns immediately and the update
    // is discarded.
    update_lock: AtomicBool,

    // Number of outstanding Registration nodes
    num_registration: AtomicUsize,

    // Tracks the number of `ReadyRef` pointers
    ref_count: AtomicUsize,
}

/// Stores the ReadinessNode state in an AtomicUsize. This wrapper around the
/// atomic variable handles encoding / decoding `ReadinessState` values.
struct AtomicState {
    inner: AtomicUsize,
}

const MASK_2: usize = 4 - 1;
const MASK_4: usize = 16 - 1;
const QUEUED_MASK: usize = 1 << QUEUED_SHIFT;
const DROPPED_MASK: usize = 1 << DROPPED_SHIFT;

const READINESS_SHIFT: usize = 0;
const INTEREST_SHIFT: usize = 4;
const POLL_OPT_SHIFT: usize = 8;
const TOKEN_RD_SHIFT: usize = 12;
const TOKEN_WR_SHIFT: usize = 14;
const QUEUED_SHIFT: usize = 16;
const DROPPED_SHIFT: usize = 17;

/// Tracks all state for a single `ReadinessNode`. The state is packed into a
/// `usize` variable from low to high bit as follows:
///
/// 4 bits: Registration current readiness
/// 4 bits: Registration interest
/// 4 bits: Poll options
/// 2 bits: Token position currently being read from by `poll`
/// 2 bits: Token position last written to by `update`
/// 1 bit:  Queued flag, set when node is being pushed into MPSC queue.
/// 1 bit:  Dropped flag, set when all `Registration` handles have been dropped.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
struct ReadinessState(usize);

/// Returned by `dequeue_node`. Represents the different states as described by
/// the queue documentation on 1024cores.net.
enum Dequeue {
    Data(*mut ReadinessNode),
    Empty,
    Inconsistent,
}

const AWAKEN: Token = Token(usize::MAX);
const MAX_REFCOUNT: usize = (isize::MAX) as usize;

/*
 *
 * ===== Poll =====
 *
 */

impl Poll {
    /// Return a new `Poll` handle using a default configuration.
    pub fn new() -> io::Result<Poll> {
        is_send::<Poll>();
        is_sync::<Poll>();

        let poll = Poll {
            selector: try!(sys::Selector::new()),
            readiness_queue: try!(ReadinessQueue::new()),
            lock_state: AtomicUsize::new(0),
            lock: Mutex::new(()),
            condvar: Condvar::new(),
        };

        // Register the notification wakeup FD with the IO poller
        try!(poll.readiness_queue.inner().awakener.register(&poll, AWAKEN, Ready::readable(), PollOpt::edge()));

        Ok(poll)
    }

    /// Register an `Evented` handle with the `Poll` instance.
    pub fn register<E: ?Sized>(&self, io: &E, token: Token, interest: Ready, opts: PollOpt) -> io::Result<()>
        where E: Evented
    {
        try!(validate_args(token, interest));

        /*
         * Undefined behavior:
         * - Reusing a token with a different `Evented` without deregistering
         * (or closing) the original `Evented`.
         */
        trace!("registering with poller");

        // Register interests for this socket
        try!(io.register(self, token, interest, opts));

        Ok(())
    }

    /// Re-register an `Evented` handle with the `Poll` instance.
    pub fn reregister<E: ?Sized>(&self, io: &E, token: Token, interest: Ready, opts: PollOpt) -> io::Result<()>
        where E: Evented
    {
        try!(validate_args(token, interest));

        trace!("registering with poller");

        // Register interests for this socket
        try!(io.reregister(self, token, interest, opts));

        Ok(())
    }

    /// Deregister an `Evented` handle with the `Poll` instance.
    pub fn deregister<E: ?Sized>(&self, io: &E) -> io::Result<()>
        where E: Evented
    {
        trace!("deregistering IO with poller");

        // Deregister interests for this socket
        try!(io.deregister(self));

        Ok(())
    }

    /// Block the current thread and wait until any `Evented` values registered
    /// with the `Poll` instance are ready or the given timeout has elapsed.
    pub fn poll(&self, events: &mut Events, mut timeout: Option<Duration>) -> io::Result<usize> {
        let zero = Some(Duration::from_millis(0));


        // At a high level, the synchronization strategy is to acquire access to
        // the critical section by transitioning the atomic from unlocked ->
        // locked. If the attempt fails, the thread will wait on the condition
        // variable.
        //
        // # Some more detail
        //
        // The `lock_state` atomic usize combines:
        //
        // - locked flag, stored in the least significant bit
        // - number of waiting threads, stored in the rest of the bits.
        //
        // When a thread transitions the locked flag from 0 -> 1, it has
        // obtained access to the critical section.
        //
        // When entering `poll`, a compare-and-swap from 0 -> 1 is attempted.
        // This is a fast path for the case when there are no concurrent calls
        // to poll, which is very common.
        //
        // On failure, the mutex is locked, and the thread attempts to increment
        // the number of waiting threads component of `lock_state`. If this is
        // successfully done while the locked flag is set, then the thread can
        // wait on the condition variable.
        //
        // When a thread exits the critical section, it unsets the locked flag.
        // If there are any waiters, which is atomically determined while
        // unsetting the locked flag, then the condvar is notified.

        let mut curr = self.lock_state.compare_and_swap(0, 1, Acquire);

        if 0 != curr {
            // Enter slower path
            let mut lock = self.lock.lock().unwrap();
            let mut inc = false;

            loop {
                if curr & 1 == 0 {
                    // The lock is currently free, attempt to grab it
                    let mut next = curr | 1;

                    if inc {
                        // The waiter count has previously been incremented, so
                        // decrement it here
                        next -= 2;
                    }

                    let actual = self.lock_state.compare_and_swap(curr, next, Acquire);

                    if actual != curr {
                        curr = actual;
                        continue;
                    }

                    // Lock acquired, break from the loop
                    break;
                } else {
                    if timeout == zero {
                        assert!(!inc); // TODO: is this true?
                        return Ok(0);
                    }

                    // The lock is currently held, so wait for it to become
                    // free. If the waiter count hasn't been incremented yet, do
                    // so now
                    if !inc {
                        // Prevent overflow
                        if curr | 1 == usize::MAX {
                            panic!();
                        }

                        let next = curr + 2;
                        let actual = self.lock_state.compare_and_swap(curr, next, Acquire);

                        if actual != curr {
                            curr = actual;
                            continue;
                        }

                        // Track that the waiter count has been incremented for
                        // this thread and fall through to the condvar waiting
                        inc = true;
                    }
                }

                lock = match timeout {
                    Some(to) => {
                        let now = Instant::now();

                        // Wait to be notified
                        let (l, result) = self.condvar.wait_timeout(lock, to).unwrap();

                        // Wait timed out, stop trying to wait
                        if result.timed_out() {
                            debug_assert!(inc);
                            self.lock_state.fetch_sub(2, Relaxed);
                            return Ok(0);
                        }

                        // See how much time was elapsed in the wait
                        let elapsed = now.elapsed();

                        // If the timeout was elapsed, then stop trying to wait
                        if elapsed >= to {
                            debug_assert!(inc);
                            self.lock_state.fetch_sub(2, Relaxed);
                            return Ok(0);
                        }

                        // Update the timeout
                        timeout = Some(to - elapsed);
                        l
                    }
                    _ => self.condvar.wait(lock).unwrap(),
                };

                // Try to lock again...
            }
        }

        let ret = self.poll2(events, timeout);

        // Release the lock
        if 1 != self.lock_state.fetch_and(!1, Release) {
            // There is at least one waiting thread, so notify one
            self.condvar.notify_one();
        }

        ret
    }

    #[inline]
    fn poll2(&self, events: &mut Events, timeout: Option<Duration>) -> io::Result<usize> {
        let mut sleep = false;

        // Compute the timeout value passed to the system selector. If the
        // readiness queue has pending nodes, we still want to poll the system
        // selector for new events, but we don't want to block the thread to
        // wait for new events.
        let timeout = if timeout == Some(Duration::from_millis(0)) {
            // If blocking is not requested, then there is no need to prepare
            // the queue for sleep
            timeout
        } else if self.readiness_queue.prepare_for_sleep() {
            // The readiness queue is empty. The call to `prepare_for_sleep`
            // inserts `sleep_marker` into the queue. This signals to any
            // threads setting readiness that the `Poll::poll` is going to
            // sleep, so the awakener should be used.
            sleep = true;
            timeout
        } else {
            // The readiness queue is not empty, so do not block the thread.
            Some(Duration::from_millis(0))
        };

        // First get selector events
        let res = self.selector.select(&mut events.inner, AWAKEN, timeout);

        if sleep {
            // Cleanup the sleep marker. Removing `sleep_marker` avoids
            // unnecessary syscalls to the awakener. It also needs to be removed
            // from the queue before it can be inserted again.
            //
            // Note, that this won't *guarantee* that the sleep marker is
            // removed. If the sleep marker cannot be removed, it is no longer
            // at the head of the queue, which still achieves the goal of
            // avoiding extra awakener syscalls.
            self.readiness_queue.try_remove_sleep_marker();
        }

        if try!(res) {
            // Some awakeners require reading from a FD.
            self.readiness_queue.inner().awakener.cleanup();
        }

        // Poll custom event queue
        self.readiness_queue.poll(&mut events.inner);

        // Return number of polled events
        Ok(events.len())
    }
}

fn validate_args(token: Token, interest: Ready) -> io::Result<()> {
    if token == AWAKEN {
        return Err(io::Error::new(io::ErrorKind::Other, "invalid token"));
    }

    if !interest.is_readable() && !interest.is_writable() {
        return Err(io::Error::new(io::ErrorKind::Other, "interest must include readable or writable"));
    }

    Ok(())
}

impl fmt::Debug for Poll {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "Poll")
    }
}

/// A buffer for I/O events to get placed into, passed to `Poll::poll`.
///
/// This structure is normally re-used on each turn of the event loop and will
/// contain any I/O events that happen during a `poll`. After a call to `poll`
/// returns the various accessor methods on this structure can be used to
/// iterate over the underlying events that ocurred.
pub struct Events {
    inner: sys::Events,
}

/// Iterate an Events structure
pub struct EventsIter<'a> {
    inner: &'a Events,
    pos: usize,
}

impl Events {
    /// Create a net blank set of events capable of holding up to `capacity`
    /// events.
    ///
    /// This parameter typically is an indicator on how many events can be
    /// returned each turn of the event loop, but it is not necessarily a hard
    /// limit across platforms.
    pub fn with_capacity(capacity: usize) -> Events {
        Events {
            inner: sys::Events::with_capacity(capacity),
        }
    }

    /// Returns the `idx`-th event.
    ///
    /// Returns `None` if `idx` is greater than the length of this event buffer.
    pub fn get(&self, idx: usize) -> Option<Event> {
        self.inner.get(idx)
    }

    /// Returns how many events this buffer contains.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns whether this buffer contains 0 events.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn iter(&self) -> EventsIter {
        EventsIter {
            inner: self,
            pos: 0
        }
    }
}

impl<'a> IntoIterator for &'a Events {
    type Item = Event;
    type IntoIter = EventsIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a> Iterator for EventsIter<'a> {
    type Item = Event;

    fn next(&mut self) -> Option<Event> {
        let ret = self.inner.get(self.pos);
        self.pos += 1;
        ret
    }
}

// ===== Accessors for internal usage =====

pub fn selector(poll: &Poll) -> &sys::Selector {
    &poll.selector
}

/*
 *
 * ===== Registration =====
 *
 */

impl Registration {
    /// Create a new `Registration` associated with the given `Poll` instance.
    ///
    /// The returned `Registration` will be associated with this `Poll` for its
    /// entire lifetime. Dropping the `Registration` will prevent any further
    /// notifications to be polled.
    pub fn new(poll: &Poll, token: Token, interest: Ready, opt: PollOpt) -> (Registration, SetReadiness) {
        is_send::<Registration>();
        is_sync::<Registration>();
        is_send::<SetReadiness>();
        is_sync::<SetReadiness>();

        // Clone handle to the readiness queue, this bumps the ref count
        let queue = poll.readiness_queue.clone();

        // Allocate the registration node. The new node will have `ref_count`
        // set to 3, and `num_registration` set to
        // 1. The 3 ref_counts represent ownership by one SetReadiness, one
        // Registration, and the Poll handle.
        let node = Box::into_raw(Box::new(ReadinessNode::new(token, interest, opt)));

        let registration = Registration {
            inner: RegistrationInner {
                node: node,
                queue: queue.clone(),
            },
        };

        let set_readiness = SetReadiness {
            inner: RegistrationInner {
                node: node,
                queue: queue.clone(),
            },
        };

        (registration, set_readiness)
    }

    /// Update the registration details
    ///
    /// # Note
    ///
    /// `update` does not guarantee to establish any memory ordering. Any
    /// concurrent data access must be synchronized using another strategy.
    pub fn update(&self, poll: &Poll, token: Token, interest: Ready, opts: PollOpt) -> io::Result<()> {
        self.inner.update(poll, token, interest, opts)
    }

    /// Disable the registration.
    ///
    /// No further notifcations for this registration will be polled until the
    /// registration details are updated with `update`.
    ///
    /// # Note
    ///
    /// `deregister` does not guarantee to establish any memory ordering. Any
    /// concurrent data access must be synchronized using another strategy.
    pub fn deregister(&self, poll: &Poll) -> io::Result<()> {
        self.inner.update(poll, Token(0), Ready::none(), PollOpt::empty())
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let n = self.inner.num_registration.fetch_sub(1, AcqRel);

        if n == 1 {
            // This is the last `Registration` handle, the registration should
            // be released.
            //
            // `flag_as_dropped` toggled the `dropped` flag and notifies
            // `Poll::poll` to release its handle (which is just decrementing
            // the ref count).
            self.inner.flag_as_dropped();
        }
    }
}

impl fmt::Debug for Registration {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("Registration")
            .finish()
    }
}

impl SetReadiness {
    /// Returns the registration's current readiness.
    ///
    /// # Note
    ///
    /// `readiness` does not guarantee to establish any memory ordering. Any
    /// concurrent data access must be synchronized using another strategy.
    pub fn readiness(&self) -> Ready {
        self.inner.readiness()
    }

    /// Update the registration's readiness
    ///
    /// # Note
    ///
    /// `set_readiness` does not guarantee to establish any memory ordering. Any
    /// concurrent data access must be synchronized using another strategy.
    pub fn set_readiness(&self, ready: Ready) -> io::Result<()> {
        self.inner.set_readiness(ready)
    }
}

impl RegistrationInner {
    /// Get the registration's readiness.
    fn readiness(&self) -> Ready {
        self.state.load(Relaxed).readiness()
    }

    /// Set the registration's readiness.
    ///
    /// This function can be called concurrently by an arbitrary number of
    /// SetReadiness handles.
    fn set_readiness(&self, ready: Ready) -> io::Result<()> {
        // Load the current atomic state.
        let mut state = self.state.load(Acquire);
        let mut next;

        loop {
            next = state;

            if state.is_dropped() {
                // Node is dropped, no more notifications
                return Ok(());
            }

            // Update the readiness
            next.set_readiness(ready);

            // If the readiness is not blank, try to obtain permission to
            // push the node into the readiness queue.
            if next.effective_readiness().is_some() {
                next.set_queued();
            }

            let actual = self.state.compare_and_swap(state, next, AcqRel);

            if state == actual {
                break;
            }

            state = actual;
        }

        if !state.is_queued() && next.is_queued() {
            // We toggled the queued flag, making us responsible for queuing the
            // node in the MPSC readiness queue.
            try!(self.queue.enqueue_node_with_wakeup(self));
        }

        Ok(())
    }

    /// Update the registration details associated with the node
    fn update(&self, poll: &Poll, token: Token, interest: Ready, opt: PollOpt) -> io::Result<()> {
        // Ensure poll instances match
        if !self.queue.identical(&poll.readiness_queue) {
            return Err(io::Error::new(io::ErrorKind::Other, "registration registered with another instance of Poll"));
        }

        // The `update_lock` atomic is used as a flag ensuring only a single
        // thread concurrently enters the `update` critical section. Any
        // concurrent calls to update are discarded. If coordinated updates are
        // required, the Mio user is responsible for handling that.
        //
        // Acquire / Release ordering is used on `update_lock` to ensure that
        // data access to the `token_*` variables are scoped to the critical
        // section.

        // Acquire the update lock.
        if self.update_lock.compare_and_swap(false, true, Acquire) {
            // The lock is already held. Discard the update
            return Ok(());
        }

        // Relaxed ordering is acceptable here as the only memory that needs to
        // be visible as part of the update are the `token_*` variables, and
        // ordering has already been handled by the `update_lock` access.
        let mut state = self.state.load(Relaxed);
        let mut next;

        // Read the current token, again this memory has been ordered by the
        // acquire on `update_lock`.
        let curr_token_pos = state.token_write_pos();
        let curr_token = unsafe { self::token(self, curr_token_pos) };

        let mut next_token_pos = curr_token_pos;

        // If the `update` call is changing the token, then compute the next
        // available token slot and write the token there.
        //
        // Note that this computation is happening *outside* of the
        // compare-and-swap loop. The update lock ensures that only a single
        // thread could be mutating the write_token_position, so the
        // `next_token_pos` will never need to be recomputed even if
        // `token_read_pos` concurrently changes. This is because
        // `token_read_pos` can ONLY concurrently change to the current value of
        // `token_write_pos`, so `next_token_pos` will always remain valid.
        if token != curr_token {
            next_token_pos = state.next_token_pos();

            // Update the token
            match next_token_pos {
                0 => unsafe { *self.token_0.get() = token },
                1 => unsafe { *self.token_1.get() = token },
                2 => unsafe { *self.token_2.get() = token },
                _ => unreachable!(),
            }
        }

        // Now enter the compare-and-swap loop
        loop {
            next = state;

            // The node is only dropped once all `Registration` handles are
            // dropped. Only `Registration` can call `update`.
            debug_assert!(!state.is_dropped());

            // Update the write token position, this will also release the token
            // to Poll::poll.
            if curr_token != token {
                next.set_token_write_pos(next_token_pos);
            }

            // Update readiness and poll opts
            next.set_interest(interest);
            next.set_poll_opt(opt);

            // If there is effective readiness, the node will need to be queued
            // for processing. This exact behavior is still TBD, so we are
            // conservative for now and always fire.
            //
            // See https://github.com/carllerche/mio/issues/535.
            if next.effective_readiness().is_some() {
                next.set_queued();
            }

            // compare-and-swap the state values. Only `Release` is needed here.
            // The `Release` ensures that `Poll::poll` will see the token
            // update and the update function doesn't care about any other
            // memory visibility.
            let actual = self.state.compare_and_swap(state, next, Release);

            if actual == state {
                break;
            }

            // CAS failed, but `curr_token_pos` should not have changed given
            // that we still hold the update lock.
            debug_assert_eq!(curr_token_pos, actual.token_write_pos());

            state = actual;
        }

        // Release the lock
        self.update_lock.store(false, Release);

        if !state.is_queued() && next.is_queued() {
            // We are responsible for enqueing the node.
            try!(self.queue.enqueue_node_with_wakeup(self));
        }

        Ok(())
    }

    /// Set the node's dropped flag, informing `Poll::poll` that it should
    /// decrement its ref count.
    fn flag_as_dropped(&self) {
        self.state.flag_as_dropped();
    }
}

impl ops::Deref for RegistrationInner {
    type Target = ReadinessNode;

    fn deref(&self) -> &ReadinessNode {
        unsafe { &*self.node }
    }
}

impl Clone for RegistrationInner {
    fn clone(&self) -> RegistrationInner {
        // Using a relaxed ordering is alright here, as knowledge of the
        // original reference prevents other threads from erroneously deleting
        // the object.
        //
        // As explained in the [Boost documentation][1], Increasing the
        // reference counter can always be done with memory_order_relaxed: New
        // references to an object can only be formed from an existing
        // reference, and passing an existing reference from one thread to
        // another must already provide any required synchronization.
        //
        // [1]: (www.boost.org/doc/libs/1_55_0/doc/html/atomic/usage_examples.html)
        let old_size = self.ref_count.fetch_add(1, Relaxed);

        // However we need to guard against massive refcounts in case someone
        // is `mem::forget`ing Arcs. If we don't do this the count can overflow
        // and users will use-after free. We racily saturate to `isize::MAX` on
        // the assumption that there aren't ~2 billion threads incrementing
        // the reference count at once. This branch will never be taken in
        // any realistic program.
        //
        // We abort because such a program is incredibly degenerate, and we
        // don't care to support it.
        if old_size & !MAX_REFCOUNT != 0 {
            // TODO: This should really abort the process
            panic!();
        }

        RegistrationInner {
            queue: self.queue.clone(),
            node: self.node.clone(),
        }
    }
}

impl Drop for RegistrationInner {
    fn drop(&mut self) {
        // Only handles releasing from `Registration` and `SetReadiness`
        // handles. Poll has to call this itself.
        release_node(self.node);
    }
}

/*
 *
 * ===== ReadinessQueue =====
 *
 */

impl ReadinessQueue {
    /// Create a new `ReadinessQueue`.
    fn new() -> io::Result<ReadinessQueue> {
        is_send::<Self>();
        is_sync::<Self>();

        let end_marker = Box::new(ReadinessNode::marker());
        let sleep_marker = Box::new(ReadinessNode::marker());

        let ptr = &*end_marker as *const _ as *mut _;

        Ok(ReadinessQueue {
            inner: Arc::new(UnsafeCell::new(ReadinessQueueInner {
                awakener: try!(sys::Awakener::new()),
                head_readiness: AtomicPtr::new(ptr),
                tail_readiness: UnsafeCell::new(ptr),
                end_marker: end_marker,
                sleep_marker: sleep_marker,
            }))
        })
    }

    /// Poll the queue for new events
    fn poll(&self, dst: &mut sys::Events) {
        // `until` is set with the first node that gets re-enqueued due to being
        // set to have level-triggered notifications. This prevents an infinite
        // loop where `Poll::poll` will keep dequeuing nodes it enqueues.
        let mut until = ptr::null_mut();

        'outer:
        while dst.len() < dst.capacity() {
            // Dequeue a node. If the queue is in an inconsistent state, then
            // stop polling. `Poll::poll` will be called again shortly and enter
            // a syscall, which should be enough to enable the other thread to
            // finish the queuing process.
            let ptr = match self.dequeue_node(until) {
                Dequeue::Empty | Dequeue::Inconsistent => break,
                Dequeue::Data(ptr) => ptr,
            };

            let node = unsafe { &*ptr };

            // Read the node state with Acquire ordering. This allows reading
            // the token variables.
            let mut state = node.state.load(Acquire);
            let mut next;
            let mut readiness;
            let mut opt;

            loop {
                // Build up any changes to the readiness node's state and
                // attempt the CAS at the end
                next = state;

                // Given that the node was just read from the queue, the
                // `queued` flag should still be set.
                debug_assert!(state.is_queued());

                // The dropped flag means we need to release the node and
                // perform no further processing on it.
                if state.is_dropped() {
                    // Release the node and continue
                    release_node(ptr);
                    continue 'outer;
                }

                // Process the node
                readiness = state.effective_readiness();
                opt = state.poll_opt();

                if opt.is_edge() {
                    // Mark the node as dequeued
                    next.set_dequeued();

                    if opt.is_oneshot() && readiness.is_some() {
                        next.disarm();
                    }
                } else if readiness.is_none() {
                    // The event needs to be queued up again. This typically
                    // happens when using level-triggered notifications.
                    next.set_dequeued();
                }

                // Ensure `token_read_pos` is set to `token_write_pos` so that
                // we read the most up to date token value.
                next.update_token_read_pos();

                if state == next {
                    break;
                }

                let actual = node.state.compare_and_swap(state, next, AcqRel);

                if actual == state {
                    break;
                }

                state = actual;
            }

            // If the queued flag is still set, then the node must be requeued
            if next.is_queued() {
                if until.is_null() {
                    // We never want to see the node again
                    until = ptr;
                }

                // Requeue the node
                self.enqueue_node(node);
            }

            if readiness.is_some() {
                // Get the token
                let token = unsafe { token(node, next.token_read_pos()) };

                // Push the event
                dst.push_event(Event::new(readiness, token));
            }
        }
    }

    fn wakeup(&self) -> io::Result<()> {
        self.inner().awakener.wakeup()
    }

    /// Prepend the given node to the head of the readiness queue. This is done
    /// with relaxed ordering. Returns true if `Poll` needs to be woken up.
    fn enqueue_node_with_wakeup(&self, node: &ReadinessNode) -> io::Result<()> {
        if self.enqueue_node(node) {
            try!(self.wakeup());
        }

        Ok(())
    }

    fn enqueue_node(&self, node: &ReadinessNode) -> bool {
        let inner = self.inner();
        let node_ptr = node as *const _ as *mut _;

        // Relaxed used as the ordering is "released" when swapping
        // `head_readiness`
        node.next_readiness.store(ptr::null_mut(), Relaxed);

        unsafe {
            let prev = inner.head_readiness.swap(node_ptr, AcqRel);

            debug_assert!((*prev).next_readiness.load(Relaxed).is_null());

            (*prev).next_readiness.store(node_ptr, Release);

            prev == self.sleep_marker()
        }
    }

    /// Must only be called in `poll`
    fn dequeue_node(&self, until: *mut ReadinessNode) -> Dequeue {
        unsafe {
            let mut tail = *self.inner().tail_readiness.get();
            let mut next = (*tail).next_readiness.load(Acquire);

            if tail == self.end_marker() || tail == self.sleep_marker() {
                if next.is_null() {
                    return Dequeue::Empty;
                }

                *self.inner().tail_readiness.get() = next;
                tail = next;
                next = (*next).next_readiness.load(Acquire);
            }

            // Only need to check `until` at this point. `until` is either null,
            // which will never match tail OR it is a node that was pushed by
            // the current thread. This means that either:
            //
            // 1) The queue is inconsistent, which is handled explicitly
            // 2) We encounter `until` at this point in dequeue
            // 3) we will pop a different node
            if tail == until {
                return Dequeue::Empty;
            }

            if !next.is_null() {
                *self.inner().tail_readiness.get() = next;
                return Dequeue::Data(tail);
            }

            if self.inner().head_readiness.load(Acquire) != tail {
                return Dequeue::Inconsistent;
            }

            // Push the stub node
            self.enqueue_node(&*self.inner().end_marker);

            next = (*tail).next_readiness.load(Acquire);

            if !next.is_null() {
                *self.inner().tail_readiness.get() = next;
                return Dequeue::Data(tail);
            }

            Dequeue::Inconsistent
        }
    }

    /// Prepare the queue for the `Poll::poll` thread to block in the system
    /// selector. This involves changing `head_readiness` to `sleep_marker`.
    /// Returns true if successfull and `poll` can block.
    fn prepare_for_sleep(&self) -> bool {
        let end_marker = self.end_marker();
        let sleep_marker = self.sleep_marker();

        self.inner().sleep_marker.next_readiness.store(ptr::null_mut(), Relaxed);

        let actual = self.inner().head_readiness.compare_and_swap(
            end_marker, sleep_marker, AcqRel);

        debug_assert!(actual != sleep_marker);

        if actual != end_marker {
            // The readiness queue is not empty
            return false;
        }

        // The current tail should be pointing to `end_marker`
        debug_assert!(unsafe { *self.inner().tail_readiness.get() == end_marker });
        // The `end_marker` next pointer should be null
        debug_assert!(self.inner().end_marker.next_readiness.load(Relaxed).is_null());

        // Update tail pointer.
        unsafe { *self.inner().tail_readiness.get() = sleep_marker; }
        true
    }

    fn try_remove_sleep_marker(&self) {
        let end_marker = self.end_marker();
        let sleep_marker = self.sleep_marker();

        // Set the next ptr to null
        self.inner().end_marker.next_readiness.store(ptr::null_mut(), Relaxed);

        let actual = self.inner().head_readiness.compare_and_swap(
            sleep_marker, end_marker, AcqRel);

        // If the swap is successful, then the queue is still empty.
        if actual != sleep_marker {
            return;
        }

        unsafe { *self.inner().tail_readiness.get() = end_marker; }
    }

    fn end_marker(&self) -> *mut ReadinessNode {
        &*self.inner().end_marker as *const ReadinessNode as *mut ReadinessNode
    }

    fn sleep_marker(&self) -> *mut ReadinessNode {
        &*self.inner().sleep_marker as *const ReadinessNode as *mut ReadinessNode
    }

    fn identical(&self, other: &ReadinessQueue) -> bool {
        self.inner.get() == other.inner.get()
    }

    fn inner(&self) -> &ReadinessQueueInner {
        unsafe { &*self.inner.get() }
    }
}

impl ReadinessNode {
    /// Return a new `ReadinessNode`, initialized with a ref_count of 3.
    fn new(token: Token, interest: Ready, opt: PollOpt) -> ReadinessNode {
        ReadinessNode {
            state: AtomicState::new(interest, opt),
            // Only the first token is set, the others are initialized to 0
            token_0: UnsafeCell::new(token),
            token_1: UnsafeCell::new(Token(0)),
            token_2: UnsafeCell::new(Token(1)),
            next_readiness: AtomicPtr::new(ptr::null_mut()),
            update_lock: AtomicBool::new(false),
            num_registration: AtomicUsize::new(1),
            ref_count: AtomicUsize::new(3),
        }
    }

    fn marker() -> ReadinessNode {
        ReadinessNode {
            state: AtomicState::new(Ready::none(), PollOpt::empty()),
            token_0: UnsafeCell::new(Token(0)),
            token_1: UnsafeCell::new(Token(0)),
            token_2: UnsafeCell::new(Token(0)),
            next_readiness: AtomicPtr::new(ptr::null_mut()),
            update_lock: AtomicBool::new(false),
            num_registration: AtomicUsize::new(0),
            ref_count: AtomicUsize::new(0),
        }
    }
}

unsafe fn token(node: &ReadinessNode, pos: usize) -> Token {
    match pos {
        0 => *node.token_0.get(),
        1 => *node.token_1.get(),
        2 => *node.token_2.get(),
        _ => unreachable!(),
    }
}

fn release_node(ptr: *mut ReadinessNode) {
    unsafe {
        // Because `fetch_sub` is already atomic, we do not need to synchronize
        // with other threads unless we are going to delete the object. This
        // same logic applies to the below `fetch_sub` to the `weak` count.
        if (*ptr).ref_count.fetch_sub(1, Release) != 1 {
            return;
        }

        // This fence is needed to prevent reordering of use of the data and
        // deletion of the data.  Because it is marked `Release`, the decreasing
        // of the reference count synchronizes with this `Acquire` fence. This
        // means that use of the data happens before decreasing the reference
        // count, which happens before this fence, which happens before the
        // deletion of the data.
        //
        // As explained in the [Boost documentation][1],
        //
        // > It is important to enforce any possible access to the object in one
        // > thread (through an existing reference) to *happen before* deleting
        // > the object in a different thread. This is achieved by a "release"
        // > operation after dropping a reference (any access to the object
        // > through this reference must obviously happened before), and an
        // > "acquire" operation before deleting the object.
        //
        // [1]: (www.boost.org/doc/libs/1_55_0/doc/html/atomic/usage_examples.html)
        atomic::fence(Acquire);

        let _ = Box::from_raw(ptr);
    }
}

impl AtomicState {
    fn new(interest: Ready, opt: PollOpt) -> AtomicState {
        let state = ReadinessState::new(interest, opt);

        AtomicState {
            inner: AtomicUsize::new(state.into()),
        }
    }

    /// Loads the current `ReadinessState`
    fn load(&self, order: Ordering) -> ReadinessState {
        self.inner.load(order).into()
    }

    /// Stores a state if the current state is the same as `current`.
    fn compare_and_swap(&self, current: ReadinessState, new: ReadinessState, order: Ordering) -> ReadinessState {
        self.inner.compare_and_swap(current.into(), new.into(), order).into()
    }

    fn flag_as_dropped(&self) {
        let prev = self.inner.fetch_or(DROPPED_MASK, Release);
        // The flag should not have been previously set
        debug_assert!(prev & DROPPED_MASK == 0);
    }
}

impl ReadinessState {
    // Create a `ReadinessState` initialized with the provided arguments
    #[inline]
    fn new(interest: Ready, opt: PollOpt) -> ReadinessState {
        let interest = event::ready_as_usize(interest);
        let opt = event::opt_as_usize(opt);

        debug_assert!(interest <= MASK_4);
        debug_assert!(opt <= MASK_4);

        let mut val = interest << INTEREST_SHIFT;
        val |= opt << POLL_OPT_SHIFT;

        ReadinessState(val)
    }

    #[inline]
    fn get(&self, mask: usize, shift: usize) -> usize{
        (self.0 >> shift) & mask
    }

    #[inline]
    fn set(&mut self, val: usize, mask: usize, shift: usize) {
        self.0 = (self.0 & !(mask << shift)) | (val << shift)
    }

    /// Get the readiness
    #[inline]
    fn readiness(&self) -> Ready {
        let v = self.get(MASK_4, READINESS_SHIFT);
        event::ready_from_usize(v)
    }

    #[inline]
    fn effective_readiness(&self) -> Ready {
        self.readiness() & self.interest()
    }

    /// Set the readiness
    #[inline]
    fn set_readiness(&mut self, v: Ready) {
        self.set(event::ready_as_usize(v), MASK_4, READINESS_SHIFT);
    }

    /// Get the interest
    #[inline]
    fn interest(&self) -> Ready {
        let v = self.get(MASK_4, INTEREST_SHIFT);
        event::ready_from_usize(v)
    }

    /// Set the interest
    #[inline]
    fn set_interest(&mut self, v: Ready) {
        self.set(event::ready_as_usize(v), MASK_4, INTEREST_SHIFT);
    }

    #[inline]
    fn disarm(&mut self) {
        self.set_interest(Ready::none());
    }

    /// Get the poll options
    #[inline]
    fn poll_opt(&self) -> PollOpt {
        let v = self.get(MASK_4, POLL_OPT_SHIFT);
        event::opt_from_usize(v)
    }

    /// Set the poll options
    #[inline]
    fn set_poll_opt(&mut self, v: PollOpt) {
        self.set(event::opt_as_usize(v), MASK_4, POLL_OPT_SHIFT);
    }

    #[inline]
    fn is_queued(&self) -> bool {
        self.0 & QUEUED_MASK == QUEUED_MASK
    }

    /// Set the queued flag
    #[inline]
    fn set_queued(&mut self) {
        // Dropped nodes should never be queued
        debug_assert!(!self.is_dropped());
        self.0 |= QUEUED_MASK;
    }

    #[inline]
    fn set_dequeued(&mut self) {
        debug_assert!(self.is_queued());
        self.0 &= !QUEUED_MASK
    }

    #[inline]
    fn is_dropped(&self) -> bool {
        self.0 & DROPPED_MASK == DROPPED_MASK
    }

    #[inline]
    fn token_read_pos(&self) -> usize {
        self.get(MASK_2, TOKEN_RD_SHIFT)
    }

    #[inline]
    fn token_write_pos(&self) -> usize {
        self.get(MASK_2, TOKEN_WR_SHIFT)
    }

    #[inline]
    fn next_token_pos(&self) -> usize {
        let rd = self.token_read_pos();
        let wr = self.token_write_pos();

        match wr {
            0 => {
                match rd {
                    1 => 2,
                    2 => 1,
                    0 => 1,
                    _ => unreachable!(),
                }
            }
            1 => {
                match rd {
                    0 => 2,
                    2 => 0,
                    1 => 2,
                    _ => unreachable!(),
                }
            }
            2 => {
                match rd {
                    0 => 1,
                    1 => 0,
                    2 => 0,
                    _ => unreachable!(),
                }
            }
            _ => unreachable!(),
        }
    }

    #[inline]
    fn set_token_write_pos(&mut self, val: usize) {
        self.set(val, MASK_2, TOKEN_WR_SHIFT);
    }

    #[inline]
    fn update_token_read_pos(&mut self) {
        let val = self.token_write_pos();
        self.set(val, MASK_2, TOKEN_WR_SHIFT);
    }
}

impl From<ReadinessState> for usize {
    fn from(src: ReadinessState) -> usize {
        src.0
    }
}

impl From<usize> for ReadinessState {
    fn from(src: usize) -> ReadinessState {
        ReadinessState(src)
    }
}

fn is_send<T: Send>() {}
fn is_sync<T: Sync>() {}
