use crate::internal::{
    alloc::{dyn_vec::DynElemMut, DynVec},
    epoch::QuiesceEpoch,
    pointer::PtrExt,
    stats,
    tcell_erased::TCellErased,
    usize_aligned::{ForcedUsizeAligned, UsizeAligned},
};
use std::{
    mem::{self, ManuallyDrop},
    num::NonZeroUsize,
    ptr::{self, NonNull},
    raw::TraitObject,
    sync::atomic::Ordering::{Acquire, Relaxed, Release},
};

#[repr(C)]
pub struct WriteEntryImpl<T> {
    dest:    Option<NonNull<TCellErased>>,
    pending: ForcedUsizeAligned<T>,
}

impl<T> WriteEntryImpl<T> {
    #[inline]
    pub const fn new(dest: &TCellErased, pending: T) -> Self {
        WriteEntryImpl {
            dest:    Some(unsafe { NonNull::new_unchecked(dest as *const _ as _) }),
            pending: ForcedUsizeAligned::new(pending),
        }
    }
}

pub unsafe trait WriteEntry {}
unsafe impl<T> WriteEntry for WriteEntryImpl<T> {}

impl<'a> dyn WriteEntry + 'a {
    #[inline]
    fn tcell(&self) -> Option<NonNull<TCellErased>> {
        let ptr = unsafe { *self.tcell_ptr().as_ptr() };
        likely!(ptr.is_some());
        ptr
    }

    #[inline]
    fn tcell_ptr(&self) -> NonNull<Option<NonNull<TCellErased>>> {
        unsafe {
            let raw: TraitObject = mem::transmute::<&Self, _>(self);
            NonNull::new_unchecked(raw.data as *mut _)
        }
    }

    #[inline]
    fn pending(&self) -> NonNull<usize> {
        unsafe { self.tcell_ptr().add(1).cast() }
    }

    #[inline]
    pub fn deactivate(&mut self) {
        debug_assert!(
            self.tcell().is_some(),
            "unexpectedly deactivating an inactive write log entry"
        );
        unsafe { *self.tcell_ptr().as_mut() = None }
    }

    #[inline]
    pub fn is_dest_tcell(&self, v: &TCellErased) -> bool {
        match self.tcell() {
            Some(ptr) => ptr::eq(ptr.as_ptr(), v),
            None => false,
        }
    }

    #[inline]
    pub unsafe fn read<T>(&self) -> ManuallyDrop<T> {
        debug_assert!(
            mem::size_of_val(self) == mem::size_of::<UsizeAligned<T>>() + mem::size_of::<usize>(),
            "destination size error during `WriteEntry::read`"
        );
        assert!(
            mem::size_of::<T>() > 0,
            "`WriteEntry` performing a read of size 0 unexpectedly"
        );
        let mut value: UsizeAligned<ManuallyDrop<T>> = mem::uninitialized();
        let slice = value.as_mut();
        self.pending().copy_to_n(slice.as_mut_ptr(), slice.len());
        value.into_inner()
    }

    #[inline]
    #[must_use]
    pub unsafe fn try_lock(&self, pin_epoch: QuiesceEpoch) -> bool {
        match self.tcell() {
            Some(ptr) => ptr
                .as_ref()
                .current_epoch
                .try_lock(pin_epoch, Acquire, Relaxed),
            None => true,
        }
    }

    #[inline]
    pub unsafe fn unlock(&self) {
        match self.tcell() {
            Some(ptr) => ptr.as_ref().current_epoch.unlock(Release),
            None => {}
        }
    }

    #[inline]
    pub unsafe fn perform_write(&self) {
        match self.tcell() {
            Some(ptr) => {
                let size = mem::size_of_val(self);
                assume!(
                    size % mem::size_of::<usize>() == 0,
                    "buggy alignment on `WriteEntry`"
                );
                let len = size / mem::size_of::<usize>() - 1;
                assume!(
                    len > 0,
                    "`WriteEntry` performing a write of size 0 unexpectedly"
                );
                let src = std::slice::from_raw_parts(self.pending().as_ptr(), len);
                ptr.as_ref().store_release(src)
            }
            None => {}
        }
    }

    #[inline]
    pub unsafe fn publish(&self, publish_epoch: QuiesceEpoch) {
        match self.tcell() {
            Some(ptr) => ptr
                .as_ref()
                .current_epoch
                .unlock_as_epoch(publish_epoch, Release),
            None => {}
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum Contained {
    No,
    Maybe,
}

/// TODO: WriteLog is very very slow if the bloom filter fails.
/// probably worth looking into some true hashmaps
#[repr(C)]
pub struct WriteLog {
    filter: usize,
    data:   DynVec<dyn WriteEntry>,
}

impl WriteLog {
    #[inline]
    pub fn new() -> Self {
        WriteLog {
            filter: 0,
            data:   DynVec::new(),
        }
    }

    #[inline]
    pub fn contained(&self, hash: NonZeroUsize) -> Contained {
        stats::bloom_check();
        if unlikely!(self.filter & hash.get() != 0) {
            Contained::Maybe
        } else {
            Contained::No
        }
    }

    #[inline]
    pub fn clear(&mut self) {
        self.filter = 0;
        // TODO: NESTING: tx's can start here
        stats::write_word_size(self.data.word_len());
        self.data.clear();
    }

    #[inline]
    pub fn clear_no_drop(&mut self) {
        self.filter = 0;
        stats::write_word_size(self.data.word_len());
        self.data.clear_no_drop();
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        unsafe {
            if self.filter == 0 {
                assume!(
                    self.data.is_empty(),
                    "bloom filter and container out of sync"
                );
                true
            } else {
                assume!(
                    !self.data.is_empty(),
                    "bloom filter and container out of sync"
                );
                false
            }
        }
    }

    #[inline(never)]
    fn find_slow(&self, dest_tcell: &TCellErased) -> Option<&dyn WriteEntry> {
        let result = self
            .data
            .iter()
            .find(move |&entry| entry.is_dest_tcell(dest_tcell));
        if result.is_some() {
            stats::bloom_success_slow()
        } else {
            stats::bloom_failure()
        }
        result
    }

    // biased against finding the tcell
    #[inline]
    pub fn find(&self, dest_tcell: &TCellErased) -> Option<&dyn WriteEntry> {
        let hash = bloom_hash(dest_tcell);
        debug_assert!(hash.get() != 0, "bug in bloom_hash algorithm");
        if likely!(self.contained(hash) == Contained::No) {
            None
        } else {
            self.find_slow(dest_tcell)
        }
    }

    #[inline(never)]
    fn entry_slow<'a>(&'a mut self, dest_tcell: &TCellErased, hash: NonZeroUsize) -> Entry<'a> {
        match self
            .data
            .iter_mut()
            .find(|entry| entry.is_dest_tcell(dest_tcell))
        {
            // TODO: why does this need to be passed through a pointer first?
            Some(entry) => {
                stats::bloom_success_slow();
                stats::double_write();
                Entry::new_occupied(unsafe { mem::transmute(entry) }, hash)
            }
            None => {
                stats::bloom_failure();
                Entry::new_hash(self, hash)
            }
        }
    }

    // biased against finding the tcell
    #[inline]
    pub fn entry<'a>(&'a mut self, dest_tcell: &TCellErased) -> Entry<'a> {
        let hash = bloom_hash(dest_tcell);
        debug_assert!(hash.get() != 0, "bug in dumb_reference_hash algorithm");
        if likely!(self.contained(hash) == Contained::No) {
            Entry::new_hash(self, hash)
        } else {
            self.entry_slow(dest_tcell, hash)
        }
    }

    #[inline]
    pub fn next_push_allocates<T>(&self) -> bool {
        self.data.next_push_allocates::<WriteEntryImpl<T>>()
    }

    #[inline]
    pub unsafe fn push<T: 'static>(
        &mut self,
        dest_tcell: &TCellErased,
        val: T,
        hash: NonZeroUsize,
    ) {
        {
            let _ptr = dest_tcell as *const TCellErased;
            debug_assert!(
                self.data.iter().find(|x| x.is_dest_tcell(&*_ptr)).is_none(),
                "attempt to add `TCell` to the `WriteLog` twice"
            );
        }

        self.filter |= hash.get();
        self.data.push(WriteEntryImpl::new(dest_tcell, val));
    }

    #[inline]
    pub unsafe fn push_unchecked<T: 'static>(
        &mut self,
        dest_tcell: &TCellErased,
        val: T,
        hash: NonZeroUsize,
    ) {
        {
            let _ptr = dest_tcell as *const TCellErased;
            debug_assert!(
                self.data.iter().find(|x| x.is_dest_tcell(&*_ptr)).is_none(),
                "attempt to add `TCell` to the `WriteLog` twice"
            );
        }

        self.filter |= hash.get();
        self.data
            .push_unchecked(WriteEntryImpl::new(dest_tcell, val));
    }

    #[must_use]
    #[inline]
    pub fn try_lock_entries(&self, pin_epoch: QuiesceEpoch) -> bool {
        debug_assert!(!self.is_empty(), "attempt to lock empty write set");

        for entry in &self.data {
            unsafe {
                if unlikely!(!entry.try_lock(pin_epoch)) {
                    self.unlock_entries_until(entry);
                    return false;
                }
            }
        }
        true
    }

    #[inline(never)]
    #[cold]
    unsafe fn unlock_entries_until(&self, entry: &dyn WriteEntry) {
        let iter = self.data.iter().take_while(#[inline]
        move |&e| !ptr::eq(e, entry));
        for entry in iter {
            entry.unlock();
        }
    }

    #[inline]
    pub unsafe fn unlock_entries(&self) {
        for entry in &self.data {
            entry.unlock();
        }
    }

    #[inline]
    pub unsafe fn perform_writes(&self) {
        for entry in &self.data {
            entry.perform_write();
        }
    }

    #[inline]
    pub unsafe fn publish(&self, publish_epoch: QuiesceEpoch) {
        for entry in &self.data {
            entry.publish(publish_epoch);
        }
    }
}

pub enum Entry<'a> {
    Vacant {
        write_log: &'a mut WriteLog,
        hash:      NonZeroUsize,
    },
    Occupied {
        entry: DynElemMut<'a, dyn WriteEntry>,
        hash:  NonZeroUsize,
    },
}

impl<'a> Entry<'a> {
    #[inline]
    fn new_hash(write_log: &'a mut WriteLog, hash: NonZeroUsize) -> Self {
        Entry::Vacant { write_log, hash }
    }

    #[inline]
    fn new_occupied(entry: DynElemMut<'a, dyn WriteEntry>, hash: NonZeroUsize) -> Self {
        Entry::Occupied { entry, hash }
    }
}

#[inline]
const fn calc_shift() -> usize {
    (mem::align_of::<TCellErased>() > 1) as usize
        + (mem::align_of::<TCellErased>() > 2) as usize
        + (mem::align_of::<TCellErased>() > 4) as usize
        + (mem::align_of::<TCellErased>() > 8) as usize
        + 1 // In practice this +1 results in less failures, however it's not "correct". Any TCell with a
            // meaningful value happens to have a minimum size of mem::size_of::<usize>() + 1 which might
            // explain why the +1 is helpful for certain workloads.
}

#[inline]
pub fn bloom_hash(value: &TCellErased) -> NonZeroUsize {
    const SHIFT: usize = calc_shift();
    let raw_hash: usize = value as *const TCellErased as usize >> SHIFT;
    let result = 1 << (raw_hash & (mem::size_of::<NonZeroUsize>() * 8 - 1));
    unsafe { NonZeroUsize::new_unchecked(result) }
}
