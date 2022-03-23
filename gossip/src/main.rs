//! A command-line executable for monitoring a cluster's gossip plane.

use {
    clap::{crate_description, crate_name, Arg, ArgMatches, Command},
    solana_clap_utils::{
        input_parsers::keypair_of,
        input_validators::{is_keypair_or_ask_keyword, is_port, is_pubkey},
    },
    solana_gossip::{contact_info::ContactInfo, gossip_service::discover},
    solana_sdk::pubkey::Pubkey,
    solana_streamer::socket::SocketAddrSpace,
    std::{
        error,
        net::{IpAddr, Ipv4Addr, SocketAddr},
        process::exit,
        time::Duration,
    },
};

fn parse_matches() -> ArgMatches {
    let shred_version_arg = Arg::new("shred_version")
        .long("shred-version")
        .value_name("VERSION")
        .takes_value(true)
        .default_value("0")
        .help("Filter gossip nodes by this shred version");

    Command::new(crate_name!())
        .about(crate_description!())
        .version(solana_version::version!())
        .subcommand_required(true)
        .arg_required_else_help(true)
        .arg(
            Arg::new("allow_private_addr")
                .long("allow-private-addr")
                .takes_value(false)
                .help("Allow contacting private ip addresses")
                .hide(true),
        )
        .subcommand(
            Command::new("rpc-url")
                .about("Get an RPC URL for the cluster")
                .arg(
                    Arg::new("entrypoint")
                        .short('n')
                        .long("entrypoint")
                        .value_name("HOST:PORT")
                        .takes_value(true)
                        .required(true)
                        .validator(|s| solana_net_utils::is_host_port(s.to_string()))
                        .help("Rendezvous with the cluster at this entry point"),
                )
                .arg(
                    Arg::new("all")
                        .long("all")
                        .takes_value(false)
                        .help("Return all RPC URLs"),
                )
                .arg(
                    Arg::new("any")
                        .long("any")
                        .takes_value(false)
                        .conflicts_with("all")
                        .help("Return any RPC URL"),
                )
                .arg(
                    Arg::new("timeout")
                        .long("timeout")
                        .value_name("SECONDS")
                        .takes_value(true)
                        .default_value("15")
                        .help("Timeout in seconds"),
                )
                .arg(&shred_version_arg)
                .disable_version_flag(true),
        )
        .subcommand(
            Command::new("spy")
                .about("Monitor the gossip entrypoint")
                .disable_version_flag(true)
                .arg(
                    Arg::new("entrypoint")
                        .short('n')
                        .long("entrypoint")
                        .value_name("HOST:PORT")
                        .takes_value(true)
                        .validator(|s| solana_net_utils::is_host_port(s.to_string()))
                        .help("Rendezvous with the cluster at this entrypoint"),
                )
                .arg(
                    clap::Arg::new("gossip_port")
                        .long("gossip-port")
                        .value_name("PORT")
                        .takes_value(true)
                        .validator(|s| is_port(s))
                        .help("Gossip port number for the node"),
                )
                .arg(
                    clap::Arg::new("gossip_host")
                        .long("gossip-host")
                        .value_name("HOST")
                        .takes_value(true)
                        .validator(|s| solana_net_utils::is_host(s.to_string()))
                        .help("Gossip DNS name or IP address for the node to advertise in gossip \
                               [default: ask --entrypoint, or 127.0.0.1 when --entrypoint is not provided]"),
                )
                .arg(
                    Arg::new("identity")
                        .short('i')
                        .long("identity")
                        .value_name("PATH")
                        .takes_value(true)
                        .validator(|s| is_keypair_or_ask_keyword(s))
                        .help("Identity keypair [default: ephemeral keypair]"),
                )
                .arg(
                    Arg::new("num_nodes")
                        .short('N')
                        .long("num-nodes")
                        .value_name("NUM")
                        .takes_value(true)
                        .conflicts_with("num_nodes_exactly")
                        .help("Wait for at least NUM nodes to be visible"),
                )
                .arg(
                    Arg::new("num_nodes_exactly")
                        .short('E')
                        .long("num-nodes-exactly")
                        .value_name("NUM")
                        .takes_value(true)
                        .help("Wait for exactly NUM nodes to be visible"),
                )
                .arg(
                    Arg::new("node_pubkey")
                        .short('p')
                        .long("pubkey")
                        .value_name("PUBKEY")
                        .takes_value(true)
                        .validator(|s| is_pubkey(s))
                        .help("Public key of a specific node to wait for"),
                )
                .arg(&shred_version_arg)
                .arg(
                    Arg::new("timeout")
                        .long("timeout")
                        .value_name("SECONDS")
                        .takes_value(true)
                        .help("Maximum time to wait in seconds [default: wait forever]"),
                ),
        )
        .get_matches()
}

fn parse_gossip_host(matches: &ArgMatches, entrypoint_addr: Option<SocketAddr>) -> IpAddr {
    matches
        .value_of("gossip_host")
        .map(|gossip_host| {
            solana_net_utils::parse_host(gossip_host).unwrap_or_else(|e| {
                eprintln!("failed to parse gossip-host: {}", e);
                exit(1);
            })
        })
        .unwrap_or_else(|| {
            if let Some(entrypoint_addr) = entrypoint_addr {
                solana_net_utils::get_public_ip_addr(&entrypoint_addr).unwrap_or_else(|err| {
                    eprintln!(
                        "Failed to contact cluster entrypoint {}: {}",
                        entrypoint_addr, err
                    );
                    exit(1);
                })
            } else {
                IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))
            }
        })
}

fn process_spy_results(
    timeout: Option<u64>,
    validators: Vec<ContactInfo>,
    num_nodes: Option<usize>,
    num_nodes_exactly: Option<usize>,
    pubkey: Option<Pubkey>,
) {
    if timeout.is_some() {
        if let Some(num) = num_nodes {
            if validators.len() < num {
                let add = if num_nodes_exactly.is_some() {
                    ""
                } else {
                    " or more"
                };
                eprintln!(
                    "Error: Insufficient validators discovered.  Expecting {}{}",
                    num, add,
                );
                exit(1);
            }
        }
        if let Some(node) = pubkey {
            if !validators.iter().any(|x| x.id == node) {
                eprintln!("Error: Could not find node {:?}", node);
                exit(1);
            }
        }
    }
    if let Some(num_nodes_exactly) = num_nodes_exactly {
        if validators.len() > num_nodes_exactly {
            eprintln!(
                "Error: Extra nodes discovered.  Expecting exactly {}",
                num_nodes_exactly
            );
            exit(1);
        }
    }
}

fn process_spy(matches: &ArgMatches, socket_addr_space: SocketAddrSpace) -> std::io::Result<()> {
    let num_nodes_exactly = matches
        .value_of("num_nodes_exactly")
        .map(|num| num.to_string().parse().unwrap());
    let num_nodes = matches
        .value_of("num_nodes")
        .map(|num| num.to_string().parse().unwrap())
        .or(num_nodes_exactly);
    let timeout = matches
        .value_of("timeout")
        .map(|secs| secs.to_string().parse().unwrap());
    let pubkey = matches
        .value_of("node_pubkey")
        .map(|pubkey_str| pubkey_str.parse::<Pubkey>().unwrap());
    let shred_version: u16 = matches.value_of_t_or_exit("shred_version");
    let identity_keypair = keypair_of(matches, "identity");

    let entrypoint_addr = parse_entrypoint(matches);

    let gossip_host = parse_gossip_host(matches, entrypoint_addr);

    let gossip_addr = SocketAddr::new(
        gossip_host,
        matches
            .value_of_t::<u16>("gossip_port")
            .unwrap_or_else(|_| {
                solana_net_utils::find_available_port_in_range(
                    IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
                    (0, 1),
                )
                .expect("unable to find an available gossip port")
            }),
    );
    let discover_timeout = Duration::from_secs(timeout.unwrap_or(u64::MAX));
    let (_all_peers, validators) = discover(
        identity_keypair,
        entrypoint_addr.as_ref(),
        num_nodes,
        discover_timeout,
        pubkey,             // find_node_by_pubkey
        None,               // find_node_by_gossip_addr
        Some(&gossip_addr), // my_gossip_addr
        shred_version,
        socket_addr_space,
    )?;

    process_spy_results(timeout, validators, num_nodes, num_nodes_exactly, pubkey);

    Ok(())
}

fn parse_entrypoint(matches: &ArgMatches) -> Option<SocketAddr> {
    matches.value_of("entrypoint").map(|entrypoint| {
        solana_net_utils::parse_host_port(entrypoint).unwrap_or_else(|e| {
            eprintln!("failed to parse entrypoint address: {}", e);
            exit(1);
        })
    })
}

fn process_rpc_url(
    matches: &ArgMatches,
    socket_addr_space: SocketAddrSpace,
) -> std::io::Result<()> {
    let any = matches.is_present("any");
    let all = matches.is_present("all");
    let entrypoint_addr = parse_entrypoint(matches);
    let timeout: u64 = matches.value_of_t_or_exit("timeout");
    let shred_version: u16 = matches.value_of_t_or_exit("shred_version");
    let (_all_peers, validators) = discover(
        None, // keypair
        entrypoint_addr.as_ref(),
        Some(1), // num_nodes
        Duration::from_secs(timeout),
        None,                     // find_node_by_pubkey
        entrypoint_addr.as_ref(), // find_node_by_gossip_addr
        None,                     // my_gossip_addr
        shred_version,
        socket_addr_space,
    )?;

    let rpc_addrs: Vec<_> = validators
        .iter()
        .filter_map(|contact_info| {
            if (any || all || Some(contact_info.gossip) == entrypoint_addr)
                && ContactInfo::is_valid_address(&contact_info.rpc, &socket_addr_space)
            {
                return Some(contact_info.rpc);
            }
            None
        })
        .collect();

    if rpc_addrs.is_empty() {
        eprintln!("No RPC URL found");
        exit(1);
    }

    for rpc_addr in rpc_addrs {
        println!("http://{}", rpc_addr);
        if any {
            break;
        }
    }

    Ok(())
}

fn main() -> Result<(), Box<dyn error::Error>> {
    solana_logger::setup_with_default("solana=info");

    let matches = parse_matches();
    let socket_addr_space = SocketAddrSpace::new(matches.is_present("allow_private_addr"));
    match matches.subcommand() {
        Some(("spy", matches)) => {
            process_spy(matches, socket_addr_space)?;
        }
        Some(("rpc-url", matches)) => {
            process_rpc_url(matches, socket_addr_space)?;
        }
        _ => unreachable!(),
    }

    Ok(())
}
