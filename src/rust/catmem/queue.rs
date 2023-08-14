// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//======================================================================================================================
// Imports
//======================================================================================================================

use crate::{
    catmem::{
        ring::{
            Ring,
            MAX_RETRIES_PUSH_EOF,
        },
        QMode,
    },
    runtime::{
        fail::Fail,
        limits,
        memory::DemiBuffer,
        queue::IoQueue,
        QToken,
        QType,
    },
    scheduler::{
        TaskHandle,
        Yielder,
        YielderHandle,
    },
};
use ::std::{
    cell::{
        RefCell,
        RefMut,
    },
    collections::HashMap,
    rc::Rc,
};

//======================================================================================================================
// Structures
//======================================================================================================================

/// This structure contains code for manipulating a single, Catmem-specific Demikernel queue. Queue state is kept in
/// the [ring] structure, while [pending_ops] holds the map of TaskHandles and YielderHandle for currently active async
/// functions.
#[derive(Clone)]
pub struct CatmemQueue {
    ring: Rc<RefCell<Ring>>,
    pending_ops: Rc<RefCell<HashMap<TaskHandle, YielderHandle>>>,
}

//======================================================================================================================
// Associated Functions
//======================================================================================================================

impl CatmemQueue {
    /// This function creates a new CatmemQueue and a new shared ring buffer and connects to it to either the consumer
    /// or producer end indicated by [mode].
    pub fn create(name: &str, mode: QMode) -> Result<Self, Fail> {
        let pending_ops: Rc<RefCell<HashMap<TaskHandle, YielderHandle>>> =
            Rc::new(RefCell::<HashMap<TaskHandle, YielderHandle>>::new(HashMap::<
                TaskHandle,
                YielderHandle,
            >::new()));
        match mode {
            QMode::Push => Ok(Self {
                ring: Rc::new(RefCell::<Ring>::new(Ring::create_push_ring(name)?)),
                pending_ops,
            }),
            QMode::Pop => Ok(Self {
                ring: Rc::new(RefCell::<Ring>::new(Ring::create_pop_ring(name)?)),
                pending_ops,
            }),
        }
    }

    /// This function creates a new CatmemQueue and attaches to an existing share ring buffer as either a consumer or
    /// producer as indicated by [mode].
    pub fn open(name: &str, mode: QMode) -> Result<Self, Fail> {
        let pending_ops: Rc<RefCell<HashMap<TaskHandle, YielderHandle>>> =
            Rc::new(RefCell::<HashMap<TaskHandle, YielderHandle>>::new(HashMap::<
                TaskHandle,
                YielderHandle,
            >::new()));
        match mode {
            QMode::Push => Ok(Self {
                ring: Rc::new(RefCell::<Ring>::new(Ring::open_push_ring(name)?)),
                pending_ops,
            }),
            QMode::Pop => Ok(Self {
                ring: Rc::new(RefCell::<Ring>::new(Ring::open_pop_ring(name)?)),
                pending_ops,
            }),
        }
    }

    pub fn shutdown(&mut self) -> Result<(), Fail> {
        {
            let mut ring: RefMut<Ring> = self.ring.borrow_mut();
            ring.prepare_close()?;
            ring.commit();
            ring.prepare_closed()?;
            ring.commit();
        }
        self.cancel_pending_ops(Fail::new(libc::ECANCELED, "this queue was shutdown"));

        Ok(())
    }

    /// This function closes a ring endpoint.
    /// TODO merge this with async_close().
    pub fn close(&mut self) -> Result<(), Fail> {
        {
            let mut ring: RefMut<Ring> = self.ring.borrow_mut();
            ring.prepare_close()?;
            ring.close()?;
            ring.commit();
            ring.prepare_closed()?;
            ring.commit();
        }
        self.cancel_pending_ops(Fail::new(libc::ECANCELED, "this queue was closed"));
        Ok(())
    }

    /// Start an asynchronous coroutine to close this queue. This function contains all of the single-queue,
    /// asynchronous code necessary to run a close and any single-queue functionality after the close completes.
    pub fn async_close<F>(&self, insert_coroutine: F) -> Result<QToken, Fail>
    where
        F: FnOnce(Yielder) -> Result<TaskHandle, Fail>,
    {
        self.ring.borrow_mut().prepare_close()?;
        self.do_generic_sync_control_path_call(insert_coroutine, false)
    }

    /// This function perms an async close on the target queue.
    pub async fn do_async_close(&self, yielder: Yielder) -> Result<(), Fail> {
        for _ in 0..MAX_RETRIES_PUSH_EOF {
            if let Ok(_) = self.ring.borrow_mut().try_close() {
                return Ok(());
            }
            if let Err(cause) = yielder.yield_once().await {
                return Err(cause);
            }
        }
        // We ran out of retries, thus fail.
        let cause: String = format!("failed to push EoF");
        error!("push_eof(): {}", cause);
        Err(Fail::new(libc::EIO, &cause))
    }

    /// This private function tries to pop from the queue and is mostly used for scoping the borrow.
    fn try_pop(&self) -> Result<(Option<u8>, bool), Fail> {
        match &mut *self.ring.borrow_mut() {
            Ring::PushOnly(_) => {
                let cause: &String = &format!("Cannot pop from push-only queue");
                error!("{}", &cause);
                Err(Fail::new(libc::EINVAL, cause))
            },
            Ring::PopOnly(ring) => {
                let (byte, eof) = ring.try_pop()?;
                if eof {
                    ring.prepare_close()?;
                    ring.commit();
                }
                Ok((byte, eof))
            },
        }
    }

    /// Schedule a coroutine to pop from this queue. This function contains all of the single-queue,
    /// asynchronous code necessary to pop a buffer and any single-queue functionality after the pop completes.
    pub fn pop<F: FnOnce(Yielder) -> Result<TaskHandle, Fail>>(&self, insert_coroutine: F) -> Result<QToken, Fail> {
        self.do_generic_sync_data_path_call(insert_coroutine)
    }

    /// This function pops a buffer of optional [size] from the queue. If the queue is connected to the push end of a
    /// shared memory ring, this function returns an error.
    pub async fn do_pop(&self, size: Option<usize>, yielder: Yielder) -> Result<(DemiBuffer, bool), Fail> {
        let size: usize = size.unwrap_or(limits::RECVBUF_SIZE_MAX);
        let mut buf: DemiBuffer = DemiBuffer::new(size as u16);
        let mut index: usize = 0;
        let eof: bool = loop {
            match self.try_pop()? {
                (Some(byte), eof) => {
                    if eof {
                        // If eof, then trim everything that we have received so far and return.
                        buf.trim(size - index)
                            .expect("cannot trim more bytes than the buffer has");
                        break true;
                    } else {
                        // If not eof, add byte to buffer.
                        buf[index] = byte;
                        index += 1;

                        // Check if we read enough bytes.
                        if index >= size {
                            // If so, trim buffer to length.
                            buf.trim(size - index)
                                .expect("cannot trim more bytes than the buffer has");
                            break false;
                        }
                    }
                },
                (None, _) => {
                    if index > 0 {
                        buf.trim(size - index)
                            .expect("cannot trim more bytes than the buffer has");
                        break false;
                    } else {
                        // Operation in progress. Check if cancelled.
                        match yielder.yield_once().await {
                            Ok(()) => continue,
                            Err(cause) => return Err(cause),
                        }
                    }
                },
            }
        };
        trace!("data read ({:?}/{:?} bytes, eof={:?})", buf.len(), size, eof);
        Ok((buf, eof))
    }

    /// Schedule a coroutine to push to this queue. This function contains all of the single-queue,
    /// asynchronous code necessary to run push a buffer and any single-queue functionality after the push completes.
    pub fn push<F: FnOnce(Yielder) -> Result<TaskHandle, Fail>>(&self, insert_coroutine: F) -> Result<QToken, Fail> {
        self.do_generic_sync_data_path_call(insert_coroutine)
    }

    /// This private function tries to push a single byte and is used for scoping the borrow.
    fn try_push(&self, byte: &u8) -> Result<bool, Fail> {
        match &mut *self.ring.borrow_mut() {
            Ring::PushOnly(ring) => Ok(ring.try_push(byte)?),
            Ring::PopOnly(_) => {
                let cause: &String = &format!("Cannot push to a pop-only queue");
                error!("{}", &cause);
                Err(Fail::new(libc::EINVAL, cause))
            },
        }
    }

    /// This function tries to push [buf] to the shared memory ring. If the queue is connected to the pop end, then
    /// this function returns an error.
    pub async fn do_push(&self, buf: DemiBuffer, yielder: Yielder) -> Result<(), Fail> {
        for byte in &buf[..] {
            loop {
                match self.try_push(byte)? {
                    true => break,
                    false => {
                        // Operation not completed. Check if it was cancelled.
                        match yielder.yield_once().await {
                            Ok(()) => continue,
                            Err(cause) => return Err(cause),
                        }
                    },
                }
            }
        }
        trace!("data written ({:?}/{:?} bytes)", buf.len(), buf.len());
        Ok(())
    }

    /// Generic function for spawning a control-path coroutine on [self].
    fn do_generic_sync_control_path_call<F>(&self, coroutine: F, add_as_pending_op: bool) -> Result<QToken, Fail>
    where
        F: FnOnce(Yielder) -> Result<TaskHandle, Fail>,
    {
        // Spawn coroutine.
        let yielder: Yielder = Yielder::new();
        let yielder_handle: YielderHandle = yielder.get_handle();
        let task_handle: TaskHandle = match coroutine(yielder) {
            // We successfully spawned the coroutine.
            Ok(handle) => {
                // Commit the operation on the socket.
                self.ring.borrow_mut().commit();
                handle
            },
            // We failed to spawn the coroutine.
            Err(e) => {
                // Abort the operation on the socket.
                self.ring.borrow_mut().abort();
                return Err(e);
            },
        };

        // If requested, add this operation to the list of pending operations on this queue.
        if add_as_pending_op {
            self.add_pending_op(&task_handle, &yielder_handle);
        }

        Ok(task_handle.get_task_id().into())
    }

    /// Generic function for spawning a data-path coroutine on [self].
    fn do_generic_sync_data_path_call<F>(&self, coroutine: F) -> Result<QToken, Fail>
    where
        F: FnOnce(Yielder) -> Result<TaskHandle, Fail>,
    {
        let yielder: Yielder = Yielder::new();
        let yielder_handle: YielderHandle = yielder.get_handle();
        let task_handle: TaskHandle = coroutine(yielder)?;
        self.add_pending_op(&task_handle, &yielder_handle);
        Ok(task_handle.get_task_id().into())
    }

    /// Adds a new operation to the list of pending operations on this queue.
    pub fn add_pending_op(&self, handle: &TaskHandle, yielder_handle: &YielderHandle) {
        self.pending_ops
            .borrow_mut()
            .insert(handle.clone(), yielder_handle.clone());
    }

    /// Removes an operation from the list of pending operations on this queue.
    pub fn remove_pending_op(&self, handle: &TaskHandle) {
        self.pending_ops
            .borrow_mut()
            .remove_entry(handle)
            .expect("operation should be registered");
    }

    /// Cancels all pending operations on this queue.
    pub fn cancel_pending_ops(&mut self, cause: Fail) {
        for (handle, mut yielder_handle) in self.pending_ops.borrow_mut().drain() {
            if !handle.has_completed() {
                yielder_handle.wake_with(Err(cause.clone()));
            }
        }
    }
}

//======================================================================================================================
// Trait implementation
//======================================================================================================================

impl IoQueue for CatmemQueue {
    fn get_qtype(&self) -> QType {
        QType::MemoryQueue
    }
}
