// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//==============================================================================
// Imports
//==============================================================================

use crate::demikernel::dbuf::DataBuffer;
use ::arrayvec::ArrayVec;
use ::catwalk::{
    Scheduler,
    SchedulerFuture,
    SchedulerHandle,
};
use ::libc::c_void;
use ::rand::{
    distributions::Standard,
    prelude::Distribution,
};
use ::runtime::{
    fail::Fail,
    memory::{
        Buffer,
        MemoryRuntime,
    },
    network::{
        config::{
            ArpConfig,
            TcpConfig,
            UdpConfig,
        },
        consts::RECEIVE_BATCH_SIZE,
        types::MacAddress,
        NetworkRuntime,
    },
    task::SchedulerRuntime,
    timer::{
        Timer,
        TimerRc,
        WaitFuture,
    },
    types::{
        dmtr_sgarray_t,
        dmtr_sgaseg_t,
    },
    utils::UtilsRuntime,
    Runtime,
};
use ::std::{
    mem,
    net::Ipv4Addr,
    ptr,
    rc::Rc,
    slice,
    time::Instant,
};

//==============================================================================
// Structures
//==============================================================================

/// POSIX Runtime
#[derive(Clone)]
pub struct PosixRuntime {
    /// Timer.
    timer: TimerRc,
    /// Scheduler
    scheduler: Scheduler,
}

//==============================================================================
// Associate Functions
//==============================================================================

/// Associate Functions for POSIX Runtime
impl PosixRuntime {
    pub fn new(now: Instant) -> Self {
        Self {
            timer: TimerRc(Rc::new(Timer::new(now))),
            scheduler: Scheduler::default(),
        }
    }
}

//==============================================================================
// Trait Implementations
//==============================================================================

/// Memory Runtime Trait Implementation for POSIX Runtime
impl MemoryRuntime for PosixRuntime {
    /// Memory Buffer
    type Buf = DataBuffer;

    /// Converts a runtime buffer into a scatter-gather array.
    fn into_sgarray(&self, dbuf: DataBuffer) -> Result<dmtr_sgarray_t, Fail> {
        let len: usize = dbuf.len();
        let dbuf_ptr: *const [u8] = DataBuffer::into_raw(dbuf)?;
        let sgaseg: dmtr_sgaseg_t = dmtr_sgaseg_t {
            sgaseg_buf: dbuf_ptr as *mut c_void,
            sgaseg_len: len as u32,
        };
        Ok(dmtr_sgarray_t {
            sga_buf: ptr::null_mut(),
            sga_numsegs: 1,
            sga_segs: [sgaseg],
            sga_addr: unsafe { mem::zeroed() },
        })
    }

    /// Allocates a scatter-gather array.
    fn alloc_sgarray(&self, size: usize) -> Result<dmtr_sgarray_t, Fail> {
        // Allocate a heap-managed buffer.
        let dbuf: DataBuffer = DataBuffer::new(size)?;
        let dbuf_ptr: *const [u8] = DataBuffer::into_raw(dbuf)?;
        let sgaseg: dmtr_sgaseg_t = dmtr_sgaseg_t {
            sgaseg_buf: dbuf_ptr as *mut c_void,
            sgaseg_len: size as u32,
        };
        Ok(dmtr_sgarray_t {
            sga_buf: ptr::null_mut(),
            sga_numsegs: 1,
            sga_segs: [sgaseg],
            sga_addr: unsafe { mem::zeroed() },
        })
    }

    /// Releases a scatter-gather array.
    fn free_sgarray(&self, sga: dmtr_sgarray_t) -> Result<(), Fail> {
        // Check arguments.
        // TODO: Drop this check once we support scatter-gather arrays with multiple segments.
        if sga.sga_numsegs != 1 {
            return Err(Fail::new(libc::EINVAL, "scatter-gather array with invalid size"));
        }

        // Release heap-managed buffer.
        let sgaseg: dmtr_sgaseg_t = sga.sga_segs[0];
        let (data_ptr, length): (*mut u8, usize) = (sgaseg.sgaseg_buf as *mut u8, sgaseg.sgaseg_len as usize);

        // Convert back raw slice to a heap buffer and drop allocation.
        DataBuffer::from_raw_parts(data_ptr, length)?;

        Ok(())
    }

    /// Clones a scatter-gather array.
    fn clone_sgarray(&self, sga: &dmtr_sgarray_t) -> Result<DataBuffer, Fail> {
        // Check arguments.
        // TODO: Drop this check once we support scatter-gather arrays with multiple segments.
        if sga.sga_numsegs != 1 {
            return Err(Fail::new(libc::EINVAL, "scatter-gather array with invalid size"));
        }

        let sgaseg: dmtr_sgaseg_t = sga.sga_segs[0];
        let (ptr, len): (*mut c_void, usize) = (sgaseg.sgaseg_buf, sgaseg.sgaseg_len as usize);

        // Clone heap-managed buffer.
        Ok(DataBuffer::from_slice(unsafe {
            slice::from_raw_parts(ptr as *const u8, len)
        }))
    }
}

/// Scheduler Runtime Trait Implementation for POSIX Runtime
impl SchedulerRuntime for PosixRuntime {
    type WaitFuture = WaitFuture<TimerRc>;

    /// Creates a future on which one should wait a given duration.
    fn wait(&self, duration: std::time::Duration) -> Self::WaitFuture {
        let now: Instant = self.timer.now();
        self.timer.wait_until(self.timer.clone(), now + duration)
    }

    /// Creates a future on which one should until a given point in time.
    fn wait_until(&self, when: std::time::Instant) -> Self::WaitFuture {
        self.timer.wait_until(self.timer.clone(), when)
    }

    /// Returns the current runtime clock.
    fn now(&self) -> std::time::Instant {
        self.timer.now()
    }

    /// Advances the runtime clock to some point in time.
    fn advance_clock(&self, now: Instant) {
        self.timer.advance_clock(now);
    }

    /// Spawns a new task.
    fn spawn<F: SchedulerFuture>(&self, future: F) -> SchedulerHandle {
        self.scheduler.insert(future)
    }

    /// Schedules a task for execution.
    fn schedule<F: SchedulerFuture>(&self, future: F) -> SchedulerHandle {
        self.scheduler.insert(future)
    }

    /// Gets the handle of a task.
    fn get_handle(&self, key: u64) -> Option<SchedulerHandle> {
        self.scheduler.from_raw_handle(key)
    }

    /// Takes out a given task from the scheduler.
    fn take(&self, handle: SchedulerHandle) -> Box<dyn SchedulerFuture> {
        self.scheduler.take(handle)
    }

    /// Polls the scheduler.
    fn poll(&self) {
        self.scheduler.poll()
    }
}

/// Network Runtime Trait Implementation for POSIX Runtime
impl NetworkRuntime for PosixRuntime {
    // TODO: Rely on a default implementation for this.
    fn transmit(&self, _pkt: impl runtime::network::PacketBuf<Self::Buf>) {
        unreachable!()
    }

    // TODO: Rely on a default implementation for this.
    fn receive(&self) -> ArrayVec<Self::Buf, RECEIVE_BATCH_SIZE> {
        unreachable!()
    }

    // TODO: Rely on a default implementation for this.
    fn local_link_addr(&self) -> MacAddress {
        unreachable!()
    }

    // TODO: Rely on a default implementation for this.
    fn local_ipv4_addr(&self) -> Ipv4Addr {
        unreachable!()
    }

    // TODO: Rely on a default implementation for this.
    fn arp_options(&self) -> ArpConfig {
        unreachable!()
    }

    // TODO: Rely on a default implementation for this.
    fn tcp_options(&self) -> TcpConfig {
        unreachable!()
    }

    // TODO: Rely on a default implementation for this.
    fn udp_options(&self) -> UdpConfig {
        unreachable!()
    }
}

/// Utilities Runtime Trait Implementation for POSIX Runtime
impl UtilsRuntime for PosixRuntime {
    // TODO: Rely on a default implementation for this.
    fn rng_gen<T>(&self) -> T
    where
        Standard: Distribution<T>,
    {
        unreachable!()
    }

    // TODO: Rely on a default implementation for this.
    fn rng_shuffle<T>(&self, _slice: &mut [T]) {
        unreachable!()
    }
}

/// Runtime Trait Implementation for POSIX Runtime
impl Runtime for PosixRuntime {}