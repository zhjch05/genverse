//! `genverse-server [--port 7878] [--data server-data] [--flat]`: hosts a game, every
//! world in it, for others to join with `GENVERSE_SERVER=host:port cargo run`. `--flat`
//! makes a new game's first world flat and empty. A local game can be served as it is:
//! `--data saves/games/<game>`.

use std::path::PathBuf;

use genverse_server::{ServerConfig, start};

fn main() {
    let mut config = ServerConfig {
        data_dir: "server-data".into(),
        port: genverse_net::DEFAULT_PORT,
        seed: 1337,
        flat: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--flat" {
            config.flat = true;
            continue;
        }
        match (arg.as_str(), args.next()) {
            ("--port", Some(port)) => config.port = port.parse().expect("--port takes a number"),
            ("--data", Some(dir)) => config.data_dir = PathBuf::from(dir),
            _ => {
                eprintln!("usage: genverse-server [--port 7878] [--data server-data] [--flat]");
                std::process::exit(2);
            }
        }
    }
    let running = start(config).expect("couldn't start the server");
    println!("Genverse server listening on {}", running.addr);
    loop {
        std::thread::park();
    }
}
