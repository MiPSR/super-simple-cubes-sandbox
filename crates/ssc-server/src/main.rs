//! `ssc-server` — dedicated (headless) server entry point.
//!
//! The game logic itself lives in the library part of this crate; this
//! binary accepts TCP clients and runs one authoritative session per
//! client, sequentially (v0: single client at a time; multi-player comes
//! with the entity system).

use std::net::TcpListener;

use ssc_protocol::wire::TcpWire;
use ssc_server::{Server, ServerConfig};

fn main() {
    let mut cfg = ServerConfig::default();
    let mut port: u16 = 25565;
    let mut seed_specified = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => {
                if let Some(v) = args.next() {
                    port = v.parse().unwrap_or_else(|_| {
                        eprintln!("bad --port value {v:?}, using {port}");
                        port
                    });
                }
            }
            "--seed" => {
                if let Some(v) = args.next() {
                    if let Ok(seed) = v.parse::<u64>() {
                        cfg.seed = seed;
                        seed_specified = true;
                    } else {
                        eprintln!("bad --seed value {v:?}, keeping {}", cfg.seed);
                    }
                }
            }
            "--rd" => {
                if let Some(v) = args.next() {
                    if let Ok(rd) = v.parse::<i32>() {
                        cfg.render_distance = rd.clamp(1, 32);
                    }
                }
            }
            "--lod" => {
                if let Some(v) = args.next() {
                    if let Ok(ld) = v.parse::<i32>() {
                        cfg.lod_distance = ld.clamp(cfg.render_distance, 128);
                    }
                }
            }
            other => eprintln!("unknown argument {other:?}"),
        }
    }
    if !seed_specified {
        // Random-ish default seed for a dedicated box.
        cfg.seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(cfg.seed);
    }

    let version = env!("CARGO_PKG_VERSION");
    println!("ssc-server {version} (dedicated) — Super Simple Cubes");
    println!("seed: {}, render distance: {}, lod distance: {}",
        cfg.seed, cfg.render_distance, cfg.lod_distance);

    let listener = TcpListener::bind(("0.0.0.0", port))
        .unwrap_or_else(|e| panic!("cannot bind 0.0.0.0:{port}: {e}"));
    println!("listening on 0.0.0.0:{port}");

    loop {
        let (stream, peer) = match listener.accept() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("accept error: {e}");
                continue;
            }
        };
        println!("client connected: {peer}");
        let wire = match TcpWire::new(stream) {
            Ok(w) => Box::new(w) as Box<dyn ssc_protocol::wire::Wire>,
            Err(e) => {
                eprintln!("wire setup failed for {peer}: {e}");
                continue;
            }
        };
        Server::new(cfg.clone(), wire).run();
        println!("client disconnected: {peer}");
    }
}
