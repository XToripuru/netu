#![allow(async_fn_in_trait)]
use super::forward::*;

use std::{
    io,
    net::{
        SocketAddrV4,
        SocketAddr,
        IpAddr,
        Ipv4Addr
    },
    sync::{Arc, Mutex, Condvar},
    error::Error
};
use tokio::{
    net::{
        ToSocketAddrs,
        TcpListener,
        TcpStream
    },
    io::{AsyncReadExt, AsyncWriteExt},
    task::spawn,
    time::sleep
};
use igd::{aio::search_gateway, PortMappingProtocol, SearchOptions};
use local_ip_address::local_ip;
use serde::{de::{DeserializeOwned, Deserialize}, Serialize};
use bincode;

pub struct AsyncMessageStream {
    inner: TcpStream,
    offset: usize,
    buffer: Vec<u8>,
    referenced: bool
}

impl AsyncMessageStream {
    pub async fn connect<A: ToSocketAddrs>(addr: A) -> io::Result<Self> {
        TcpStream::connect(addr).await.map(|stream| {
            AsyncMessageStream {
                inner: stream,
                offset: 0,
                buffer: vec![0u8; 1024],
                referenced: false
            }
        })
    }
    pub async fn send<M: Serialize>(&mut self, message: M) -> Result<(), Box<dyn Error>> {
        let raw = bincode::serialize(&message)?;
        let header = (8 + raw.len() as u64).to_be_bytes();
        self.inner.write_all(&header).await?;
        self.inner.write_all(&raw).await?;
        Ok(())
    }
    pub async fn recv<M: DeserializeOwned>(&mut self) -> Result<Option<M>, Box<dyn Error>> {
        if self.referenced {
            let size = u64::from_be_bytes(self.buffer[0..8].try_into().unwrap()) as usize;
            self.buffer.drain(0..size);
            self.offset -= size;
            self.referenced = false;
        }

        let n = self.inner.read(&mut self.buffer[self.offset..]).await?;
        self.offset += n;

        // Extend buffer while it's full
        while self.offset == self.buffer.len() {
            self.buffer.extend(std::iter::repeat(0).take(self.buffer.len() * 2));

            // If the buffer is full it most likely means that there's more waiting already
            let n = self.inner.read(&mut self.buffer[self.offset..]).await?;
            self.offset += n;
        }

        if self.offset > 8 {
            let size = u64::from_be_bytes(self.buffer[0..8].try_into().unwrap()) as usize;
            if self.offset >= size {
                let message: M = bincode::deserialize(&self.buffer[8..size])?;
                self.buffer.drain(0..size);
                self.offset -= size;
                return Ok(Some(message));
            }
        }

        Ok(None)
    }
    pub async fn recv_ref<'a, M: Deserialize<'a>>(&'a mut self) -> Result<Option<M>, Box<dyn Error>> {
        if self.referenced {
            let size = u64::from_be_bytes(self.buffer[0..8].try_into().unwrap()) as usize;
            self.buffer.drain(0..size);
            self.offset -= size;
            self.referenced = false;
        }

        let n = self.inner.read(&mut self.buffer[self.offset..]).await?;
        self.offset += n;

        // Extend buffer while it's full
        while self.offset == self.buffer.len() {
            self.buffer.extend(std::iter::repeat(0).take(self.buffer.len() * 2));

            // If the buffer is full it most likely means that there's more waiting already
            let n = self.inner.read(&mut self.buffer[self.offset..]).await?;
            self.offset += n;
        }

        if self.offset > 8 {
            let size = u64::from_be_bytes(self.buffer[0..8].try_into().unwrap()) as usize;
            if self.offset >= size {
                let message: M = bincode::deserialize(&self.buffer[8..size])?;
                self.referenced = true;
                return Ok(Some(message));
            }
        }

        Ok(None)
    }
}

pub trait AsyncTcpListenerExt {
    async fn forwarded<A: ToSocketAddrs>(addr: A) -> Result<Forwarded<Self>, Box<dyn Error>> where Self: Sized;
    async fn messenger(&self) -> io::Result<(AsyncMessageStream, SocketAddr)>;
}

impl AsyncTcpListenerExt for TcpListener {
    /// Works the same as `TcpListener::bind` but also spawns a task that periodically requests router to uPnP forward specified port.
    async fn forwarded<A: ToSocketAddrs>(addr: A) -> Result<Forwarded<Self>, Box<dyn Error>> {
        let listener = TcpListener::bind(addr).await?;
        let (external, state) = forward(listener.local_addr().unwrap().port()).await?;
        Ok(Forwarded {
            inner: listener,
            external,
            state
        })
    }
    /// Works the same as `TcpListener::accept` but returns a `MessageStream` instead of `TcpStream`.
    async fn messenger(&self) -> io::Result<(AsyncMessageStream, SocketAddr)> {
        self.accept().await.map(|(stream, addr)| (
            AsyncMessageStream {
                inner: stream,
                offset: 0,
                buffer: vec![0u8; 1024],
                referenced: false
            },
            addr
        ))
    }
}

async fn forward(port: u16) -> Result<(Ipv4Addr, Arc<(Mutex<State>, Condvar)>), Box<dyn Error>>  {
    let ip = local_ip().unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));

    let gateway = match search_gateway(SearchOptions::default()).await {
        Ok(gateway) => gateway,
        Err(err) => {
            log::warn!("uPnP discovery failed err: {err}");
            return Err(Box::new(ForwardingError::DiscoveryFailed));
        }
    };

    let Ok(external_ip) = gateway.get_external_ip().await else {
        log::error!("NAT can't provide external ip");
        return Err(Box::new(ForwardingError::CannotFetchExternalIp));
    };

    let IpAddr::V4(ip) = ip else {
        log::error!("Non-V4 address");
        return Err(Box::new(ForwardingError::NonIpv4));
    };

    gateway
    .add_port(
        PortMappingProtocol::TCP,
        port,
        SocketAddrV4::new(ip, port),
        LEASE.as_secs() as u32 + 1,
        "Flow port mapping",
    ).await?;

    let state = Arc::new((Mutex::new(State::Running), Condvar::new()));

    spawn({
        let state = state.clone();
        async move {
            sleep(LEASE).await;
            loop {
                if let Err(_err) = gateway
                    .add_port(
                        PortMappingProtocol::TCP,
                        port,
                        SocketAddrV4::new(ip, port),
                        LEASE.as_secs() as u32 + 1,
                        "Port mapping",
                    )
                    .await
                {
                    log::error!("uPnP forwarding failed");
                    *state.0.lock().unwrap() = State::Error;
                    break
                }
                
                let (lock, cv) = &*state;
                let (guard, wait) = cv.wait_timeout_while(
                    lock.lock().unwrap(),
                    LEASE,
                    |state| *state != State::Terminate
                ).unwrap();

                if !wait.timed_out() && *guard == State::Terminate {
                    break;
                }
            }
            let _ = gateway.remove_port(PortMappingProtocol::TCP, port).await;
        }
    });

    Ok((external_ip, state))
}