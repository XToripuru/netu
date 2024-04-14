use std::{
    net::TcpListener,
    thread::{sleep, spawn},
    time::Duration
};

use netu::prelude::*;
use serde::{Serialize, Deserialize};

fn main() {

    spawn(server);
    client();

}

fn client() {
    let mut stream = MessageStream::connect("localhost:8080").unwrap();

    loop {
        match stream.recv::<Tick>() {
            Ok(Some(message)) => {
                println!("{:?}", message);
            }
            Ok(None) => {
                println!("Still waiting");
            }
            Err(err) => {
                println!("There was an error: {err}");
                break;
            }
        }
        sleep(Duration::from_millis(400));
    }
}

fn server() {
    let listener = TcpListener::bind("0.0.0.0:8080").unwrap();

    let (mut stream, _addr) = listener.messenger().unwrap();

    // Send a tick every second
    for count in 0.. {
        stream.send(Tick {
            count
        }).unwrap();

        sleep(Duration::from_secs(1));
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Tick {
    count: u64
}