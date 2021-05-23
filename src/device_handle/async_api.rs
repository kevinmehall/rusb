use crate::{DeviceHandle, UsbContext};
use libusb1_sys as ffi;

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::convert::TryInto;
use std::ptr::NonNull;
use std::time::{Instant, Duration};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum AsyncError {
    #[error("No transfers pending")]
    NoTransfersPending,
    #[error("Poll timed out")]
    PollTimeout,
    #[error("Transfer is stalled")]
    Stall,
    #[error("Device was disconnected")]
    Disconnected,
    #[error("Device sent more data than expected")]
    Overflow,
    #[error("Other Error: {0}")]
    Other(&'static str),
    #[error("{0}ERRNO: {1}")]
    Errno(&'static str, i32),
    #[error("Transfer was cancelled")]
    Cancelled,
}

struct AsyncTransfer {
    ptr: NonNull<ffi::libusb_transfer>,
    buffer: Vec<u8>,
}

impl AsyncTransfer {
    // Invariant: Caller must ensure `device` outlives this transfer
    unsafe fn new_bulk(
        device: *mut ffi::libusb_device_handle,
        endpoint: u8,
        buffer: Vec<u8>,
    ) -> Self {
        // non-isochronous endpoints (e.g. control, bulk, interrupt) specify a value of 0
        // This is step 1 of async API
        let ptr = ffi::libusb_alloc_transfer(0);
        let ptr = NonNull::new(ptr).expect("Could not allocate transfer!");

        let user_data = Box::into_raw(Box::new(AtomicBool::new(false))).cast::<libc::c_void>();

        let length = if endpoint & ffi::constants::LIBUSB_ENDPOINT_DIR_MASK == ffi::constants::LIBUSB_ENDPOINT_OUT {
            // for OUT endpoints: the currently valid data in the buffer
            buffer.len()
        } else {
            // for IN endpoints: the full capacity
            buffer.capacity()
        }.try_into().unwrap();

        ffi::libusb_fill_bulk_transfer(
            ptr.as_ptr(),
            device,
            endpoint,
            buffer.as_ptr() as *mut u8,
            length,
            Self::transfer_cb,
            user_data,
            0,
        );

        Self { ptr, buffer }
    }

    // Part of step 4 of async API the transfer is finished being handled when
    // `poll()` is called.
    extern "system" fn transfer_cb(transfer: *mut ffi::libusb_transfer) {
        // Safety: transfer is still valid because libusb just completed
        // it but we haven't told anyone yet. user_data remains valid
        // because it is freed only with the transfer.
        // After the store to completed, these may no longer be valid if
        // the polling thread freed it after seeing it completed.
        let completed = unsafe { 
            let transfer = &mut *transfer;
            &*transfer.user_data.cast::<AtomicBool>()
        };
        completed.store(true, SeqCst);
    }

    fn transfer(&self) -> &ffi::libusb_transfer {
        // Safety: transfer remains valid as long as self
        unsafe { self.ptr.as_ref() }
    }

    fn completed_flag(&self) -> &AtomicBool {
        // Safety: transfer and user_data remain valid as long as self
        unsafe {  &*self.transfer().user_data.cast::<AtomicBool>() }
    }

    /// Prerequisite: self.buffer ans self.ptr are both correctly set
    fn swap_buffer(&mut self, new_buf: Vec<u8>) -> Vec<u8> {
        let transfer_struct = unsafe { self.ptr.as_mut() };

        let data = std::mem::replace(&mut self.buffer, new_buf);

        // Update transfer struct for new buffer
        transfer_struct.actual_length = 0; // TODO: Is this necessary?
        transfer_struct.buffer = self.buffer.as_mut_ptr();
        transfer_struct.length = self.buffer.capacity() as i32;

        data
    }

    // Step 3 of async API
    fn submit(&mut self) -> Result<(), AsyncError> {
        self.completed_flag().store(false, SeqCst);
        let errno = unsafe { ffi::libusb_submit_transfer(self.ptr.as_ptr()) };

        use ffi::constants::*;
        use AsyncError as E;
        match errno {
            0 => Ok(()),
            LIBUSB_ERROR_NO_DEVICE => Err(E::Disconnected),
            LIBUSB_ERROR_BUSY => {
                unreachable!("We shouldn't be calling submit on transfers already submitted!")
            }
            LIBUSB_ERROR_NOT_SUPPORTED => Err(E::Other("Transfer not supported")),
            LIBUSB_ERROR_INVALID_PARAM => Err(E::Other("Transfer size bigger than OS supports")),
            _ => Err(E::Errno("Error while submitting transfer: ", errno)),
        }
    }

    fn cancel(&mut self) {
        unsafe {
            ffi::libusb_cancel_transfer(self.ptr.as_ptr());
        }
    }

    fn handle_completed(&mut self) -> Result<Vec<u8>, AsyncError> {
        assert!(self.completed_flag().load(std::sync::atomic::Ordering::Relaxed));
        use ffi::constants::*;
        let err = match self.transfer().status {
            LIBUSB_TRANSFER_COMPLETED => {
                debug_assert!(self.transfer().length >= self.transfer().actual_length);
                unsafe {
                    self.buffer.set_len(self.transfer().actual_length as usize);
                }
                let data = self.swap_buffer(Vec::new());
                return Ok(data)
            },
            LIBUSB_TRANSFER_CANCELLED => AsyncError::Cancelled,
            LIBUSB_TRANSFER_ERROR => AsyncError::Other("Error occurred during transfer execution"),
            LIBUSB_TRANSFER_TIMED_OUT => {
                unreachable!("We are using timeout=0 which means no timeout")
            }
            LIBUSB_TRANSFER_STALL => AsyncError::Stall,
            LIBUSB_TRANSFER_NO_DEVICE => AsyncError::Disconnected,
            LIBUSB_TRANSFER_OVERFLOW => AsyncError::Overflow,
            _ => panic!("Found an unexpected error value for transfer status"),
        };
        Err(err)
    }
}

/// Invariant: transfer must not be pending
impl Drop for AsyncTransfer {
    fn drop(&mut self) {
        unsafe {
            drop(Box::from_raw(self.transfer().user_data));
            ffi::libusb_free_transfer(self.ptr.as_ptr());
        }
    }
}

/// Represents a pool of asynchronous transfers, that can be polled to completion
pub struct AsyncPool<C: UsbContext> {
    device: Arc<DeviceHandle<C>>,
    endpoint: u8,
    pending: VecDeque<AsyncTransfer>,
}
impl<C: UsbContext> AsyncPool<C> {
    pub fn new_bulk(
        device: Arc<DeviceHandle<C>>,
        endpoint: u8,
    ) -> Result<Self, AsyncError> {
        Ok(Self {
            device,
            endpoint,
            pending: VecDeque::new(),
        })
    }

    pub fn submit(&mut self, buf: Vec<u8>) -> Result<(), AsyncError> {
        // Safety: If transfer is submitted, it is pushed onto `pending` where it will be
        // dropped before `device` is freed.
        unsafe {
            let mut transfer = AsyncTransfer::new_bulk(self.device.as_raw(), self.endpoint, buf);
            transfer.submit()?;
            self.pending.push_back(transfer);
            Ok(())
        }
    }

    pub fn poll(
        &mut self,
        timeout: Duration,
    ) -> Result<Vec<u8>, AsyncError> {
        let next = self.pending.front().ok_or(AsyncError::NoTransfersPending)?;
        if poll_completed(self.device.context(), timeout, next.completed_flag()) {
            let mut transfer = self.pending.pop_front().unwrap();
            let res = transfer.handle_completed();
            res
        } else {
            Err(AsyncError::PollTimeout)
        }
    }

    pub fn cancel_all(&mut self) {
        // Cancel in reverse order to avoid a race condition in which one
        // transfer is cancelled but another submitted later makes its way onto
        // the bus.
        for transfer in self.pending.iter_mut().rev() {
            transfer.cancel();
        }
    }

    /// Returns the number of async transfers pending
    pub fn pending(&self) -> usize {
        self.pending.len()
    }
}

unsafe impl<C: UsbContext> Send for AsyncPool<C> {}
unsafe impl<C: UsbContext> Sync for AsyncPool<C> {}

impl<C:UsbContext> Drop for AsyncPool<C> {
    fn drop(&mut self) {
        self.cancel_all();
        while self.pending() > 0 {
            self.poll(Duration::from_secs(1)).ok();
        }
    }
}


/// This is effectively libusb_handle_events_timeout_completed, but with
/// `completed` as `AtomicBool` instead of `c_int` so it is safe to access
/// without the events lock held. It also continues polling until completion,
/// timeout, or error, instead of potentially returning early.
///
/// This design is based on
/// https://libusb.sourceforge.io/api-1.0/libusb_mtasync.html#threadwait
///
/// Returns `true` when `completed` becomes true, `false` on timeout, and panics on
/// any other libusb error.
fn poll_completed(ctx: &impl UsbContext, timeout: Duration, completed: &AtomicBool) -> bool {
    use ffi::{*, constants::*};

    let deadline = Instant::now() + timeout;

    unsafe {
        let mut err = 0;
        while err == 0 && !completed.load(SeqCst) && deadline > Instant::now() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let timeval = libc::timeval {
                tv_sec: remaining.as_secs().try_into().unwrap(),
                tv_usec: remaining.subsec_micros().try_into().unwrap(),
            };

            if libusb_try_lock_events(ctx.as_raw()) == 0 {
                if !completed.load(SeqCst) && libusb_event_handling_ok(ctx.as_raw()) != 0 {
                    err = libusb_handle_events_locked(ctx.as_raw(), &timeval as *const _);
                }
                libusb_unlock_events(ctx.as_raw());
            } else {
                libusb_lock_event_waiters(ctx.as_raw());
                if !completed.load(SeqCst) && libusb_event_handler_active(ctx.as_raw()) != 0 {
                    libusb_wait_for_event(ctx.as_raw(), &timeval as *const _);
                }
                libusb_unlock_event_waiters(ctx.as_raw());
            }
        }

        match err {
            0 => completed.load(SeqCst),
            LIBUSB_ERROR_TIMEOUT => false,
            _ => panic!(
                    "Error {} when polling transfers: {}",
                    err,
                    std::ffi::CStr::from_ptr(ffi::libusb_strerror(err)).to_string_lossy()
                )
        }
    }    
}

