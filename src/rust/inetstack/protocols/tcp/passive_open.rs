// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use super::{
    constants::FALLBACK_MSS,
    established::SharedControlBlock,
    isn_generator::IsnGenerator,
};
use crate::{
    inetstack::protocols::{
        arp::SharedArpPeer,
        ethernet2::{
            EtherType2,
            Ethernet2Header,
        },
        ip::IpProtocol,
        ipv4::Ipv4Header,
        tcp::{
            established::{
                congestion_control,
                congestion_control::CongestionControl,
            },
            segment::{
                TcpHeader,
                TcpOptions2,
                TcpSegment,
            },
            SeqNumber,
        },
    },
    runtime::{
        fail::Fail,
        network::{
            config::TcpConfig,
            types::MacAddress,
            NetworkRuntime,
        },
        timer::TimerRc,
        SharedBox,
        SharedDemiRuntime,
        SharedObject,
    },
    scheduler::TaskHandle,
};
use ::libc::{
    EBADMSG,
    ETIMEDOUT,
};
use ::std::{
    collections::{
        HashMap,
        HashSet,
        VecDeque,
    },
    convert::TryInto,
    net::SocketAddrV4,
    ops::{
        Deref,
        DerefMut,
    },
    task::{
        Context,
        Poll,
        Waker,
    },
    time::Duration,
};

struct InflightAccept {
    local_isn: SeqNumber,
    remote_isn: SeqNumber,
    header_window_size: u16,
    remote_window_scale: Option<u8>,
    mss: usize,
    handle: TaskHandle,
}

struct ReadySockets<const N: usize> {
    ready: VecDeque<Result<SharedControlBlock<N>, Fail>>,
    endpoints: HashSet<SocketAddrV4>,
    waker: Option<Waker>,
}

impl<const N: usize> ReadySockets<N> {
    fn push_ok(&mut self, cb: SharedControlBlock<N>) {
        assert!(self.endpoints.insert(cb.get_remote()));
        self.ready.push_back(Ok(cb));
        if let Some(w) = self.waker.take() {
            w.wake()
        }
    }

    fn push_err(&mut self, err: Fail) {
        self.ready.push_back(Err(err));
        if let Some(w) = self.waker.take() {
            w.wake()
        }
    }

    fn poll(&mut self, ctx: &mut Context) -> Poll<Result<SharedControlBlock<N>, Fail>> {
        let r = match self.ready.pop_front() {
            Some(r) => r,
            None => {
                self.waker.replace(ctx.waker().clone());
                return Poll::Pending;
            },
        };
        if let Ok(ref cb) = r {
            assert!(self.endpoints.remove(&cb.get_remote()));
        }
        Poll::Ready(r)
    }

    fn len(&self) -> usize {
        self.ready.len()
    }
}

pub struct PassiveSocket<const N: usize> {
    inflight: HashMap<SocketAddrV4, InflightAccept>,
    ready: ReadySockets<N>,
    max_backlog: usize,
    isn_generator: IsnGenerator,
    local: SocketAddrV4,
    runtime: SharedDemiRuntime,
    transport: SharedBox<dyn NetworkRuntime<N>>,
    clock: TimerRc,
    tcp_config: TcpConfig,
    local_link_addr: MacAddress,
    arp: SharedArpPeer<N>,
}

#[derive(Clone)]
pub struct SharedPassiveSocket<const N: usize>(SharedObject<PassiveSocket<N>>);

impl<const N: usize> SharedPassiveSocket<N> {
    pub fn new(
        local: SocketAddrV4,
        max_backlog: usize,
        runtime: SharedDemiRuntime,
        transport: SharedBox<dyn NetworkRuntime<N>>,
        clock: TimerRc,
        tcp_config: TcpConfig,
        local_link_addr: MacAddress,
        arp: SharedArpPeer<N>,
        nonce: u32,
    ) -> Self {
        let ready = ReadySockets {
            ready: VecDeque::new(),
            endpoints: HashSet::new(),
            waker: None,
        };
        Self(SharedObject::<PassiveSocket<N>>::new(PassiveSocket::<N> {
            inflight: HashMap::new(),
            ready,
            max_backlog,
            isn_generator: IsnGenerator::new(nonce),
            local,
            local_link_addr,
            runtime,
            transport,
            clock,
            tcp_config,
            arp,
        }))
    }

    /// Returns the address that the socket is bound to.
    pub fn endpoint(&self) -> SocketAddrV4 {
        self.local
    }

    pub fn poll_accept(&mut self, ctx: &mut Context) -> Poll<Result<SharedControlBlock<N>, Fail>> {
        self.ready.poll(ctx)
    }

    pub fn receive(&mut self, ip_header: &Ipv4Header, header: &TcpHeader) -> Result<(), Fail> {
        let remote = SocketAddrV4::new(ip_header.get_src_addr(), header.src_port);
        if self.ready.endpoints.contains(&remote) {
            // TODO: What should we do if a packet shows up for a connection that hasn't been `accept`ed yet?
            return Ok(());
        }
        let inflight_len = self.inflight.len();

        // If the packet is for an inflight connection, route it there.
        if self.inflight.contains_key(&remote) {
            if !header.ack {
                return Err(Fail::new(EBADMSG, "expeting ACK"));
            }
            debug!("Received ACK: {:?}", header);
            let &InflightAccept {
                local_isn,
                remote_isn,
                header_window_size,
                remote_window_scale,
                mss,
                ..
            } = self.inflight.get(&remote).unwrap();
            if header.ack_num != local_isn + SeqNumber::from(1) {
                return Err(Fail::new(EBADMSG, "invalid SYN+ACK seq num"));
            }

            let (local_window_scale, remote_window_scale) = match remote_window_scale {
                Some(w) => (self.tcp_config.get_window_scale() as u32, w),
                None => (0, 0),
            };
            let remote_window_size = (header_window_size)
                .checked_shl(remote_window_scale as u32)
                .expect("TODO: Window size overflow")
                .try_into()
                .expect("TODO: Window size overflow");
            let local_window_size = (self.tcp_config.get_receive_window_size() as u32)
                .checked_shl(local_window_scale as u32)
                .expect("TODO: Window size overflow");
            info!(
                "Window sizes: local {}, remote {}",
                local_window_size, remote_window_size
            );
            info!(
                "Window scale: local {}, remote {}",
                local_window_scale, remote_window_scale
            );

            if let Some(mut inflight) = self.inflight.remove(&remote) {
                inflight.handle.deschedule();
            }

            let cb = SharedControlBlock::new(
                self.local,
                remote,
                self.runtime.clone(),
                self.transport.clone(),
                self.clock.clone(),
                self.local_link_addr,
                self.tcp_config.clone(),
                self.arp.clone(),
                remote_isn + SeqNumber::from(1),
                self.tcp_config.get_ack_delay_timeout(),
                local_window_size,
                local_window_scale,
                local_isn + SeqNumber::from(1),
                remote_window_size,
                remote_window_scale,
                mss,
                congestion_control::None::new,
                None,
            );
            self.ready.push_ok(cb);
            return Ok(());
        }

        // Otherwise, start a new connection.
        if !header.syn || header.ack || header.rst {
            return Err(Fail::new(EBADMSG, "invalid flags"));
        }
        debug!("Received SYN: {:?}", header);
        if inflight_len + self.ready.len() >= self.max_backlog {
            let cause: String = format!(
                "backlog full (inflight={}, ready={}, backlog={})",
                inflight_len,
                self.ready.len(),
                self.max_backlog
            );
            error!("receive(): {:?}", &cause);
            return Err(Fail::new(libc::ECONNREFUSED, &cause));
        }
        let local: SocketAddrV4 = self.local.clone();
        let local_isn = self.isn_generator.generate(&local, &remote);
        let remote_isn = header.seq_num;
        let future = self.clone().background(remote, remote_isn, local_isn);
        let handle: TaskHandle = self
            .runtime
            .insert_background_coroutine("Inetstack::TCP::passiveopen::background", Box::pin(future))?;

        let mut remote_window_scale = None;
        let mut mss = FALLBACK_MSS;
        for option in header.iter_options() {
            match option {
                TcpOptions2::WindowScale(w) => {
                    info!("Received window scale: {:?}", w);
                    remote_window_scale = Some(*w);
                },
                TcpOptions2::MaximumSegmentSize(m) => {
                    info!("Received advertised MSS: {}", m);
                    mss = *m as usize;
                },
                _ => continue,
            }
        }
        let accept = InflightAccept {
            local_isn,
            remote_isn,
            header_window_size: header.window_size,
            remote_window_scale,
            mss,
            handle,
        };
        self.inflight.insert(remote, accept);
        Ok(())
    }

    async fn background(mut self, remote: SocketAddrV4, remote_isn: SeqNumber, local_isn: SeqNumber) {
        let handshake_retries: usize = self.tcp_config.get_handshake_retries();
        let handshake_timeout: Duration = self.tcp_config.get_handshake_timeout();

        for _ in 0..handshake_retries {
            let remote_link_addr = match self.arp.query(remote.ip().clone()).await {
                Ok(r) => r,
                Err(e) => {
                    warn!("ARP query failed: {:?}", e);
                    continue;
                },
            };
            let mut tcp_hdr = TcpHeader::new(self.local.port(), remote.port());
            tcp_hdr.syn = true;
            tcp_hdr.seq_num = local_isn;
            tcp_hdr.ack = true;
            tcp_hdr.ack_num = remote_isn + SeqNumber::from(1);
            tcp_hdr.window_size = self.tcp_config.get_receive_window_size();

            let mss = self.tcp_config.get_advertised_mss() as u16;
            tcp_hdr.push_option(TcpOptions2::MaximumSegmentSize(mss));
            info!("Advertising MSS: {}", mss);

            tcp_hdr.push_option(TcpOptions2::WindowScale(self.tcp_config.get_window_scale()));
            info!("Advertising window scale: {}", self.tcp_config.get_window_scale());

            debug!("Sending SYN+ACK: {:?}", tcp_hdr);
            let segment = TcpSegment {
                ethernet2_hdr: Ethernet2Header::new(remote_link_addr, self.local_link_addr, EtherType2::Ipv4),
                ipv4_hdr: Ipv4Header::new(self.local.ip().clone(), remote.ip().clone(), IpProtocol::TCP),
                tcp_hdr,
                data: None,
                tx_checksum_offload: self.tcp_config.get_rx_checksum_offload(),
            };
            self.transport.transmit(Box::new(segment));
            self.clock.wait(self.clock.clone(), handshake_timeout).await;
        }
        self.ready.push_err(Fail::new(ETIMEDOUT, "handshake timeout"));
    }
}

//======================================================================================================================
// Trait Implementations
//======================================================================================================================

impl<const N: usize> Deref for SharedPassiveSocket<N> {
    type Target = PassiveSocket<N>;

    fn deref(&self) -> &Self::Target {
        self.0.deref()
    }
}

impl<const N: usize> DerefMut for SharedPassiveSocket<N> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.deref_mut()
    }
}
