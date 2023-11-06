use clap::{App, Arg};

pub fn cli_app<'a, 'b>() -> App<'a, 'b> {
    App::new("light_client")
        .visible_aliases(&["l", "lc", "light"])
        .setting(clap::AppSettings::ColoredHelp)
        .about("")
        .arg(
            Arg::with_name("beacon-node")
                .long("beacon-node")
                .value_name("NETWORK_ADDRESS")
                .help("The address to a beacon node HTTP API server.")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("execution-endpoint")
                .long("execution-endpoint")
                .value_name("EXECUTION-ENDPOINT")
                .alias("execution-endpoints")
                .help("Server endpoint for an execution layer JWT-authenticated HTTP \
                       JSON-RPC connection. Uses the same endpoint to populate the \
                       deposit cache.")
                .takes_value(true)
        )
        .arg(
            Arg::with_name("execution-jwt")
                .long("execution-jwt")
                .value_name("EXECUTION-JWT")
                .alias("jwt-secrets")
                .help("File path which contains the hex-encoded JWT secret for the \
                       execution endpoint provided in the --execution-endpoint flag.")
                .requires("execution-endpoint")
                .takes_value(true)
        )
        .arg(
            Arg::with_name("execution-jwt-secret-key")
                .long("execution-jwt-secret-key")
                .value_name("EXECUTION-JWT-SECRET-KEY")
                .alias("jwt-secret-key")
                .help("Hex-encoded JWT secret for the \
                       execution endpoint provided in the --execution-endpoint flag.")
                .requires("execution-endpoint")
                .conflicts_with("execution-jwt")
                .takes_value(true)
        )
        .arg(
            Arg::with_name("execution-jwt-id")
                .long("execution-jwt-id")
                .value_name("EXECUTION-JWT-ID")
                .alias("jwt-id")
                .help("Used by the beacon node to communicate a unique identifier to execution nodes \
                       during JWT authentication. It corresponds to the 'id' field in the JWT claims object.\
                       Set to empty by default")
                .requires("execution-jwt")
                .takes_value(true)
        )
        .arg(
            Arg::with_name("execution-jwt-version")
                .long("execution-jwt-version")
                .value_name("EXECUTION-JWT-VERSION")
                .alias("jwt-version")
                .help("Used by the beacon node to communicate a client version to execution nodes \
                       during JWT authentication. It corresponds to the 'clv' field in the JWT claims object.\
                       Set to empty by default")
                .requires("execution-jwt")
                .takes_value(true)
        )
        .arg(
            Arg::with_name("execution-timeout-multiplier")
                .long("execution-timeout-multiplier")
                .value_name("NUM")
                .help("Unsigned integer to multiply the default execution timeouts by.")
                .default_value("1")
                .takes_value(true)
        )
        .arg(
            Arg::with_name("checkpoint-root")
                .long("checkpoint-root")
                .help("Set a checkpoint root to start syncing from.")
                .value_name("HASH256")
                .takes_value(true)
        )
}
