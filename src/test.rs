use std::sync::atomic::Ordering;
use super::{Cgc, force_collection, ALLOCATED, CgcCompat};
use std::sync::Mutex;

lazy_static! {
    static ref TEST_MUTEX: Mutex<()> = Mutex::new(());
}

macro_rules! test_start {
    () => {
        // Lock the test mutex so that only one test gets to run at a time
        let _gc_test_lock = TEST_MUTEX.lock();

        // Force a GC such that the heap should be empty
        if let Ok(jh) = force_collection() {
            jh.join().unwrap();
        } else { unreachable!(); }

        // Ensure that the heap is empty
        assert_eq!(0, ALLOCATED.load(Ordering::SeqCst));
    }
}

#[test]
fn simple() {
    test_start!();

    {
        let _still_alive = Cgc::new(5);
        assert_eq!(40, ALLOCATED.load(Ordering::SeqCst));

        if let Ok(jh) = force_collection() {
            jh.join().unwrap();
            assert_eq!(40, ALLOCATED.load(Ordering::SeqCst));
        } else { unreachable!(); }
    }

    if let Ok(jh) = force_collection() {
        jh.join().unwrap();
        assert_eq!(0, ALLOCATED.load(Ordering::SeqCst));
    } else { unreachable!(); }
}

struct Thing(Option<Cgc<Thing>>);

derive_CgcCompat!(Thing, 0);


#[test]
fn deeper() {
    test_start!();

    {
        let _still_alive = Cgc::new(Thing(Some(Cgc::new(Thing(None)))));
        assert_eq!(112, ALLOCATED.load(Ordering::SeqCst));

        if let Ok(jh) = force_collection() {
            jh.join().unwrap();
            assert_eq!(112, ALLOCATED.load(Ordering::SeqCst));
        } else { unreachable!(); }
    }

    if let Ok(jh) = force_collection() {
        jh.join().unwrap();
        assert_eq!(0, ALLOCATED.load(Ordering::SeqCst));
    } else { unreachable!(); }
}

