extern crate clap;
extern crate nix;
extern crate socket2;
extern crate solana;

use clap::{App, Arg};
use nix::sys::socket::setsockopt;
use nix::sys::socket::sockopt::ReusePort;
use socket2::{Domain, SockAddr, Socket, Type};
use solana::packet::{Packet, PacketRecycler, BLOB_SIZE, PACKET_DATA_SIZE};
use solana::result::Result;
use solana::streamer::{receiver, PacketReceiver};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::channel;
use std::sync::Arc;
use std::thread::sleep;
use std::thread::{spawn, JoinHandle};
use std::time::Duration;
use std::time::SystemTime;

fn producer(addr: &SocketAddr, recycler: &PacketRecycler, exit: Arc<AtomicBool>) -> JoinHandle<()> {
    let send = UdpSocket::bind("0.0.0.0:0").unwrap();
    let msgs = recycler.allocate();
    let msgs_ = msgs.clone();
    msgs.write().unwrap().packets.resize(10, Packet::default());
    for w in &mut msgs.write().unwrap().packets {
        w.meta.size = PACKET_DATA_SIZE;
        w.meta.set_addr(&addr);
    }
    spawn(move || loop {
        if exit.load(Ordering::Relaxed) {
            return;
        }
        let mut num = 0;
        for p in &msgs_.read().unwrap().packets {
            let a = p.meta.addr();
            assert!(p.meta.size < BLOB_SIZE);
            send.send_to(&p.data[..p.meta.size], &a).unwrap();
            num += 1;
        }
        assert_eq!(num, 10);
    })
}

fn sink(
    recycler: PacketRecycler,
    exit: Arc<AtomicBool>,
    rvs: Arc<AtomicUsize>,
    r: PacketReceiver,
) -> JoinHandle<()> {
    spawn(move || loop {
        if exit.load(Ordering::Relaxed) {
            return;
        }
        let timer = Duration::new(1, 0);
        if let Ok(msgs) = r.recv_timeout(timer) {
            rvs.fetch_add(msgs.read().unwrap().packets.len(), Ordering::Relaxed);
            recycler.recycle(msgs, "sink");
        }
    })
}

macro_rules! socketaddr {
    ($ip:expr, $port:expr) => {
        SocketAddr::from((Ipv4Addr::from($ip), $port))
    };
    ($str:expr) => {{
        let a: SocketAddr = $str.parse().unwrap();
        a
    }};
}

fn main() -> Result<()> {
    let mut num_sockets = 1usize;

    let matches = App::new("solana-bench-streamer")
        .arg(
            Arg::with_name("num-recv-sockets")
                .short("N")
                .long("num-recv-sockets")
                .value_name("NUM")
                .takes_value(true)
                .help("Use NUM receive sockets"),
        )
        .get_matches();

    if let Some(n) = matches.value_of("num-recv-sockets") {
        num_sockets = n.to_string().parse().expect("integer");
    }

    fn bind_to(port: u16) -> UdpSocket {
        let sock = Socket::new(Domain::ipv4(), Type::dgram(), None).unwrap();
        let sock_fd = sock.as_raw_fd();
        setsockopt(sock_fd, ReusePort, &true).unwrap();
        let addr = socketaddr!(0, port);
        match sock.bind(&SockAddr::from(addr)) {
            Ok(_) => sock.into_udp_socket(),
            Err(err) => {
                panic!("Failed to bind to {:?}, err: {}", addr, err);
            }
        }
    };

    let mut port = 0;
    let mut addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 0);

    let exit = Arc::new(AtomicBool::new(false));
    let pack_recycler = PacketRecycler::default();

    let mut read_channels = Vec::new();
    let read_threads: Vec<JoinHandle<()>> = (0..num_sockets)
        .into_iter()
        .map(|_| {
            let read = bind_to(port);
            read.set_read_timeout(Some(Duration::new(1, 0))).unwrap();

            addr = read.local_addr().unwrap();
            port = addr.port();

            let (s_reader, r_reader) = channel();
            read_channels.push(r_reader);
            receiver(
                Arc::new(read),
                exit.clone(),
                pack_recycler.clone(),
                s_reader,
            )
        })
        .collect();

    let t_producer1 = producer(&addr, &pack_recycler, exit.clone());
    let t_producer2 = producer(&addr, &pack_recycler, exit.clone());
    let t_producer3 = producer(&addr, &pack_recycler, exit.clone());

    let rvs = Arc::new(AtomicUsize::new(0));
    let sink_threads: Vec<JoinHandle<()>> = read_channels
        .into_iter()
        .map(|r_reader| sink(pack_recycler.clone(), exit.clone(), rvs.clone(), r_reader))
        .collect();
    let start = SystemTime::now();
    let start_val = rvs.load(Ordering::Relaxed);
    sleep(Duration::new(5, 0));
    let elapsed = start.elapsed().unwrap();
    let end_val = rvs.load(Ordering::Relaxed);
    let time = elapsed.as_secs() * 10_000_000_000 + u64::from(elapsed.subsec_nanos());
    let ftime = (time as f64) / 10_000_000_000_f64;
    let fcount = (end_val - start_val) as f64;
    println!("performance: {:?}", fcount / ftime);
    exit.store(true, Ordering::Relaxed);
    for t_reader in read_threads {
        t_reader.join()?;
    }
    t_producer1.join()?;
    t_producer2.join()?;
    t_producer3.join()?;
    for t_sink in sink_threads {
        t_sink.join()?;
    }
    Ok(())
}
