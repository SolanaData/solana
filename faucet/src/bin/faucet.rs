use {
    clap::{crate_description, crate_name, Arg, Command},
    log::*,
    solana_clap_utils::input_parsers::{lamports_of_sol, value_of},
    solana_faucet::{
        faucet::{run_faucet, Faucet, FAUCET_PORT},
        socketaddr,
    },
    solana_sdk::signature::read_keypair_file,
    std::{
        collections::HashSet,
        net::{IpAddr, Ipv4Addr, SocketAddr},
        sync::{Arc, Mutex},
        thread,
    },
};

#[tokio::main]
async fn main() {
    let default_keypair = solana_cli_config::Config::default().keypair_path;

    solana_logger::setup_with_default("solana=info");
    solana_metrics::set_panic_hook("faucet", /*version:*/ None);
    let matches = Command::new(crate_name!())
        .about(crate_description!())
        .version(solana_version::version!())
        .arg(
            Arg::new("keypair")
                .short('k')
                .long("keypair")
                .value_name("PATH")
                .takes_value(true)
                .default_value(&default_keypair)
                .help("File from which to read the faucet's keypair"),
        )
        .arg(
            Arg::new("slice")
                .long("slice")
                .value_name("SECS")
                .takes_value(true)
                .help("Time slice over which to limit requests to faucet"),
        )
        .arg(
            Arg::new("per_time_cap")
                .long("per-time-cap")
                .alias("cap")
                .value_name("NUM")
                .takes_value(true)
                .help("Request limit for time slice, in SOL"),
        )
        .arg(
            Arg::new("per_request_cap")
                .long("per-request-cap")
                .value_name("NUM")
                .takes_value(true)
                .help("Request limit for a single request, in SOL"),
        )
        .arg(
            Arg::new("allowed_ip")
                .long("allow-ip")
                .value_name("IP_ADDRESS")
                .takes_value(true)
                .multiple_occurrences(true)
                .multiple_values(true)
                .help(
                    "Allow requests from a particular IP address without request limit; \
                    recipient address will be used to check request limits instead",
                ),
        )
        .get_matches();

    let faucet_keypair = read_keypair_file(matches.value_of("keypair").unwrap())
        .expect("failed to read client keypair");

    let time_slice = value_of(&matches, "slice");
    let per_time_cap = lamports_of_sol(&matches, "per_time_cap");
    let per_request_cap = lamports_of_sol(&matches, "per_request_cap");

    let allowed_ips: HashSet<_> = matches
        .values_of_t::<IpAddr>("allowed_ip")
        .unwrap_or_default()
        .into_iter()
        .collect();

    let faucet_addr = socketaddr!(0, FAUCET_PORT);

    let faucet = Arc::new(Mutex::new(Faucet::new_with_allowed_ips(
        faucet_keypair,
        time_slice,
        per_time_cap,
        per_request_cap,
        allowed_ips,
    )));

    let faucet1 = faucet.clone();
    thread::spawn(move || loop {
        let time = faucet1.lock().unwrap().time_slice;
        thread::sleep(time);
        debug!("clearing ip cache");
        faucet1.lock().unwrap().clear_caches();
    });

    run_faucet(faucet, faucet_addr, None).await;
}
