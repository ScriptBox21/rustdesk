use crate::{bail, ResultType};
use anyhow::anyhow;
use bytes::{Bytes, BytesMut};
use futures::{SinkExt, StreamExt};
use protobuf::Message;
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use tokio::net::{ToSocketAddrs, UdpSocket};
use tokio_socks::{udp::Socks5UdpFramed, IntoTargetAddr, TargetAddr, ToProxyAddrs};
use tokio_util::{codec::BytesCodec, udp::UdpFramed};

pub enum FramedSocket {
    Direct(UdpFramed<BytesCodec>),
    ProxySocks(Socks5UdpFramed),
}

fn new_socket(addr: SocketAddr, reuse: bool) -> Result<Socket, std::io::Error> {
    let socket = match addr {
        SocketAddr::V4(..) => Socket::new(Domain::ipv4(), Type::dgram(), None),
        SocketAddr::V6(..) => Socket::new(Domain::ipv6(), Type::dgram(), None),
    }?;
    if reuse {
        // windows has no reuse_port, but it's reuse_address
        // almost equals to unix's reuse_port + reuse_address,
        // though may introduce nondeterministic behavior
        #[cfg(unix)]
        socket.set_reuse_port(true)?;
        socket.set_reuse_address(true)?;
    }
    socket.bind(&addr.into())?;
    Ok(socket)
}

impl FramedSocket {
    pub async fn new<T: ToSocketAddrs>(addr: T) -> ResultType<Self> {
        let socket = UdpSocket::bind(addr).await?;
        Ok(Self::Direct(UdpFramed::new(socket, BytesCodec::new())))
    }

    #[allow(clippy::never_loop)]
    pub async fn new_reuse<T: std::net::ToSocketAddrs>(addr: T) -> ResultType<Self> {
        for addr in addr.to_socket_addrs()? {
            let socket = new_socket(addr, true)?.into_udp_socket();
            return Ok(Self::Direct(UdpFramed::new(
                UdpSocket::from_std(socket)?,
                BytesCodec::new(),
            )));
        }
        bail!("could not resolve to any address");
    }

    pub async fn new_proxy<'a, 't, P: ToProxyAddrs, T: ToSocketAddrs>(
        proxy: P,
        local: T,
        username: &'a str,
        password: &'a str,
        ms_timeout: u64,
    ) -> ResultType<Self> {
        let framed = if username.trim().is_empty() {
            super::timeout(ms_timeout, Socks5UdpFramed::connect(proxy, Some(local))).await??
        } else {
            super::timeout(
                ms_timeout,
                Socks5UdpFramed::connect_with_password(proxy, Some(local), username, password),
            )
            .await??
        };
        log::trace!(
            "Socks5 udp connected, local addr: {:?}, target addr: {}",
            framed.local_addr(),
            framed.socks_addr()
        );
        Ok(Self::ProxySocks(framed))
    }

    #[inline]
    pub async fn send(
        &mut self,
        msg: &impl Message,
        addr: impl IntoTargetAddr<'_>,
    ) -> ResultType<()> {
        let addr = addr.into_target_addr()?.to_owned();
        let send_data = Bytes::from(msg.write_to_bytes()?);
        let _ = match self {
            Self::Direct(f) => match addr {
                TargetAddr::Ip(addr) => f.send((send_data, addr)).await?,
                _ => unreachable!(),
            },
            Self::ProxySocks(f) => f.send((send_data, addr)).await?,
        };
        Ok(())
    }

    // https://stackoverflow.com/a/68733302/1926020
    #[inline]
    pub async fn send_raw(
        &mut self,
        msg: &'static [u8],
        addr: impl IntoTargetAddr<'static>,
    ) -> ResultType<()> {
        let addr = addr.into_target_addr()?.to_owned();

        let _ = match self {
            Self::Direct(f) => match addr {
                TargetAddr::Ip(addr) => f.send((Bytes::from(msg), addr)).await?,
                _ => unreachable!(),
            },
            Self::ProxySocks(f) => f.send((Bytes::from(msg), addr)).await?,
        };
        Ok(())
    }

    #[inline]
    pub async fn next(&mut self) -> Option<ResultType<(BytesMut, TargetAddr<'static>)>> {
        match self {
            Self::Direct(f) => match f.next().await {
                Some(Ok((data, addr))) => {
                    Some(Ok((data, addr.into_target_addr().ok()?.to_owned())))
                }
                Some(Err(e)) => Some(Err(anyhow!(e))),
                None => None,
            },
            Self::ProxySocks(f) => match f.next().await {
                Some(Ok((data, _))) => Some(Ok((data.data, data.dst_addr))),
                Some(Err(e)) => Some(Err(anyhow!(e))),
                None => None,
            },
        }
    }

    #[inline]
    pub async fn next_timeout(
        &mut self,
        ms: u64,
    ) -> Option<ResultType<(BytesMut, TargetAddr<'static>)>> {
        if let Ok(res) =
            tokio::time::timeout(std::time::Duration::from_millis(ms), self.next()).await
        {
            res
        } else {
            None
        }
    }
}

// const DEFAULT_MULTICAST: &str = "239.255.42.98";

pub fn bind_multicast(maddr: Option<SocketAddrV4>) -> ResultType<FramedSocket> {
    // todo: https://github.com/bltavares/multicast-socket
    // 0.0.0.0 bind to default interface, if there are two interfaces, there will be problem.
    let socket = Socket::new(Domain::ipv4(), Type::dgram(), Some(Protocol::udp()))?;
    socket.set_reuse_address(true)?;
    // somehow without this, timer.tick() under tokio::select! does not work
    socket.set_read_timeout(Some(std::time::Duration::from_millis(100)))?;
    if let Some(maddr) = maddr {
        assert!(maddr.ip().is_multicast(), "Must be multcast address");
        let addr = SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0).into(), maddr.port());
        socket.join_multicast_v4(maddr.ip(), addr.ip())?;
        socket.set_multicast_loop_v4(true)?;
        socket.bind(&socket2::SockAddr::from(addr))?;
    } else {
        socket.set_multicast_if_v4(&Ipv4Addr::new(0, 0, 0, 0))?;
        socket.bind(&socket2::SockAddr::from(SocketAddr::new(
            Ipv4Addr::new(0, 0, 0, 0).into(),
            0,
        )))?;
    }
    Ok(FramedSocket::Direct(UdpFramed::new(
        UdpSocket::from_std(socket.into_udp_socket())?,
        BytesCodec::new(),
    )))
}
