//! `cubelark-server [--port 7878] [--data server-data]`: runs a world others can join
//! with `CUBELARK_SERVER=host:port cargo run`.

use std::path::PathBuf;

use cubelark_server::{ServerConfig, start};

fn main() {
    let mut config = ServerConfig { data_dir: "server-data".into(), port: cubelark_net::DEFAULT_PORT, seed: 1337 };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match (arg.as_str(), args.next()) {
            ("--port", Some(port)) => config.port = port.parse().expect("--port takes a number"),
            ("--data", Some(dir)) => config.data_dir = PathBuf::from(dir),
            _ => {
                eprintln!("usage: cubelark-server [--port 7878] [--data server-data]");
                std::process::exit(2);
            }
        }
    }
    let running = start(config).expect("couldn't start the server");
    println!("Cubelark server listening on {}", running.addr);
    loop {
        std::thread::park();
    }
}
