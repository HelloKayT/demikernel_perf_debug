// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

mod futures;
mod iouring;
mod queue;
mod runtime;

//======================================================================================================================
// Exports
//======================================================================================================================

pub use self::{
    queue::CatcollarQueue,
    runtime::IoUringRuntime,
};

//======================================================================================================================
// Imports
//======================================================================================================================

use self::futures::{
    accept::accept_coroutine,
    close::close_coroutine,
    connect::connect_coroutine,
    pop::pop_coroutine,
    push::push_coroutine,
    pushto::pushto_coroutine,
};
use crate::{
    demikernel::config::Config,
    pal::{
        constants::SOMAXCONN,
        data_structures::{
            SockAddr,
            SockAddrIn,
            Socklen,
        },
        linux,
    },
    runtime::{
        fail::Fail,
        limits,
        memory::{
            DemiBuffer,
            MemoryRuntime,
        },
        queue::{
            IoQueueTable,
            Operation,
            OperationResult,
            OperationTask,
            QDesc,
            QToken,
            QType,
        },
        types::{
            demi_accept_result_t,
            demi_opcode_t,
            demi_qr_value_t,
            demi_qresult_t,
            demi_sgarray_t,
        },
    },
    scheduler::{
        TaskHandle,
        Yielder,
    },
};
use ::std::{
    cell::{
        RefCell,
        RefMut,
    },
    mem,
    net::SocketAddrV4,
    os::unix::prelude::RawFd,
    pin::Pin,
    rc::Rc,
};

//======================================================================================================================
// Structures
//======================================================================================================================

/// Catcollar LibOS
pub struct CatcollarLibOS {
    /// Table of queue descriptors.
    qtable: Rc<RefCell<IoQueueTable<CatcollarQueue>>>, // TODO: Move this into runtime module.
    /// Underlying runtime.
    runtime: IoUringRuntime,
}

//======================================================================================================================
// Associated Functions
//======================================================================================================================

/// Associate Functions for Catcollar LibOS
impl CatcollarLibOS {
    /// Instantiates a Catcollar LibOS.
    pub fn new(_config: &Config) -> Self {
        let qtable: Rc<RefCell<IoQueueTable<CatcollarQueue>>> =
            Rc::new(RefCell::new(IoQueueTable::<CatcollarQueue>::new()));
        let runtime: IoUringRuntime = IoUringRuntime::new();
        Self { qtable, runtime }
    }

    /// Creates a socket.
    pub fn socket(&mut self, domain: libc::c_int, typ: libc::c_int, _protocol: libc::c_int) -> Result<QDesc, Fail> {
        trace!("socket() domain={:?}, type={:?}, protocol={:?}", domain, typ, _protocol);

        // Parse communication domain.
        if domain != libc::AF_INET {
            return Err(Fail::new(libc::ENOTSUP, "communication domain not supported"));
        }

        // Parse socket type and protocol.
        if (typ != libc::SOCK_STREAM) && (typ != libc::SOCK_DGRAM) {
            return Err(Fail::new(libc::ENOTSUP, "socket type not supported"));
        }

        // Create socket.
        match unsafe { libc::socket(domain, typ, 0) } {
            fd if fd >= 0 => {
                let qtype: QType = match typ {
                    libc::SOCK_STREAM => QType::TcpSocket,
                    libc::SOCK_DGRAM => QType::UdpSocket,
                    _ => return Err(Fail::new(libc::ENOTSUP, "socket type not supported")),
                };

                // Set socket options.
                unsafe {
                    if typ == libc::SOCK_STREAM {
                        if linux::set_tcp_nodelay(fd) != 0 {
                            let errno: libc::c_int = *libc::__errno_location();
                            warn!("cannot set TCP_NONDELAY option (errno={:?})", errno);
                        }
                    }
                    if linux::set_nonblock(fd) != 0 {
                        let errno: libc::c_int = *libc::__errno_location();
                        warn!("cannot set O_NONBLOCK option (errno={:?})", errno);
                    }
                    if linux::set_so_reuseport(fd) != 0 {
                        let errno: libc::c_int = *libc::__errno_location();
                        warn!("cannot set SO_REUSEPORT option (errno={:?})", errno);
                    }
                }

                trace!("socket: {:?}, domain: {:?}, typ: {:?}", fd, domain, typ);
                let mut queue: CatcollarQueue = CatcollarQueue::new(qtype);
                queue.set_fd(fd);
                Ok(self.qtable.borrow_mut().alloc(queue))
            },
            _ => {
                let errno: libc::c_int = unsafe { *libc::__errno_location() };
                Err(Fail::new(errno, "failed to create socket"))
            },
        }
    }

    /// Binds a socket to a local endpoint.
    pub fn bind(&mut self, qd: QDesc, local: SocketAddrV4) -> Result<(), Fail> {
        trace!("bind() qd={:?}, local={:?}", qd, local);
        let mut qtable: RefMut<IoQueueTable<CatcollarQueue>> = self.qtable.borrow_mut();
        // Check if we are binding to the wildcard port.
        if local.port() == 0 {
            let cause: String = format!("cannot bind to port 0 (qd={:?})", qd);
            error!("bind(): {}", cause);
            return Err(Fail::new(libc::ENOTSUP, &cause));
        }

        // Check if queue descriptor is valid.
        if qtable.get(&qd).is_none() {
            let cause: String = format!("invalid queue descriptor {:?}", qd);
            error!("bind(): {}", &cause);
            return Err(Fail::new(libc::EBADF, &cause));
        }

        // Check wether the address is in use.
        for (_, queue) in qtable.get_values() {
            if let Some(addr) = queue.get_addr() {
                if addr == local {
                    let cause: String = format!("address is already bound to a socket (qd={:?}", qd);
                    error!("bind(): {}", &cause);
                    return Err(Fail::new(libc::EADDRINUSE, &cause));
                }
            }
        }

        // Get a mutable reference to the queue table.
        // It is safe to unwrap because we checked before that the queue descriptor is valid.
        let queue: &mut CatcollarQueue = qtable.get_mut(&qd).expect("queue descriptor should be in queue table");

        // Get reference to the underlying file descriptor.
        // It is safe to unwrap because when creating a queue we assigned it a valid file descritor.
        let fd: RawFd = queue.get_fd().expect("queue should have a file descriptor");

        // Bind underlying socket.
        let saddr: SockAddr = linux::socketaddrv4_to_sockaddr(&local);
        match unsafe { libc::bind(fd, &saddr as *const SockAddr, mem::size_of::<SockAddrIn>() as Socklen) } {
            stats if stats == 0 => {
                queue.set_addr(local);
                Ok(())
            },
            _ => {
                let errno: libc::c_int = unsafe { *libc::__errno_location() };
                error!("failed to bind socket (errno={:?})", errno);
                Err(Fail::new(errno, "operation failed"))
            },
        }
    }

    /// Sets a socket as a passive one.
    pub fn listen(&mut self, qd: QDesc, backlog: usize) -> Result<(), Fail> {
        trace!("listen() qd={:?}, backlog={:?}", qd, backlog);

        // We just assert backlog here, because it was previously checked at PDPIX layer.
        debug_assert!((backlog > 0) && (backlog <= SOMAXCONN as usize));

        // Issue listen operation.
        match self.qtable.borrow().get(&qd) {
            Some(queue) => match queue.get_fd() {
                Some(fd) => {
                    if unsafe { libc::listen(fd, backlog as i32) } != 0 {
                        let errno: libc::c_int = unsafe { *libc::__errno_location() };
                        error!("failed to listen ({:?})", errno);
                        return Err(Fail::new(errno, "operation failed"));
                    }
                    Ok(())
                },
                None => unreachable!("CatcollarQueue has invalid underlying file descriptor"),
            },
            None => Err(Fail::new(libc::EBADF, "invalid queue descriptor")),
        }
    }

    /// Accepts connections on a socket.
    pub fn accept(&mut self, qd: QDesc) -> Result<QToken, Fail> {
        trace!("accept(): qd={:?}", qd);

        let fd: RawFd = match self.qtable.borrow().get(&qd) {
            Some(queue) => match queue.get_fd() {
                Some(fd) => fd,
                None => unreachable!("CatcollarQueue has invalid underlying file descriptor"),
            },
            None => return Err(Fail::new(libc::EBADF, "invalid queue descriptor")),
        };

        // Issue accept operation.
        let yielder: Yielder = Yielder::new();
        let coroutine: Pin<Box<Operation>> = Box::pin(Self::do_accept(self.qtable.clone(), qd, fd, yielder));
        let task_id: String = format!("Catcollar::accept for qd={:?}", qd);
        let task: OperationTask = OperationTask::new(task_id, coroutine);
        let handle: TaskHandle = match self.runtime.scheduler.insert(task) {
            Some(handle) => handle,
            None => return Err(Fail::new(libc::EAGAIN, "cannot schedule co-routine")),
        };
        Ok(handle.get_task_id().into())
    }

    async fn do_accept(
        qtable: Rc<RefCell<IoQueueTable<CatcollarQueue>>>,
        qd: QDesc,
        fd: RawFd,
        yielder: Yielder,
    ) -> (QDesc, OperationResult) {
        // Borrow the queue table to either update the queue metadata or free the queue on error.
        match accept_coroutine(fd, yielder).await {
            Ok((new_fd, addr)) => {
                let mut queue: CatcollarQueue = CatcollarQueue::new(QType::TcpSocket);
                queue.set_addr(addr);
                queue.set_fd(new_fd);
                let new_qd: QDesc = qtable.borrow_mut().alloc(queue);
                (qd, OperationResult::Accept((new_qd, addr)))
            },
            Err(e) => (qd, OperationResult::Failed(e)),
        }
    }

    /// Establishes a connection to a remote endpoint.
    pub fn connect(&mut self, qd: QDesc, remote: SocketAddrV4) -> Result<QToken, Fail> {
        trace!("connect() qd={:?}, remote={:?}", qd, remote);

        // Issue connect operation.
        match self.qtable.borrow().get(&qd) {
            Some(queue) => match queue.get_fd() {
                Some(fd) => {
                    let yielder: Yielder = Yielder::new();
                    let coroutine: Pin<Box<Operation>> = Box::pin(Self::do_connect(qd, fd, remote, yielder));
                    let task_id: String = format!("Catcollar::connect for qd={:?}", qd);
                    let task: OperationTask = OperationTask::new(task_id, coroutine);
                    let handle: TaskHandle = match self.runtime.scheduler.insert(task) {
                        Some(handle) => handle,
                        None => return Err(Fail::new(libc::EAGAIN, "cannot schedule co-routine")),
                    };
                    Ok(handle.get_task_id().into())
                },
                None => unreachable!("CatcollarQueue has invalid underlying file descriptor"),
            },
            _ => Err(Fail::new(libc::EBADF, "invalid queue descriptor")),
        }
    }

    async fn do_connect(qd: QDesc, fd: RawFd, remote: SocketAddrV4, yielder: Yielder) -> (QDesc, OperationResult) {
        // Handle the result.
        match connect_coroutine(fd, remote, yielder).await {
            Ok(()) => (qd, OperationResult::Connect),
            Err(e) => (qd, OperationResult::Failed(e)),
        }
    }

    /// Closes a socket.
    pub fn close(&mut self, qd: QDesc) -> Result<(), Fail> {
        trace!("close() qd={:?}", qd);
        let mut qtable: RefMut<IoQueueTable<CatcollarQueue>> = self.qtable.borrow_mut();
        match qtable.get(&qd) {
            Some(queue) => match queue.get_fd() {
                Some(fd) => match unsafe { libc::close(fd) } {
                    stats if stats == 0 => (),
                    _ => {
                        let errno: libc::c_int = unsafe { *libc::__errno_location() };
                        error!("failed to close socket (fd={:?}, errno={:?})", fd, errno);
                        return Err(Fail::new(errno, "operation failed"));
                    },
                },
                None => unreachable!("CatcollarQueue has invalid underlying file descriptor"),
            },
            None => return Err(Fail::new(libc::EBADF, "invalid queue descriptor")),
        };
        qtable.free(&qd);
        Ok(())
    }

    /// Asynchronous close
    pub fn async_close(&mut self, qd: QDesc) -> Result<QToken, Fail> {
        trace!("close() qd={:?}", qd);

        match self.qtable.borrow().get(&qd) {
            Some(queue) => match queue.get_fd() {
                Some(fd) => {
                    let yielder: Yielder = Yielder::new();
                    let coroutine: Pin<Box<Operation>> = Box::pin(Self::do_close(self.qtable.clone(), qd, fd, yielder));
                    let task_id: String = format!("Catcollar::close for qd={:?}", qd);
                    let task: OperationTask = OperationTask::new(task_id, coroutine);
                    let handle: TaskHandle = match self.runtime.scheduler.insert(task) {
                        Some(handle) => handle,
                        None => return Err(Fail::new(libc::EAGAIN, "cannot schedule co-routine")),
                    };
                    Ok(handle.get_task_id().into())
                },
                None => unreachable!("CatcollarQueue has invalid underlying file descriptor"),
            },
            None => return Err(Fail::new(libc::EBADF, "invalid queue descriptor")),
        }
    }

    async fn do_close(
        qtable: Rc<RefCell<IoQueueTable<CatcollarQueue>>>,
        qd: QDesc,
        fd: RawFd,
        yielder: Yielder,
    ) -> (QDesc, OperationResult) {
        // Handle the result: Borrow the qtable and free the queue metadata and queue descriptor if the
        // close was successful.
        match close_coroutine(fd, yielder).await {
            Ok(()) => {
                qtable.borrow_mut().free(&qd);
                (qd, OperationResult::Close)
            },
            Err(e) => (qd, OperationResult::Failed(e)),
        }
    }

    /// Pushes a scatter-gather array to a socket.
    pub fn push(&mut self, qd: QDesc, sga: &demi_sgarray_t) -> Result<QToken, Fail> {
        trace!("push() qd={:?}", qd);

        let buf: DemiBuffer = self.runtime.clone_sgarray(sga)?;

        if buf.len() == 0 {
            return Err(Fail::new(libc::EINVAL, "zero-length buffer"));
        }

        // Issue push operation.
        match self.qtable.borrow().get(&qd) {
            Some(queue) => match queue.get_fd() {
                Some(fd) => {
                    // Issue operation.
                    let yielder: Yielder = Yielder::new();
                    let coroutine: Pin<Box<Operation>> =
                        Box::pin(Self::do_push(self.runtime.clone(), qd, fd, buf, yielder));
                    let task_id: String = format!("Catcollar::push for qd={:?}", qd);
                    let task: OperationTask = OperationTask::new(task_id, coroutine);
                    let handle: TaskHandle = match self.runtime.scheduler.insert(task) {
                        Some(handle) => handle,
                        None => return Err(Fail::new(libc::EAGAIN, "cannot schedule co-routine")),
                    };
                    Ok(handle.get_task_id().into())
                },
                None => unreachable!("CatcollarQueue has invalid underlying file descriptor"),
            },
            None => Err(Fail::new(libc::EBADF, "invalid queue descriptor")),
        }
    }

    async fn do_push(
        rt: IoUringRuntime,
        qd: QDesc,
        fd: RawFd,
        buf: DemiBuffer,
        yielder: Yielder,
    ) -> (QDesc, OperationResult) {
        match push_coroutine(rt, fd, buf, yielder).await {
            Ok(()) => (qd, OperationResult::Push),
            Err(e) => (qd, OperationResult::Failed(e)),
        }
    }

    /// Pushes a scatter-gather array to a socket.
    pub fn pushto(&mut self, qd: QDesc, sga: &demi_sgarray_t, remote: SocketAddrV4) -> Result<QToken, Fail> {
        trace!("pushto() qd={:?}", qd);

        match self.runtime.clone_sgarray(sga) {
            Ok(buf) => {
                if buf.len() == 0 {
                    return Err(Fail::new(libc::EINVAL, "zero-length buffer"));
                }

                // Issue pushto operation.
                match self.qtable.borrow().get(&qd) {
                    Some(queue) => match queue.get_fd() {
                        Some(fd) => {
                            // Issue operation.
                            let yielder: Yielder = Yielder::new();
                            let coroutine: Pin<Box<Operation>> =
                                Box::pin(Self::do_pushto(self.runtime.clone(), qd, fd, remote, buf, yielder));
                            let task_id: String = format!("Catcollar::pushto for qd={:?}", qd);
                            let task: OperationTask = OperationTask::new(task_id, coroutine);
                            let handle: TaskHandle = match self.runtime.scheduler.insert(task) {
                                Some(handle) => handle,
                                None => return Err(Fail::new(libc::EAGAIN, "cannot schedule co-routine")),
                            };
                            Ok(handle.get_task_id().into())
                        },
                        None => unreachable!("CatcollarQueue has invalid underlying file descriptor"),
                    },
                    None => Err(Fail::new(libc::EBADF, "invalid queue descriptor")),
                }
            },
            Err(e) => Err(e),
        }
    }

    async fn do_pushto(
        rt: IoUringRuntime,
        qd: QDesc,
        fd: RawFd,
        remote: SocketAddrV4,
        buf: DemiBuffer,
        yielder: Yielder,
    ) -> (QDesc, OperationResult) {
        match pushto_coroutine(rt, fd, remote, buf, yielder).await {
            Ok(()) => (qd, OperationResult::Push),
            Err(e) => (qd, OperationResult::Failed(e)),
        }
    }

    /// Pops data from a socket.
    pub fn pop(&mut self, qd: QDesc, size: Option<usize>) -> Result<QToken, Fail> {
        trace!("pop() qd={:?}, size={:?}", qd, size);

        // We just assert 'size' here, because it was previously checked at PDPIX layer.
        debug_assert!(size.is_none() || ((size.unwrap() > 0) && (size.unwrap() <= limits::POP_SIZE_MAX)));

        let buf: DemiBuffer = {
            let size: usize = size.unwrap_or(limits::RECVBUF_SIZE_MAX);
            DemiBuffer::new(size as u16)
        };

        // Issue pop operation.
        match self.qtable.borrow().get(&qd) {
            Some(queue) => match queue.get_fd() {
                Some(fd) => {
                    let yielder: Yielder = Yielder::new();
                    let coroutine: Pin<Box<Operation>> =
                        Box::pin(Self::do_pop(self.runtime.clone(), qd, fd, buf, yielder));
                    let task_id: String = format!("Catcollar::pop for qd={:?}", qd);
                    let task: OperationTask = OperationTask::new(task_id, coroutine);
                    let handle: TaskHandle = match self.runtime.scheduler.insert(task) {
                        Some(handle) => handle,
                        None => return Err(Fail::new(libc::EAGAIN, "cannot schedule co-routine")),
                    };
                    let qt: QToken = handle.get_task_id().into();
                    Ok(qt)
                },
                None => unreachable!("CatcollarQueue has invalid underlying file descriptor"),
            },
            _ => Err(Fail::new(libc::EBADF, "invalid queue descriptor")),
        }
    }

    async fn do_pop(
        rt: IoUringRuntime,
        qd: QDesc,
        fd: RawFd,
        buf: DemiBuffer,
        yielder: Yielder,
    ) -> (QDesc, OperationResult) {
        // Handle the result: if successful, return the addr and buffer.
        match pop_coroutine(rt, fd, buf, yielder).await {
            Ok((addr, buf)) => (qd, OperationResult::Pop(addr, buf)),
            Err(e) => (qd, OperationResult::Failed(e)),
        }
    }

    pub fn poll(&self) {
        self.runtime.scheduler.poll()
    }

    pub fn schedule(&mut self, qt: QToken) -> Result<TaskHandle, Fail> {
        match self.runtime.scheduler.from_task_id(qt.into()) {
            Some(handle) => Ok(handle),
            None => return Err(Fail::new(libc::EINVAL, "invalid queue token")),
        }
    }

    pub fn pack_result(&mut self, handle: TaskHandle, qt: QToken) -> Result<demi_qresult_t, Fail> {
        let (qd, r): (QDesc, OperationResult) = self.take_result(handle);
        Ok(pack_result(&self.runtime, r, qd, qt.into()))
    }

    /// Allocates a scatter-gather array.
    pub fn sgaalloc(&self, size: usize) -> Result<demi_sgarray_t, Fail> {
        trace!("sgalloc() size={:?}", size);
        self.runtime.alloc_sgarray(size)
    }

    /// Frees a scatter-gather array.
    pub fn sgafree(&self, sga: demi_sgarray_t) -> Result<(), Fail> {
        trace!("sgafree()");
        self.runtime.free_sgarray(sga)
    }

    /// Takes out the operation result descriptor associated with the target scheduler handle.
    fn take_result(&mut self, handle: TaskHandle) -> (QDesc, OperationResult) {
        let task: OperationTask = if let Some(task) = self.runtime.scheduler.remove(&handle) {
            OperationTask::from(task.as_any())
        } else {
            panic!("Removing task that does not exist (either was previously removed or never inserted)");
        };

        task.get_result().expect("The coroutine has not finished")
    }
}
//======================================================================================================================
// Standalone Functions
//======================================================================================================================

/// Packs a [OperationResult] into a [demi_qresult_t].
fn pack_result(rt: &IoUringRuntime, result: OperationResult, qd: QDesc, qt: u64) -> demi_qresult_t {
    match result {
        OperationResult::Connect => demi_qresult_t {
            qr_opcode: demi_opcode_t::DEMI_OPC_CONNECT,
            qr_qd: qd.into(),
            qr_qt: qt,
            qr_ret: 0,
            qr_value: unsafe { mem::zeroed() },
        },
        OperationResult::Accept((new_qd, addr)) => {
            let saddr: SockAddr = linux::socketaddrv4_to_sockaddr(&addr);
            let qr_value: demi_qr_value_t = demi_qr_value_t {
                ares: demi_accept_result_t {
                    qd: new_qd.into(),
                    addr: saddr,
                },
            };
            demi_qresult_t {
                qr_opcode: demi_opcode_t::DEMI_OPC_ACCEPT,
                qr_qd: qd.into(),
                qr_qt: qt,
                qr_ret: 0,
                qr_value,
            }
        },
        OperationResult::Push => demi_qresult_t {
            qr_opcode: demi_opcode_t::DEMI_OPC_PUSH,
            qr_qd: qd.into(),
            qr_qt: qt,
            qr_ret: 0,
            qr_value: unsafe { mem::zeroed() },
        },
        OperationResult::Pop(addr, bytes) => match rt.into_sgarray(bytes) {
            Ok(mut sga) => {
                if let Some(addr) = addr {
                    sga.sga_addr = linux::socketaddrv4_to_sockaddr(&addr);
                }
                let qr_value: demi_qr_value_t = demi_qr_value_t { sga };
                demi_qresult_t {
                    qr_opcode: demi_opcode_t::DEMI_OPC_POP,
                    qr_qd: qd.into(),
                    qr_qt: qt,
                    qr_ret: 0,
                    qr_value,
                }
            },
            Err(e) => {
                warn!("Operation Failed: {:?}", e);
                demi_qresult_t {
                    qr_opcode: demi_opcode_t::DEMI_OPC_FAILED,
                    qr_qd: qd.into(),
                    qr_qt: qt,
                    qr_ret: e.errno as i64,
                    qr_value: unsafe { mem::zeroed() },
                }
            },
        },
        OperationResult::Close => demi_qresult_t {
            qr_opcode: demi_opcode_t::DEMI_OPC_CLOSE,
            qr_qd: qd.into(),
            qr_qt: qt,
            qr_ret: 0,
            qr_value: unsafe { mem::zeroed() },
        },
        OperationResult::Failed(e) => {
            warn!("Operation Failed: {:?}", e);
            demi_qresult_t {
                qr_opcode: demi_opcode_t::DEMI_OPC_FAILED,
                qr_qd: qd.into(),
                qr_qt: qt,
                qr_ret: e.errno as i64,
                qr_value: unsafe { mem::zeroed() },
            }
        },
    }
}
