#![feature(const_fn, raw)]

#[cfg(test)]
#[macro_use]
extern crate lazy_static;

use std::sync::atomic::{AtomicPtr, AtomicUsize, AtomicIsize, AtomicBool, Ordering};
use std::ptr;
use std::mem;
use std::marker::PhantomData;
use std::raw::TraitObject;
use std::ops::Deref;
use std::thread::{self, JoinHandle};
use std::isize;
use std::cell::Cell;
use std::panic;
use std::cmp;

pub unsafe trait CgcCompat : Send + Sync {
    unsafe fn root(&self);
    unsafe fn unroot(&self);
    unsafe fn darken(&self);
}

const WHITE: usize = 0;
const GRAY: usize = 1;
const BLACK: usize = 2;

const INITIAL_HEAP_SIZE: isize = 1024;

static BOX_CHAIN: AtomicPtr<CgcHeader> = AtomicPtr::new(ptr::null_mut());
static ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static HEAP_SPACE: AtomicIsize = AtomicIsize::new(INITIAL_HEAP_SIZE);
static COLLECTING: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
#[repr(C)]
struct CgcHeader {
    vtable: *mut (),
    color: AtomicUsize,
    roots: AtomicUsize,

    // NOTE: CgcHeader is shared across threads. This Cell<T> is only safe
    // because access to the next property is only used when the CgcHeader is
    // exclusively owned (during `Cgc::new()`), and during Garbage Collection.
    // It is unsound for other threads to access this property.
    // XXX: Should this be an UnsafeCell?
    next: Cell<*const CgcHeader>,
}

impl CgcHeader {
    /// This function relies on the layout of the CgcBox<T> object, in that it
    /// relies that the `self` pointer is the same as the data pointer for the
    /// CgcBox type. It extracts the stored vtable and uses it to create a
    /// TraitObject.
    unsafe fn as_cgc_box(this: *const Self) -> *const CgcBox<CgcCompat> {
        mem::transmute::<TraitObject, *const CgcBox<CgcCompat>>(TraitObject {
            data: this as *mut Self as *mut (),
            vtable: (*this).vtable,
        })
    }

    /// Root the data stored behind the pointer by incrementing the root
    /// count and coloring the node gray if it isn't already rooted.
    fn root(&self) {
        if self.roots.fetch_add(1, Ordering::SeqCst) == 0 {
            self.color.compare_and_swap(WHITE, GRAY, Ordering::SeqCst);
        }
    }

    /// When unrooting we don't need to handle changing the color of the node
    fn unroot(&self) {
        self.roots.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Debug)]
#[repr(C)]
struct CgcBox<T: ?Sized + CgcCompat> {
    header: CgcHeader,
    data: T,
}


pub struct Cgc<T: CgcCompat> {
    // XXX: We should be able to store rooted in the low bits of the pointer, but
    // it would probably hurt performance substantially to require atomic loads
    // to read the pointer when only the low bit needs to be atomic, so instead,
    // we store the atomic bit seperately and inflate the size of a Cgc<T> a bit.
    // XXX: This should be NonZero
    ptr: *const CgcBox<T>,
    // XXX: This might not be atomic. This should only ever be set or read by a
    // single thread at a time.
    rooted: AtomicBool,
    _marker: PhantomData<T>,
}

// We need to manually implement these marker traits because we hold raw pointers.
unsafe impl<T: Send + Sync + CgcCompat> Send for Cgc<T> {}
unsafe impl<T: Send + Sync + CgcCompat> Sync for Cgc<T> {}

impl<T: Send + Sync + CgcCompat> Cgc<T> {
    pub fn new(data: T) -> Cgc<T> {
        // Bookkeeping - record how many bytes we're about to allocated, and
        // potentially force a collection.
        let box_size = mem::size_of::<CgcBox<T>>();
        assert!(box_size < isize::MAX as usize);

        ALLOCATED.fetch_add(box_size, Ordering::SeqCst);
        let heap_space = HEAP_SPACE.fetch_sub(box_size as isize, Ordering::SeqCst);
        if heap_space < box_size as isize {
            let _ = force_collection();
        }

        let mut heap = Box::new(CgcBox {
            header: CgcHeader {
                // When new entries are created, they are created gray. This
                // ensures that nodes which are created during garbage
                // collection are considered alive.
                color: AtomicUsize::new(GRAY),
                // The new heap entry will start out rooted, so we can start it with
                // a root count of 1.
                roots: AtomicUsize::new(1),
                next: Cell::new(ptr::null()),
                vtable: ptr::null_mut(),
            },
            data: data,
        });

        unsafe {
            // Extract the trait object's vtable, and store it in the heap data.
            // This is necessary to be able to traverse it later.
            heap.header.vtable = mem::transmute::<&CgcBox<CgcCompat>, TraitObject>(&*heap).vtable;
        }

        let heap_ptr = Box::into_raw(heap);

        // Add the new box to the global lockfree linked list
        let mut head = BOX_CHAIN.load(Ordering::SeqCst);
        loop {
            unsafe { (*heap_ptr).header.next.set(head); }

            match BOX_CHAIN.compare_exchange(head, heap_ptr as *mut CgcHeader, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => break,
                Err(p) => head = p,
            }
        }

        // We've added ourself to the discoverable list, so we can unroot our innards now.
        unsafe {
            (*heap_ptr).data.unroot();
        }

        Cgc {
            ptr: heap_ptr,
            rooted: AtomicBool::new(true),
            _marker: PhantomData,
        }
    }

    fn header(&self) -> &CgcHeader {
        unsafe { &(*self.ptr).header }
    }

    fn data(&self) -> &T {
        unsafe { &(*self.ptr).data }
    }
}

impl<T: Send + Sync + CgcCompat> Clone for Cgc<T> {
    fn clone(&self) -> Self {
        // Root the data stored behind the pointer by incrementing the root
        // count and coloring the node gray if it isn't already rooted.
        if self.header().roots.fetch_add(1, Ordering::SeqCst) == 0 {
            self.header().color.compare_and_swap(WHITE, GRAY, Ordering::SeqCst);
        }

        Cgc {
            ptr: self.ptr,
            rooted: AtomicBool::new(true),
            _marker: PhantomData,
        }
    }
}

impl<T: Send + Sync + CgcCompat> Deref for Cgc<T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.data()
    }
}

impl<T: CgcCompat> Drop for Cgc<T> {
    fn drop(&mut self) {
        if self.rooted.load(Ordering::SeqCst) {
            self.header().unroot();
        }
    }
}

unsafe impl<T: Send + Sync + CgcCompat> CgcCompat for Cgc<T> {
    unsafe fn root(&self) {
        self.header().root();
        let old = self.rooted.swap(true, Ordering::SeqCst);
        debug_assert!(!old);
    }

    unsafe fn unroot(&self) {
        self.header().unroot();
        let old = self.rooted.swap(true, Ordering::SeqCst);
        debug_assert!(old);
    }

    unsafe fn darken(&self) {
        if self.header().color.compare_and_swap(WHITE, BLACK, Ordering::SeqCst) == WHITE {
            println!("{:?} is WHITE setting to BLACK", self as *const _);
            self.data().darken();
        }
    }
}

unsafe fn do_collect_garbage() {
    // NOTE: The stuff we're doing in this function is embarassingly
    // parallelizable (we could traverse and finalize each gray object in our
    // graph silmultaneously!). Consider taking advantage of that with some sort
    // of thread pool.

    /// Color our roots to their initial colors
    unsafe fn whiten() {
        println!("WHITEN");
        let mut head = BOX_CHAIN.load(Ordering::SeqCst) as *const CgcHeader;
        while !head.is_null() {
            // * If we read a zero, and then it gets incremented, we're fine, as
            //   the increment will mark it as gray.
            //
            // * If we whiten a node, it's roots get decremented to zero, and
            // then we read a zero, it must be accessible from the heap either
            // in an entry we're looking at, or an entry which has been created
            // since we loaded head above, which will be created as a gray node.
            (*head).color.store(WHITE, Ordering::SeqCst);
            println!("Set {:?} to WHITE", head);
            if (*head).roots.load(Ordering::SeqCst) > 0 {
                println!("Set {:?} to GRAY", head);
                (*head).color.store(GRAY, Ordering::SeqCst);
            }
            head = (*head).next.get();
        }
    }

    /// Mark all of the reachable nodes as black
    unsafe fn blacken(head: &Cell<*const CgcHeader>,
                      black_head: &Cell<*const CgcHeader>)
                      -> Option<*const Cell<*const CgcHeader>> {
        println!("Running the blacken pass");

        let mut black_tail: Option<*const Cell<*const CgcHeader>> = None;
        loop {
            let mut saw_grays = false;
            let mut h = head;
            while !h.get().is_null() {
                // If the current node is GRAY, make it BLACK and add it to black_head
                let old_color = (*h.get()).color.compare_and_swap(GRAY, BLACK, Ordering::SeqCst);
                match old_color {
                    WHITE => {
                        h = &(*h.get()).next;
                        continue;
                    }
                    GRAY => {
                        println!("{:?} GRAY => BLACK", h.get());
                        saw_grays = true;
                        // XXX: Consider running darken method with a threadpool
                        (*CgcHeader::as_cgc_box(h.get())).data.darken();
                    }
                    _ => {}
                }
                // The entry was either GRAY or BLACK
                assert!(old_color == GRAY || old_color == BLACK,
                        "Entry should have been either GRAY or BLACK at this point");
                // Add the entry to black_head, and remove it from head.
                let old_black_head = black_head.get();
                black_head.set(h.get());
                h.set((*black_head.get()).next.get());
                (*black_head.get()).next.set(old_black_head);

                if black_tail.is_none() {
                    black_tail = Some(&(*black_head.get()).next);
                }
            }

            // If we didn't see any gray entries, we can finish now, as we just
            // finished removing all black entries from `head`.
            if !saw_grays {
                break;
            }
        }

        black_tail
    }

    whiten();

    // Take ownership over the box chain, so that we can start collecting stuff
    // in it. This is seperate from the whitening load because objects which
    // have been created between the start of the whitening phase and now may be
    // keeping white nodes alive (as we were marking nodes as white). After this
    // point, new nodes cannot be keeping white nodes alive.
    let head = Cell::new(BOX_CHAIN.swap(ptr::null_mut(), Ordering::SeqCst) as *const _);
    let black_head = Cell::new(ptr::null());

    // The blacken phase split our linked list of allocations into two lists,
    // the allocations which can be dropped (`head`), and the allocations which
    // are still alive (`black_head`).
    let black_tail = blacken(&head, &black_head);

    // Restore the items in `black_head` into the BOX_CHAIN.
    match black_tail {
        Some(tail) => {
            // Replace BLOCK_CHAIN with black_head, and set black_tail to point
            // to the old value of BLOCK_CHAIN.
            (*tail).set(BOX_CHAIN.swap(black_head.get() as *mut _, Ordering::SeqCst));
        }
        None => {
            assert!(black_head.get().is_null());
        }
    }

    // Drop all of the items found in the white head list
    let mut freed = 0;
    while !head.get().is_null() {
        let node = head.get();
        head.set((*node).next.get());

        // This frees the memory at the end of the block
        let b = Box::from_raw(CgcHeader::as_cgc_box(node) as *mut CgcBox<CgcCompat>);
        freed += mem::size_of_val(&*b);
    }

    // Record the freed memory
    let allocated = ALLOCATED.fetch_sub(freed, Ordering::SeqCst) - freed;

    // Determine how many bytes must be allocated before the next collection
    let extra_heap = cmp::max((allocated as f64 * 0.7) as isize,
                              INITIAL_HEAP_SIZE);
    HEAP_SPACE.store(extra_heap, Ordering::SeqCst);
}

unsafe fn collect_garbage() {
    let _ = panic::catch_unwind(|| {
        do_collect_garbage();
    });

    // We want to make sure to set collecting back to false when we're done
    // whether or not the garbage collector panicked.
    COLLECTING.store(false, Ordering::SeqCst);
}

/// returns Ok(JoinHandle) if we successfully started a garbage collection, and
/// Err(()) if a collection is currently in progress.
pub fn force_collection() -> Result<JoinHandle<()>, ()> {
    if COLLECTING.compare_and_swap(false, true, Ordering::SeqCst) == false {
        Ok(thread::spawn(|| unsafe { collect_garbage() }))
    } else {
        Err(())
    }
}

unsafe impl CgcCompat for i32 {
    unsafe fn root(&self) {}
    unsafe fn unroot(&self) {}
    unsafe fn darken(&self) {}
}

unsafe impl<T: CgcCompat> CgcCompat for Option<T> {
    unsafe fn root(&self) {
        match *self {
            Some(ref x) => (*x).root(),
            None => {}
        }
    }
    unsafe fn unroot(&self) {
        match *self {
            Some(ref x) => (*x).unroot(),
            None => {}
        }
    }
    unsafe fn darken(&self) {
        match *self {
            Some(ref x) => (*x).darken(),
            None => {}
        }
    }
}

#[macro_export]
macro_rules! derive_CgcCompat_body {
    ($($field:tt),*) => {
        unsafe fn root(&self) {
            $($crate::CgcCompat::root(&self.$field);)*
        }
        unsafe fn unroot(&self) {
            $($crate::CgcCompat::unroot(&self.$field);)*
        }
        unsafe fn darken(&self) {
            $($crate::CgcCompat::darken(&self.$field);)*
        }
    }
}

#[macro_export]
macro_rules! derive_CgcCompat {
    ($name:ident, $($field:tt),*) => {
        unsafe impl $crate::CgcCompat for $name {
            derive_CgcCompat_body!($($field),*);
        }
    }
}

#[cfg(test)]
mod test;
