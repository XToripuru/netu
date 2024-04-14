use super::forward::*;

use std::{
    error::Error,
    io::{self, ErrorKind, Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, ToSocketAddrs},
    sync::{Arc, Mutex, Condvar},
    thread::spawn
};
use igd::{search_gateway, PortMappingProtocol, SearchOptions};
use local_ip_address::local_ip;
use serde::{de::DeserializeOwned, Serialize};
use bincode;

/// A wrapper for `TcpStream` that allows to simply send and receive structs which implement `serde::{Serialize, Deserialize}`.
pub struct MessageStream {
    inner: TcpStream,
    offset: usize,
    buffer: Vec<u8>
}

impl MessageStream {
    /// Works the same as `TcpStream::connect`.
    pub fn connect<A: ToSocketAddrs>(addr: A) -> io::Result<Self> {
        TcpStream::connect(addr).map(|stream| {
            stream.set_nonblocking(true).unwrap();
            MessageStream {
                inner: stream,
                offset: 0,
                buffer: vec![0u8; 1024]
            }
        })
    }
    /// Send a type that implements `serde::Serialize`.
    pub fn send<M: Serialize>(&mut self, message: M) -> Result<(), Box<dyn Error>> {
        let raw = bincode::serialize(&message)?;
        let header = (8 + raw.len() as u64).to_be_bytes();
        self.inner.write_all(&header)?;
        self.inner.write_all(&raw)?;
        Ok(())
    }
    /// Receieve a type that implements `serde::Deserialize`.
    /// This function is non-blocking and has internal buffering.
    pub fn recv<M: DeserializeOwned>(&mut self) -> Result<Option<M>, Box<dyn Error>> {
        let n = match self.inner.read(&mut self.buffer[self.offset..]) {
            Ok(n) => n,
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                return Ok(None)
            },
            err => err?
        };
        self.offset += n;

        // Extend buffer while it's full
        while self.offset == self.buffer.len() {
            self.buffer.extend(std::iter::repeat(0).take(self.buffer.len() * 2));

            // If the buffer is full it most likely means that there's more waiting already
            let n = match self.inner.read(&mut self.buffer[self.offset..]) {
                Ok(n) => n,
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    break
                },
                err => err?
            };
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
}

pub trait TcpListenerExt {
    fn forwarded<A: ToSocketAddrs>(addr: A) -> Result<Forwarded<Self>, Box<dyn Error>> where Self: Sized;
    fn messenger(&self) -> io::Result<(MessageStream, SocketAddr)>;
}

impl TcpListenerExt for TcpListener {
    /// Works the same as `TcpListener::bind` but also spawns a thread that periodically requests router to uPnP forward specified port.
    fn forwarded<A: ToSocketAddrs>(addr: A) -> Result<Forwarded<Self>, Box<dyn Error>> {
        let listener = TcpListener::bind(addr)?;
        let (external, state) = forward(listener.local_addr().unwrap().port())?;
        Ok(Forwarded {
            inner: listener,
            external,
            state
        })
    }
    /// Works the same as `TcpListener::accept` but returns a `MessageStream` instead of `TcpStream`.
    fn messenger(&self) -> io::Result<(MessageStream, SocketAddr)> {
        self.accept().map(|(stream, addr)| (
            MessageStream {
                inner: stream,
                offset: 0,
                buffer: vec![0u8; 1024]
            },
            addr
        ))
    }
}

fn forward(port: u16) -> Result<(Ipv4Addr, Arc<(Mutex<State>, Condvar)>), Box<dyn Error>>  {
    let ip = local_ip().unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));

    let gateway = match search_gateway(SearchOptions::default()) {
        Ok(gateway) => gateway,
        Err(err) => {
            log::warn!("uPnP discovery failed err: {err}");
            return Err(Box::new(ForwardingError::DiscoveryFailed));
        }
    };

    let Ok(external_ip) = gateway.get_external_ip() else {
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
        "Port mapping",
    )?;

    let state = Arc::new((Mutex::new(State::Running), Condvar::new()));

    spawn({
        let state = state.clone();
        move || {
            loop {
                if let Err(_err) = gateway
                    .add_port(
                        PortMappingProtocol::TCP,
                        port,
                        SocketAddrV4::new(ip, port),
                        LEASE.as_secs() as u32 + 1,
                        "Port mapping",
                    )
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
            let _ = gateway.remove_port(PortMappingProtocol::TCP, port);
        }
    });

    Ok((external_ip, state))
}