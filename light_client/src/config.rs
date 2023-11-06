use clap::ArgMatches;
use clap_utils::{parse_optional, parse_required};
use directory::DEFAULT_ROOT_DIR;
use execution_layer::DEFAULT_JWT_FILE;
use sensitive_url::SensitiveUrl;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;
use types::Hash256;

/// The core configuration of a Lighthouse light client node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LightClientConfig {
    data_dir: PathBuf,
    /// The http endpoint to the beacon API server.
    pub beacon_node: Option<SensitiveUrl>,
    pub execution_layer: execution_layer::Config,
    pub checkpoint_root: Hash256,
    pub genesis_state_url: Option<String>,
    pub genesis_state_url_timeout: Duration,
}

impl Default for LightClientConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from(DEFAULT_ROOT_DIR),
            beacon_node: None,
            execution_layer: <_>::default(),
            checkpoint_root: <_>::default(),
            genesis_state_url: <_>::default(),
            // This default value should always be overwritten by the CLI default value.
            genesis_state_url_timeout: Duration::from_secs(60),
        }
    }
}

impl LightClientConfig {
    /// Returns a `Default` implementation of `Self` with some parameters modified by the supplied
    /// `cli_args`.
    pub fn from_cli(cli_args: &ArgMatches) -> Result<Self, String> {
        let mut config = LightClientConfig::default();

        if let Some(beacon_node) = parse_optional::<String>(cli_args, "beacon-node")? {
            config.beacon_node = Some(
                SensitiveUrl::parse(&beacon_node)
                    .map_err(|e| format!("Unable to parse beacon node URL: {:?}", e))?,
            );
        }

        config.genesis_state_url = parse_optional::<String>(cli_args, "genesis-state-url")?;
        config.genesis_state_url_timeout =
            parse_required(cli_args, "genesis-state-url-timeout").map(Duration::from_secs)?;

        if let Some(endpoint) = cli_args.value_of("execution-endpoint") {
            let execution_endpoint = SensitiveUrl::parse(endpoint)
                .map_err(|e| format!("execution-endpoint contains an invalid value {:?}", e))?;

            let secret_file: PathBuf;
            // Parse a single JWT secret from a given file_path
            if let Some(secret) = cli_args.value_of("execution-jwt") {
                secret_file = PathBuf::from_str(secret)
                    .map_err(|e| format!("execution-jwt contains an invalid value {:?}", e))?;

            // Check if the JWT secret key is passed directly via cli flag and persist it to the default
            // file location.
            } else if let Some(jwt_secret_key) = cli_args.value_of("execution-jwt-secret-key") {
                secret_file = config.data_dir().join(DEFAULT_JWT_FILE);
                let mut jwt_secret_key_file = File::create(secret_file.clone())
                    .map_err(|e| format!("Error while creating jwt_secret_key file: {:?}", e))?;
                jwt_secret_key_file
                    .write_all(jwt_secret_key.as_bytes())
                    .map_err(|e| {
                        format!(
                            "Error occurred while writing to jwt_secret_key file: {:?}",
                            e
                        )
                    })?;
            } else {
                return Err("Error! Please set either --execution-jwt file_path or --execution-jwt-secret-key directly via cli when using --execution-endpoint".to_string());
            }

            config.execution_layer = execution_layer::Config {
                execution_endpoints: vec![execution_endpoint.clone()],
                secret_files: vec![secret_file.clone()],
                jwt_id: parse_optional(cli_args, "execution-jwt-id")?,
                jwt_version: parse_optional(cli_args, "execution-jwt-version")?,
                default_datadir: config.data_dir().clone(),
                execution_timeout_multiplier: Some(parse_required(
                    cli_args,
                    "execution-timeout-multiplier",
                )?),
                ..Default::default()
            };
        }

        config.checkpoint_root = parse_required(cli_args, "checkpoint-root")?;

        Ok(config)
    }

    /// Gets the config's data_dir.
    pub fn data_dir(&self) -> &PathBuf {
        &self.data_dir
    }
}
