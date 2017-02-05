use mio::*;
use std::time::Duration;

#[test]
fn smoke() {
    let poll = Poll::new().unwrap();
    let mut events = Events::with_capacity(128);

    let (_r, set) = Registration::new(&poll, Token(0), Ready::readable(), PollOpt::edge());

    let n = poll.poll(&mut events, Some(Duration::from_millis(0))).unwrap();
    assert_eq!(n, 0);

    set.set_readiness(Ready::readable()).unwrap();

    let n = poll.poll(&mut events, Some(Duration::from_millis(0))).unwrap();
    assert_eq!(n, 1);

    assert_eq!(events.get(0).unwrap().token(), Token(0));
}

#[test]
fn stress() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering::{Acquire, Release};
    use std::thread;

    const NUM_ATTEMPTS: usize = 30;
    const NUM_ITERS: usize = 500;
    const NUM_THREADS: usize = 4;
    const NUM_REGISTRATIONS: usize = 128;

    for _ in 0..NUM_ATTEMPTS {
        let poll = Poll::new().unwrap();
        let mut events = Events::with_capacity(128);

        let registrations: Vec<_> = (0..NUM_REGISTRATIONS).map(|i| {
            Registration::new(&poll, Token(i), Ready::readable(), PollOpt::edge())
        }).collect();

        let mut ready: Vec<_> = (0..NUM_REGISTRATIONS).map(|_| Ready::none()).collect();

        let remaining = Arc::new(AtomicUsize::new(NUM_THREADS));

        for _ in 0..NUM_THREADS {
            let remaining = remaining.clone();

            let set_readiness: Vec<SetReadiness> =
                registrations.iter().map(|r| r.1.clone()).collect();

            thread::spawn(move || {
                for _ in 0..NUM_ITERS {
                    for i in 0..NUM_REGISTRATIONS {
                        set_readiness[i].set_readiness(Ready::readable()).unwrap();
                        set_readiness[i].set_readiness(Ready::none()).unwrap();
                        set_readiness[i].set_readiness(Ready::writable()).unwrap();
                        set_readiness[i].set_readiness(Ready::readable() | Ready::writable()).unwrap();
                        set_readiness[i].set_readiness(Ready::none()).unwrap();
                    }
                }

                for i in 0..NUM_REGISTRATIONS {
                    set_readiness[i].set_readiness(Ready::readable()).unwrap();
                }

                remaining.fetch_sub(1, Release);
            });
        }

        while remaining.load(Acquire) > 0 {
            // Set interest
            for (i, &(ref r, _)) in registrations.iter().enumerate() {
                r.update(&poll, Token(i), Ready::writable(), PollOpt::edge()).unwrap();
            }

            poll.poll(&mut events, Some(Duration::from_millis(0))).unwrap();

            for event in &events {
                ready[event.token().0] = event.kind();
            }

            // Update registration
            // Set interest
            for (i, &(ref r, _)) in registrations.iter().enumerate() {
                r.update(&poll, Token(i), Ready::readable(), PollOpt::edge()).unwrap();
            }
        }

        // One final poll
        poll.poll(&mut events, Some(Duration::from_millis(0))).unwrap();

        for event in &events {
            ready[event.token().0] = event.kind();
        }

        // Everything should be flagged as readable
        for ready in ready {
            assert_eq!(ready, Ready::readable());
        }
    }
}

#[test]
fn drop_registration_from_non_main_thread() {
    use std::thread;
    use std::sync::mpsc::channel;

    const THREADS: usize = 8;
    const ITERS: usize = 50_000;

    let mut poll = Poll::new().unwrap();
    let mut events = Events::with_capacity(1024);
    let mut senders = Vec::with_capacity(THREADS);
    let mut token_index = 0;

    // spawn threads, which will send messages to single receiver
    for _ in 0..THREADS {
        let (tx, rx) = channel::<(Registration, SetReadiness)>();
        senders.push(tx);

        thread::spawn(move || {
            for (registration, set_readiness) in rx {
                let _ = set_readiness.set_readiness(Ready::readable());
                drop(registration);
                drop(set_readiness);
            }
        });
    }

    let mut index: usize = 0;
    for _ in 0..ITERS {
        let (registration, set_readiness) = Registration::new(&mut poll, Token(token_index), Ready::readable(), PollOpt::edge());
        let _ = senders[index].send((registration, set_readiness));

        token_index += 1;
        index += 1;
        if index == THREADS {
            index = 0;

            let (registration, set_readiness) = Registration::new(&mut poll, Token(token_index), Ready::readable(), PollOpt::edge());
            let _ = set_readiness.set_readiness(Ready::readable());
            drop(registration);
            drop(set_readiness);
            token_index += 1;

            thread::park_timeout(Duration::from_millis(0));
            let _ = poll.poll(&mut events, None).unwrap();
        }
    }
}
