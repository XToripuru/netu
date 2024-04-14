use std::{
    io::Write,
    net::TcpListener
};

use netu::prelude::*;

fn main() {

    let listener = TcpListener::forwarded("0.0.0.0:80").unwrap();

    println!("Go to {} on your web browser", listener.external());
    while listener.is_forwarded() {
        let (mut stream, _addr) = listener.accept().unwrap();
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\nContent-Type: text/html\r\n\r\nHello World!").unwrap();
    }

}