use core::cell::UnsafeCell;
use core::fmt;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use crossbeam::utils::{Backoff, CachePadded};

// Bits indicating the state of a slot:
// * If a value has been written into the slot, `WRITE` is set.
// * If a value has been read from the slot, `READ` is set.
// * If the block is being destroyed, `DESTROY` is set.
const WRITE: usize = 1;

// Each block covers one "lap" of indices.
const LAP: usize = 32;
// The maximum number of values a block can hold.
const BLOCK_CAP: usize = LAP - 1;
// How many lower bits are reserved for metadata.
const SHIFT: usize = 1;
// Indicates that the block is not the last one.
const HAS_NEXT: usize = 1;

/// A slot in a block.
struct Slot<T> {
    /// The value.
    value: UnsafeCell<MaybeUninit<T>>,

    /// The state of the slot.
    state: AtomicUsize,
}

impl<T> Slot<T> {
    const UNINIT: Self = Self {
        value: UnsafeCell::new(MaybeUninit::uninit()),
        state: AtomicUsize::new(0),
    };

    /// Waits until a value is written into the slot.
    fn wait_write(&self) {
        let backoff = Backoff::new();
        while self.state.load(Ordering::Acquire) & WRITE == 0 {
            backoff.snooze();
        }
    }
}

/// A block in a linked list.
///
/// Each block in the list can hold up to `BLOCK_CAP` values.
struct Block<T> {
    /// The next block in the linked list.
    next: AtomicPtr<Block<T>>,

    /// Slots for values.
    slots: [Slot<T>; BLOCK_CAP],
}

impl<T> Block<T> {
    /// Creates an empty block that starts at `start_index`.
    fn new() -> Block<T> {
        Self {
            next: AtomicPtr::new(ptr::null_mut()),
            slots: [Slot::UNINIT; BLOCK_CAP],
        }
    }

    /// Waits until the next pointer is set.
    fn wait_next(&self) -> *mut Block<T> {
        let backoff = Backoff::new();
        loop {
            let next = self.next.load(Ordering::Acquire);
            if !next.is_null() {
                return next;
            }
            backoff.snooze();
        }
    }

    /// Sets the `DESTROY` bit in slots starting from `start` and destroys the block.
    unsafe fn destroy(this: *mut Block<T>) {
        // No thread is using the block, now it is safe to destroy it.
        drop(Box::from_raw(this));
    }
}

/// A position in a queue.
struct Position<T> {
    /// The index in the queue.
    index: AtomicUsize,

    /// The block in the linked list.
    block: AtomicPtr<Block<T>>,
}

impl<T> Position<T> {
    fn load_index(&self) -> usize {
        #[allow(clippy::cast_ref_to_mut)]
        let index = unsafe { &mut *(&self.index as *const _ as *mut AtomicUsize) };
        *index.get_mut()
    }

    fn set_index(&self, index: usize) {
        #[allow(clippy::cast_ref_to_mut)]
        let idx = unsafe { &mut *(&self.index as *const _ as *mut AtomicUsize) };
        *idx.get_mut() = index;
    }

    fn load_block(&self) -> *mut Block<T> {
        #[allow(clippy::cast_ref_to_mut)]
        let block = unsafe { &mut *(&self.block as *const _ as *mut AtomicPtr<Block<T>>) };
        *block.get_mut()
    }

    fn set_block(&self, block: *mut Block<T>) {
        #[allow(clippy::cast_ref_to_mut)]
        let blk = unsafe { &mut *(&self.block as *const _ as *mut AtomicPtr<Block<T>>) };
        *blk.get_mut() = block;
    }
}

/// An unbounded multi-producer multi-consumer queue.
///
/// This queue is implemented as a linked list of segments, where each segment is a small buffer
/// that can hold a handful of elements. There is no limit to how many elements can be in the queue
/// at a time. However, since segments need to be dynamically allocated as elements get pushed,
/// this queue is somewhat slower than [`ArrayQueue`].
///
/// [`ArrayQueue`]: super::ArrayQueue
///
/// # Examples
///
/// ```
/// use may::sync::queue::spsc_seg_queue::SegQueue;
///
/// let q = SegQueue::new();
///
/// q.push('a');
/// q.push('b');
///
/// assert_eq!(q.pop(), Some('a'));
/// assert_eq!(q.pop(), Some('b'));
/// assert!(q.pop().is_none());
/// ```
pub struct SegQueue<T> {
    /// The head of the queue.
    head: CachePadded<Position<T>>,

    /// The tail of the queue.
    tail: CachePadded<Position<T>>,

    /// Indicates that dropping a `SegQueue<T>` may drop values of type `T`.
    _marker: PhantomData<T>,
}

unsafe impl<T: Send> Send for SegQueue<T> {}
unsafe impl<T: Send> Sync for SegQueue<T> {}

impl<T> SegQueue<T> {
    /// Creates a new unbounded queue.
    ///
    /// # Examples
    ///
    /// ```
    /// use may::sync::queue::spsc_seg_queue::SegQueue;
    ///
    /// let q = SegQueue::<i32>::new();
    /// ```
    pub const fn new() -> SegQueue<T> {
        SegQueue {
            head: CachePadded::new(Position {
                block: AtomicPtr::new(ptr::null_mut()),
                index: AtomicUsize::new(0),
            }),
            tail: CachePadded::new(Position {
                block: AtomicPtr::new(ptr::null_mut()),
                index: AtomicUsize::new(0),
            }),
            _marker: PhantomData,
        }
    }

    /// Pushes an element into the queue.
    ///
    /// # Examples
    ///
    /// ```
    /// use may::sync::queue::spsc_seg_queue::SegQueue;
    ///
    /// let q = SegQueue::new();
    ///
    /// q.push(10);
    /// q.push(20);
    /// ```
    pub fn push(&self, value: T) {
        // let backoff = Backoff::new();
        let tail = self.tail.load_index();
        let mut block = self.tail.load_block();
        let mut next_block = None;

        // loop {
        // Calculate the offset of the index into the block.
        let offset = (tail >> SHIFT) % LAP;

        // If we're going to have to install the next block, allocate it in advance in order to
        // make the wait for other threads as short as possible.
        if offset + 1 == BLOCK_CAP && next_block.is_none() {
            next_block = Some(Box::new(Block::<T>::new()));
        }

        // If this is the first push operation, we need to allocate the first block.
        if block.is_null() {
            let new = Box::into_raw(Box::new(Block::<T>::new()));
            self.tail.set_block(new);
            self.head.block.store(new, Ordering::Release);
            block = new;
        }

        let new_tail = tail + (1 << SHIFT);
        self.tail.index.store(new_tail, Ordering::Release);

        // If we've reached the end of the block, install the next one.
        if offset + 1 == BLOCK_CAP {
            let next_block = Box::into_raw(next_block.unwrap());
            let next_index = new_tail.wrapping_add(1 << SHIFT);

            self.tail.set_block(next_block);
            self.tail.index.store(next_index, Ordering::Release);
            unsafe { (*block).next.store(next_block, Ordering::Relaxed) };
        }

        // Write the value into the slot.
        unsafe {
            let slot = (*block).slots.get_unchecked(offset);
            slot.value.get().write(MaybeUninit::new(value));
            slot.state.fetch_or(WRITE, Ordering::Release);
        }
    }

    /// Pops an element from the queue.
    ///
    /// If the queue is empty, `None` is returned.
    ///
    /// # Examples
    ///
    /// ```
    /// use may::sync::queue::spsc_seg_queue::SegQueue;
    ///
    /// let q = SegQueue::new();
    ///
    /// q.push(10);
    /// assert_eq!(q.pop(), Some(10));
    /// assert!(q.pop().is_none());
    /// ```
    pub fn pop(&self) -> Option<T> {
        let backoff = Backoff::new();
        let mut head = self.head.load_index();
        let mut block = self.head.block.load(Ordering::Acquire);

        loop {
            // Calculate the offset of the index into the block.
            let offset = (head >> SHIFT) % LAP;

            let mut new_head = head + (1 << SHIFT);

            if new_head & HAS_NEXT == 0 {
                // atomic::fence(Ordering::SeqCst);
                let tail = self.tail.index.load(Ordering::Acquire);

                // If the tail equals the head, that means the queue is empty.
                if head >> SHIFT == tail >> SHIFT {
                    return None;
                }

                // If head and tail are not in the same block, set `HAS_NEXT` in head.
                if (head >> SHIFT) / LAP != (tail >> SHIFT) / LAP {
                    new_head |= HAS_NEXT;
                }
            }

            // The block can be null here only if the first push operation is in progress. In that
            // case, just wait until it gets initialized.
            if block.is_null() {
                backoff.snooze();
                head = self.head.load_index();
                block = self.head.block.load(Ordering::Acquire);
                continue;
            }

            self.head.set_index(new_head);

            unsafe {
                // If we've reached the end of the block, move to the next one.
                if offset + 1 == BLOCK_CAP {
                    let next = (*block).wait_next();
                    let mut next_index = (new_head & !HAS_NEXT).wrapping_add(1 << SHIFT);
                    if !(*next).next.load(Ordering::Relaxed).is_null() {
                        next_index |= HAS_NEXT;
                    }

                    self.head.set_block(next);
                    self.head.set_index(next_index);
                }

                // Read the value.
                let slot = (*block).slots.get_unchecked(offset);
                slot.wait_write();
                let value = slot.value.get().read().assume_init();

                // Destroy the block if we've reached the end, or if another thread wanted to
                // destroy but couldn't because we were busy reading from the slot.
                if offset + 1 == BLOCK_CAP {
                    Block::destroy(block);
                }

                return Some(value);
            }
        }
    }

    /// Returns `true` if the queue is empty.
    ///
    /// # Examples
    ///
    /// ```
    /// use may::sync::queue::spsc_seg_queue::SegQueue;
    ///
    /// let q = SegQueue::new();
    ///
    /// assert!(q.is_empty());
    /// q.push(1);
    /// assert!(!q.is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        let head = self.head.index.load(Ordering::SeqCst);
        let tail = self.tail.index.load(Ordering::SeqCst);
        head >> SHIFT == tail >> SHIFT
    }

    /// Returns the number of elements in the queue.
    ///
    /// # Examples
    ///
    /// ```
    /// use may::sync::queue::spsc_seg_queue::SegQueue;
    ///
    /// let q = SegQueue::new();
    /// assert_eq!(q.len(), 0);
    ///
    /// q.push(10);
    /// assert_eq!(q.len(), 1);
    ///
    /// q.push(20);
    /// assert_eq!(q.len(), 2);
    /// ```
    pub fn len(&self) -> usize {
        loop {
            // Load the tail index, then load the head index.
            let mut tail = self.tail.index.load(Ordering::SeqCst);
            let mut head = self.head.index.load(Ordering::SeqCst);

            // If the tail index didn't change, we've got consistent indices to work with.
            if self.tail.index.load(Ordering::SeqCst) == tail {
                // Erase the lower bits.
                tail &= !((1 << SHIFT) - 1);
                head &= !((1 << SHIFT) - 1);

                // Fix up indices if they fall onto block ends.
                if (tail >> SHIFT) & (LAP - 1) == LAP - 1 {
                    tail = tail.wrapping_add(1 << SHIFT);
                }
                if (head >> SHIFT) & (LAP - 1) == LAP - 1 {
                    head = head.wrapping_add(1 << SHIFT);
                }

                // Rotate indices so that head falls into the first block.
                let lap = (head >> SHIFT) / LAP;
                tail = tail.wrapping_sub((lap * LAP) << SHIFT);
                head = head.wrapping_sub((lap * LAP) << SHIFT);

                // Remove the lower bits.
                tail >>= SHIFT;
                head >>= SHIFT;

                // Return the difference minus the number of blocks between tail and head.
                return tail - head - tail / LAP;
            }
        }
    }
}

impl<T> Drop for SegQueue<T> {
    fn drop(&mut self) {
        let mut head = *self.head.index.get_mut();
        let mut tail = *self.tail.index.get_mut();
        let mut block = *self.head.block.get_mut();

        // Erase the lower bits.
        head &= !((1 << SHIFT) - 1);
        tail &= !((1 << SHIFT) - 1);

        unsafe {
            // Drop all values between `head` and `tail` and deallocate the heap-allocated blocks.
            while head != tail {
                let offset = (head >> SHIFT) % LAP;

                if offset < BLOCK_CAP {
                    // Drop the value in the slot.
                    let slot = (*block).slots.get_unchecked(offset);
                    let p = &mut *slot.value.get();
                    p.as_mut_ptr().drop_in_place();
                } else {
                    // Deallocate the block and move to the next one.
                    let next = *(*block).next.get_mut();
                    drop(Box::from_raw(block));
                    block = next;
                }

                head = head.wrapping_add(1 << SHIFT);
            }

            // Deallocate the last remaining block.
            if !block.is_null() {
                drop(Box::from_raw(block));
            }
        }
    }
}

impl<T> fmt::Debug for SegQueue<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("SegQueue { .. }")
    }
}

impl<T> Default for SegQueue<T> {
    fn default() -> SegQueue<T> {
        SegQueue::new()
    }
}

impl<T> IntoIterator for SegQueue<T> {
    type Item = T;

    type IntoIter = IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        IntoIter { value: self }
    }
}

#[derive(Debug)]
pub struct IntoIter<T> {
    value: SegQueue<T>,
}

impl<T> Iterator for IntoIter<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        let value = &mut self.value;
        let head = *value.head.index.get_mut();
        let tail = *value.tail.index.get_mut();
        if head >> SHIFT == tail >> SHIFT {
            None
        } else {
            let block = *value.head.block.get_mut();
            let offset = (head >> SHIFT) % LAP;

            // SAFETY: We have mutable access to this, so we can read without
            // worrying about concurrency. Furthermore, we know this is
            // initialized because it is the value pointed at by `value.head`
            // and this is a non-empty queue.
            let item = unsafe {
                let slot = (*block).slots.get_unchecked(offset);
                let p = &mut *slot.value.get();
                p.as_mut_ptr().read()
            };
            if offset + 1 == BLOCK_CAP {
                // Deallocate the block and move to the next one.
                // SAFETY: The block is initialized because we've been reading
                // from it this entire time. We can drop it b/c everything has
                // been read out of it, so nothing is pointing to it anymore.
                unsafe {
                    let next = *(*block).next.get_mut();
                    drop(Box::from_raw(block));
                    *value.head.block.get_mut() = next;
                }
                // The last value in a block is empty, so skip it
                *value.head.index.get_mut() = head.wrapping_add(2 << SHIFT);
                // Double-check that we're pointing to the first item in a block.
                debug_assert_eq!((*value.head.index.get_mut() >> SHIFT) % LAP, 0);
            } else {
                *value.head.index.get_mut() = head.wrapping_add(1 << SHIFT);
            }
            Some(item)
        }
    }
}
