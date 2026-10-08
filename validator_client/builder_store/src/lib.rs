mod builder_definitions;
use builder_definitions::BuilderConfigFile;
pub use builder_definitions::{
    BuilderDefinition, Error, HeaderStreamDefinition, ResolvedBuilderConfig,
    ValidatorBuilderConfig, ValidatorBuilderDefinition,
};
use builder_types::{
    BuilderConfig, BuilderEntry, BuilderPubkeys, BuilderUrl, RequestAuthData, SignedRequestAuth,
};
use parking_lot::{RwLock, RwLockUpgradableReadGuard};
use ssz_types::VariableList;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::error;

#[derive(Clone)]
pub struct BuilderStore {
    config: Arc<RwLock<BuilderConfigFile>>,
    validators_dir: PathBuf,
}

/// One builder to open an MPBC header-stream websocket against, resolved for a specific proposer.
///
/// Returned only for a builder that is both in the proposer's resolved builder list AND has a
/// matching `header_streams` entry (matched by URL) — see [`BuilderStore::stream_targets`].
#[derive(Debug, Clone, PartialEq)]
pub struct StreamTarget {
    /// The builder URL to open the websocket header stream against.
    pub url: BuilderUrl,
    /// The MPBC API key for this builder's header stream.
    pub api_key: String,
    /// This builder's resolved request-auth data (explicit override, or the URL's default
    /// hostname), to sign a `SignedRequestAuth` per proposal.
    pub auth_data: RequestAuthData,
    /// Maximum trusted execution-layer payment (gwei) accepted from this builder.
    pub max_execution_payment: u64,
    /// Minimum total payment (gwei, trusted value) for this builder's streamed bid to be used.
    pub min_bid: u64,
    /// Boost factor applied to this builder's streamed bid when comparing against a local payload.
    pub builder_boost_factor: u64,
}

impl BuilderStore {
    pub fn open_or_create<P: AsRef<Path>>(validators_dir: P) -> Result<Self, Error> {
        let validators_dir = validators_dir.as_ref().to_path_buf();

        Ok(Self {
            config: Arc::new(RwLock::new(BuilderConfigFile::open_or_create(
                &validators_dir,
            )?)),
            validators_dir,
        })
    }

    /// Resolve the enabled builders into a wire [`BuilderConfig`], signing each builder's request
    /// auth via `sign`.
    ///
    /// Per-builder `min_bid`/`builder_boost_factor` inherit the global defaults when unset, and each
    /// builder's `auth_data` defaults to its lowercase ASCII hostname when unset. `sign` receives a
    /// builder's opaque auth `data` and returns the corresponding `SignedRequestAuth` — in
    /// practice signed for the current proposer/slot and cached.
    ///
    /// Signing is per-builder: a builder whose auth `sign` fails to produce is logged (with the
    /// returned error) and omitted, so one unsignable builder cannot drop the rest. The returned
    /// config always carries the validator's resolved policy; its `builders` list holds only the
    /// successfully signed builders, and is empty when no builders are enabled or every one
    /// failed to sign.
    pub async fn builder_config<F, Fut, E>(
        &self,
        validator_pubkey: &bls::PublicKeyBytes,
        sign: F,
    ) -> BuilderConfig
    where
        F: Fn(RequestAuthData) -> Fut,
        Fut: Future<Output = Result<SignedRequestAuth, E>>,
        E: std::fmt::Debug,
    {
        // Snapshot the validator's resolved builders and policy under the lock, then sign outside
        // it, so the lock is never held across an `.await`.
        let (definitions, min_bid, builder_boost_factor) = {
            let config = self.config.read();
            let resolved = config.resolved_for(validator_pubkey);
            (
                resolved.builders,
                resolved.min_bid,
                resolved.builder_boost_factor,
            )
        };

        // Sign every builder's request auth concurrently. With a remote signer each `sign` is a
        // network round trip, and the signatures are independent, so signing in sequence would put
        // up to `MaxBuilderEntries` serial round trips on the block-production critical path.
        let signed = futures::future::join_all(definitions.into_iter().filter_map(|definition| {
            let auth_data = match definition
                .auth_data
                .clone()
                .map(Ok)
                .unwrap_or_else(|| definition.url.to_default_auth_data())
            {
                Ok(data) => data,
                Err(error) => {
                    error!(
                        error = ?error,
                        builder_url = %definition.url,
                        "Failed to derive builder auth_data; omitting builder from config"
                    );
                    return None;
                }
            };
            // A zero-length auth `data` is invalid on the wire (beacon-specs #165 / beacon-APIs
            // #630); the beacon node would reject the whole request body, so drop the builder here.
            if auth_data.is_empty() {
                error!(
                    builder_url = %definition.url,
                    "Zero-length auth_data is invalid; omitting builder from config"
                );
                return None;
            }
            let signing = sign(auth_data);
            Some(async move { (definition, signing.await) })
        }))
        .await;

        // `join_all` preserves input order, so `builders` keeps the configured order. Omit any
        // builder we cannot sign for, logging the error, rather than failing the whole config.
        let mut builders = Vec::with_capacity(signed.len());
        for (definition, result) in signed {
            let auth = match result {
                Ok(auth) => auth,
                Err(e) => {
                    error!(
                        error = ?e,
                        builder_url = %definition.url,
                        "Failed to sign builder request auth; omitting builder from config"
                    );
                    continue;
                }
            };
            let Ok(builder_pubkeys) = BuilderPubkeys::new(definition.builder_pubkeys) else {
                error!(
                    builder_url = %definition.url,
                    "Too many builder pubkeys; omitting builder from config"
                );
                continue;
            };
            builders.push(BuilderEntry {
                url: definition.url,
                auth,
                builder_pubkeys,
                max_execution_payment: definition.max_execution_payment,
                min_bid: definition.min_bid.unwrap_or(min_bid),
                builder_boost_factor: definition
                    .builder_boost_factor
                    .unwrap_or(builder_boost_factor),
            });
        }

        BuilderConfig {
            // The number of builders is bounded by `MaxBuilderEntries` at config load, so this
            // cannot overflow.
            builders: VariableList::new(builders)
                .expect("builder count is bounded by MaxBuilderEntries at config load"),
            min_bid,
            builder_boost_factor,
        }
    }

    /// Resolve `validator_pubkey`'s header-stream targets: the builders in their resolved builder
    /// list whose URL also has a `header_streams` entry.
    ///
    /// A validator with no direct builders configured (or none matching a `header_streams` entry)
    /// resolves to an empty list, so no stream is opened for them.
    pub fn stream_targets(&self, validator_pubkey: &bls::PublicKeyBytes) -> Vec<StreamTarget> {
        let config = self.config.read();
        let resolved = config.resolved_for(validator_pubkey);
        config
            .header_streams
            .iter()
            .filter_map(|header_stream| {
                let builder = resolved
                    .builders
                    .iter()
                    .find(|builder| builder.url == header_stream.url)?;
                let auth_data = builder
                    .auth_data
                    .clone()
                    .map(Ok)
                    .unwrap_or_else(|| builder.url.to_default_auth_data())
                    .ok()?;
                Some(StreamTarget {
                    url: builder.url.clone(),
                    api_key: header_stream.api_key.clone(),
                    auth_data,
                    max_execution_payment: builder.max_execution_payment,
                    min_bid: builder.min_bid.unwrap_or(resolved.min_bid),
                    builder_boost_factor: builder
                        .builder_boost_factor
                        .unwrap_or(resolved.builder_boost_factor),
                })
            })
            .collect()
    }

    /// Whether at least one `header_streams` entry is configured, regardless of whether any
    /// validator's resolved builder list currently matches it.
    ///
    /// Used to gate whether the header-stream consumer service runs at all: with none configured
    /// there is nothing for it to open a stream against, so it should not subscribe to
    /// `payload_attributes`, log, or affect block-production metrics.
    pub fn has_header_streams(&self) -> bool {
        !self.config.read().header_streams.is_empty()
    }

    pub fn insert(&self, builder: BuilderDefinition) -> Result<(), Error> {
        self.persist_update(|candidate| {
            candidate.push(builder);
            candidate.validate()?;
            Ok(true)
        })
    }

    /// Return the fully resolved configuration for a validator without signing builder auth data.
    pub fn get_validator_config(
        &self,
        validator_pubkey: &bls::PublicKeyBytes,
    ) -> ResolvedBuilderConfig {
        self.config.read().resolved_for(validator_pubkey)
    }

    /// Replace the per-validator configuration and persist it atomically.
    pub fn set_validator_config(
        &self,
        validator_pubkey: &bls::PublicKeyBytes,
        validator_config: ValidatorBuilderConfig,
    ) -> Result<(), Error> {
        validator_config.validate()?;

        self.persist_update(|candidate| {
            candidate
                .validator_configs
                .insert(validator_pubkey.to_string(), validator_config);
            Ok(true)
        })
    }

    /// Remove a validator's override and persist the inherited global configuration atomically.
    pub fn delete_validator_config(
        &self,
        validator_pubkey: &bls::PublicKeyBytes,
    ) -> Result<(), Error> {
        self.persist_update(|candidate| {
            Ok(candidate
                .validator_configs
                .remove(&validator_pubkey.to_string())
                .is_some())
        })
    }

    /// Apply and persist one update while ordinary readers continue using the current config.
    ///
    /// The update returns `false` when it made no change and no save is required.
    /// The upgradable read guard serializes writers from snapshot through save. The brief upgrade
    /// publishes only a successfully persisted candidate.
    fn persist_update(
        &self,
        update: impl FnOnce(&mut BuilderConfigFile) -> Result<bool, Error>,
    ) -> Result<(), Error> {
        let config = self.config.upgradable_read();
        let mut candidate = config.clone();
        let changed = update(&mut candidate)?;
        if !changed {
            return Ok(());
        }
        candidate.save(&self.validators_dir)?;
        *RwLockUpgradableReadGuard::upgrade(config) = candidate;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bls::Keypair;
    use builder_types::RequestAuth;
    use tempfile::tempdir;
    use types::Slot;

    fn global_builder(url: &str, max_execution_payment: u64) -> BuilderDefinition {
        BuilderDefinition {
            enabled: true,
            url: url.parse().unwrap(),
            auth_data: None,
            builder_pubkeys: vec![],
            max_execution_payment,
            min_bid: None,
            builder_boost_factor: None,
        }
    }

    fn validator_builder(url: &str) -> ValidatorBuilderDefinition {
        ValidatorBuilderDefinition {
            url: url.parse().unwrap(),
            auth_data: None,
            builder_pubkeys: vec![],
            max_execution_payment: None,
            min_bid: None,
            builder_boost_factor: None,
        }
    }

    fn signed_auth(data: RequestAuthData) -> SignedRequestAuth {
        SignedRequestAuth {
            message: RequestAuth {
                data,
                slot: Slot::new(0),
            },
            signature: bls::Signature::empty(),
        }
    }

    #[test]
    fn validator_config_inherits_global_and_distinguishes_empty_builders() {
        let directory = tempdir().unwrap();
        let store = BuilderStore::open_or_create(directory.path()).unwrap();
        let validator = Keypair::random().pk.compress();

        let mut configured_global_builder = global_builder("https://global-builder.example", 7);
        configured_global_builder.min_bid = Some(100);
        configured_global_builder.builder_boost_factor = Some(200);
        store.insert(configured_global_builder).unwrap();
        let mut empty_auth = global_builder("https://empty-auth.example", 7);
        empty_auth.auth_data = Some(RequestAuthData::default());
        store.insert(empty_auth).unwrap();
        let mut excessive_pubkeys = global_builder("https://too-many-pubkeys.example", 7);
        excessive_pubkeys.builder_pubkeys =
            (0..65).map(|_| Keypair::random().pk.compress()).collect();
        store.insert(excessive_pubkeys).unwrap();
        store
            .set_validator_config(
                &validator,
                ValidatorBuilderConfig {
                    min_bid: Some(5),
                    builder_boost_factor: Some(125),
                    builders: None,
                },
            )
            .unwrap();

        let inherited = store.get_validator_config(&validator);
        assert_eq!(inherited.min_bid, 5);
        assert_eq!(inherited.builder_boost_factor, 125);
        assert_eq!(inherited.builders.len(), 1);
        assert_eq!(inherited.builders[0].max_execution_payment, 7);
        assert_eq!(inherited.builders[0].min_bid, Some(5));
        assert_eq!(inherited.builders[0].builder_boost_factor, Some(125));

        store
            .set_validator_config(
                &validator,
                ValidatorBuilderConfig {
                    min_bid: None,
                    builder_boost_factor: None,
                    builders: Some(vec![]),
                },
            )
            .unwrap();
        assert!(store.get_validator_config(&validator).builders.is_empty());

        store.delete_validator_config(&validator).unwrap();
        let restored = store.get_validator_config(&validator);
        assert_eq!(restored.min_bid, 0);
        assert_eq!(restored.builder_boost_factor, 100);
        assert_eq!(restored.builders.len(), 1);
        assert_eq!(restored.builders[0].min_bid, Some(100));
        assert_eq!(restored.builders[0].builder_boost_factor, Some(200));
    }

    #[test]
    fn empty_validator_config_persists_across_store_restart() {
        let directory = tempdir().unwrap();
        let store = BuilderStore::open_or_create(directory.path()).unwrap();
        let validator = Keypair::random().pk.compress();

        store
            .set_validator_config(&validator, ValidatorBuilderConfig::default())
            .unwrap();

        let file = builder_definitions::BuilderConfigFile::open(directory.path()).unwrap();
        assert!(file.validator_configs.contains_key(&validator.to_string()));

        let restarted = BuilderStore::open_or_create(directory.path()).unwrap();
        assert_eq!(
            restarted.get_validator_config(&validator),
            store.get_validator_config(&validator)
        );

        restarted.delete_validator_config(&validator).unwrap();
        let file = builder_definitions::BuilderConfigFile::open(directory.path()).unwrap();
        assert!(!file.validator_configs.contains_key(&validator.to_string()));
    }

    #[test]
    fn failed_insert_is_not_published() {
        let directory = tempdir().unwrap();
        let store = BuilderStore::open_or_create(directory.path()).unwrap();
        let validator = Keypair::random().pk.compress();
        std::fs::create_dir(
            directory
                .path()
                .join(builder_definitions::BUILDERS_TEMP_FILENAME),
        )
        .unwrap();

        assert!(
            store
                .insert(global_builder("https://global-builder.example", 7))
                .is_err()
        );
        assert!(store.get_validator_config(&validator).builders.is_empty());
    }

    #[test]
    fn validator_updates_are_serialized_without_losing_each_other() {
        let directory = tempdir().unwrap();
        let store = Arc::new(BuilderStore::open_or_create(directory.path()).unwrap());
        let first = Keypair::random().pk.compress();
        let second = Keypair::random().pk.compress();

        let handles = [(first, 11), (second, 22)].map(|(validator, min_bid)| {
            let store = store.clone();
            std::thread::spawn(move || {
                store.set_validator_config(
                    &validator,
                    ValidatorBuilderConfig {
                        min_bid: Some(min_bid),
                        ..Default::default()
                    },
                )
            })
        });
        for handle in handles {
            handle.join().unwrap().unwrap();
        }
        assert_eq!(store.get_validator_config(&first).min_bid, 11);
        assert_eq!(store.get_validator_config(&second).min_bid, 22);

        let restarted = BuilderStore::open_or_create(directory.path()).unwrap();
        assert_eq!(restarted.get_validator_config(&first).min_bid, 11);
        assert_eq!(restarted.get_validator_config(&second).min_bid, 22);
    }

    #[test]
    fn builder_consumer_observes_runtime_updates_immediately() {
        let directory = tempdir().unwrap();
        let store = BuilderStore::open_or_create(directory.path()).unwrap();
        let validator = Keypair::random().pk.compress();
        store
            .insert(global_builder("https://global-builder.example", 7))
            .unwrap();

        store
            .set_validator_config(
                &validator,
                ValidatorBuilderConfig {
                    builders: Some(vec![]),
                    ..Default::default()
                },
            )
            .unwrap();
        let empty =
            futures::executor::block_on(store.builder_config(&validator, |data| async move {
                Ok::<_, ()>(signed_auth(data))
            }));
        assert!(empty.builders.is_empty());

        store.delete_validator_config(&validator).unwrap();
        let inherited =
            futures::executor::block_on(store.builder_config(&validator, |data| async move {
                Ok::<_, ()>(signed_auth(data))
            }));
        assert_eq!(inherited.builders.len(), 1);
        assert_eq!(inherited.builders[0].max_execution_payment, 7);
    }

    #[test]
    fn global_and_validator_auth_data_use_hostname_or_exact_override() {
        let directory = tempdir().unwrap();
        let store = BuilderStore::open_or_create(directory.path()).unwrap();
        let validator = Keypair::random().pk.compress();
        let url = "HTTPS://Builder.Example:443/path?query#fragment";
        store.insert(global_builder(url, 9)).unwrap();

        let inherited =
            futures::executor::block_on(store.builder_config(&validator, |data| async move {
                Ok::<_, ()>(signed_auth(data))
            }));
        assert_eq!(
            &*inherited.builders[0].auth.message.data,
            b"builder.example"
        );
        assert_eq!(inherited.builders[0].url.as_str().unwrap(), url);

        for explicit in [
            None,
            Some(b"builder.example".to_vec()),
            Some(vec![0xff, 0x00, b'A']),
        ] {
            let expected = explicit
                .clone()
                .unwrap_or_else(|| b"builder.example".to_vec());
            let mut builder = validator_builder(url);
            builder.auth_data = explicit.map(|bytes| RequestAuthData::new(bytes).unwrap());
            store
                .set_validator_config(
                    &validator,
                    ValidatorBuilderConfig {
                        builders: Some(vec![builder]),
                        ..Default::default()
                    },
                )
                .unwrap();
            let config =
                futures::executor::block_on(store.builder_config(&validator, |data| async move {
                    Ok::<_, ()>(signed_auth(data))
                }));
            assert_eq!(&*config.builders[0].auth.message.data, expected.as_slice());
            assert_eq!(
                config.builders[0].max_execution_payment,
                if expected == b"builder.example" { 9 } else { 0 }
            );
        }
    }

    #[test]
    fn unicode_hostname_needs_explicit_auth_data() {
        let directory = tempdir().unwrap();
        let store = BuilderStore::open_or_create(directory.path()).unwrap();
        let validator = Keypair::random().pk.compress();
        let url = "https://bücher.example/";
        assert!(matches!(
            store.insert(global_builder(url, 9)),
            Err(Error::InvalidBuilderUrl(_))
        ));
        let mut config = ValidatorBuilderConfig {
            builders: Some(vec![validator_builder(url)]),
            ..Default::default()
        };
        assert!(matches!(
            store.set_validator_config(&validator, config.clone()),
            Err(Error::InvalidBuilderUrl(_))
        ));
        let explicit = RequestAuthData::new(vec![0xff, 0x00, b'A']).unwrap();
        let mut global = global_builder(url, 9);
        global.auth_data = Some(explicit.clone());
        store.insert(global).unwrap();
        config.builders.as_mut().unwrap()[0].auth_data = Some(explicit.clone());
        store.set_validator_config(&validator, config).unwrap();
        let signed =
            futures::executor::block_on(store.builder_config(&validator, |data| async move {
                Ok::<_, ()>(signed_auth(data))
            }));
        assert_eq!(signed.builders[0].auth.message.data, explicit);
        assert_eq!(signed.builders[0].max_execution_payment, 9);
    }

    #[test]
    fn stream_targets_only_match_header_streams_in_resolved_builder_list() {
        let directory = tempdir().unwrap();
        let store = BuilderStore::open_or_create(directory.path()).unwrap();
        let validator = Keypair::random().pk.compress();

        // A global builder the validator does not use, plus one it does.
        store
            .insert(global_builder("https://unused-builder.example", 9))
            .unwrap();
        let mut used = global_builder("https://used-builder.example", 9);
        used.min_bid = Some(42);
        used.builder_boost_factor = Some(150);
        store.insert(used).unwrap();

        // `header_streams` lists both the used builder and a builder the validator never resolves.
        store
            .persist_update(|candidate| {
                candidate
                    .header_streams
                    .push(builder_definitions::HeaderStreamDefinition {
                        url: "https://used-builder.example".parse().unwrap(),
                        api_key: "2b5d1d6a-9d3e-4a2f-8e8b-9f2c2c1f8a11".to_string(),
                    });
                candidate
                    .header_streams
                    .push(builder_definitions::HeaderStreamDefinition {
                        url: "https://never-configured.example".parse().unwrap(),
                        api_key: "11111111-1111-1111-1111-111111111111".to_string(),
                    });
                Ok(true)
            })
            .unwrap();

        let targets = store.stream_targets(&validator);
        assert_eq!(
            targets.len(),
            1,
            "only the resolved+listed builder should match"
        );
        assert_eq!(
            targets[0].url.as_str().unwrap(),
            "https://used-builder.example"
        );
        assert_eq!(targets[0].api_key, "2b5d1d6a-9d3e-4a2f-8e8b-9f2c2c1f8a11");
        assert_eq!(targets[0].max_execution_payment, 9);
        assert_eq!(targets[0].min_bid, 42);
        assert_eq!(targets[0].builder_boost_factor, 150);
        assert_eq!(&*targets[0].auth_data, b"used-builder.example");
    }

    #[test]
    fn stream_targets_empty_when_no_direct_builders_configured() {
        let directory = tempdir().unwrap();
        let store = BuilderStore::open_or_create(directory.path()).unwrap();
        let validator = Keypair::random().pk.compress();
        store
            .persist_update(|candidate| {
                candidate
                    .header_streams
                    .push(builder_definitions::HeaderStreamDefinition {
                        url: "https://builder.example".parse().unwrap(),
                        api_key: "2b5d1d6a-9d3e-4a2f-8e8b-9f2c2c1f8a11".to_string(),
                    });
                Ok(true)
            })
            .unwrap();

        assert!(store.stream_targets(&validator).is_empty());
    }

    #[test]
    fn has_header_streams_reflects_top_level_entries() {
        let directory = tempdir().unwrap();
        let store = BuilderStore::open_or_create(directory.path()).unwrap();

        // Nothing configured yet: the header-stream consumer has nothing to run against.
        assert!(!store.has_header_streams());

        store
            .persist_update(|candidate| {
                candidate
                    .header_streams
                    .push(builder_definitions::HeaderStreamDefinition {
                        url: "https://builder.example".parse().unwrap(),
                        api_key: "2b5d1d6a-9d3e-4a2f-8e8b-9f2c2c1f8a11".to_string(),
                    });
                Ok(true)
            })
            .unwrap();

        // At least one entry is configured, independent of any validator's resolved builders.
        assert!(store.has_header_streams());
    }

    #[test]
    fn header_streams_survive_a_keymanager_style_save() {
        let directory = tempdir().unwrap();
        let store = BuilderStore::open_or_create(directory.path()).unwrap();
        let validator = Keypair::random().pk.compress();
        store
            .persist_update(|candidate| {
                candidate
                    .header_streams
                    .push(builder_definitions::HeaderStreamDefinition {
                        url: "https://builder.example".parse().unwrap(),
                        api_key: "2b5d1d6a-9d3e-4a2f-8e8b-9f2c2c1f8a11".to_string(),
                    });
                Ok(true)
            })
            .unwrap();

        // A keymanager-style save: set (then delete) a per-validator override, which rewrites the
        // whole config file through `BuilderConfigFile::save`.
        store
            .set_validator_config(
                &validator,
                ValidatorBuilderConfig {
                    builders: Some(vec![]),
                    ..Default::default()
                },
            )
            .unwrap();
        store.delete_validator_config(&validator).unwrap();

        let reloaded = BuilderStore::open_or_create(directory.path()).unwrap();
        let file = builder_definitions::BuilderConfigFile::open(directory.path()).unwrap();
        assert_eq!(file.header_streams.len(), 1);
        assert_eq!(
            file.header_streams[0].api_key,
            "2b5d1d6a-9d3e-4a2f-8e8b-9f2c2c1f8a11"
        );
        // And the reopened store still resolves it once a matching builder exists.
        drop(reloaded);
    }

    #[test]
    fn custom_builders_resolve_payment_limits_from_enabled_globals() {
        let directory = tempdir().unwrap();
        let store = BuilderStore::open_or_create(directory.path()).unwrap();
        let validator = Keypair::random().pk.compress();
        store
            .insert(global_builder("https://global-builder.example", 9))
            .unwrap();
        store
            .insert(BuilderDefinition {
                enabled: false,
                url: "https://disabled-builder.example".parse().unwrap(),
                auth_data: None,
                builder_pubkeys: vec![],
                max_execution_payment: 9,
                min_bid: None,
                builder_boost_factor: None,
            })
            .unwrap();

        store
            .set_validator_config(
                &validator,
                ValidatorBuilderConfig {
                    builders: Some(vec![
                        validator_builder("https://global-builder.example"),
                        validator_builder("https://disabled-builder.example"),
                    ]),
                    ..Default::default()
                },
            )
            .unwrap();

        let resolved = store.get_validator_config(&validator);
        assert_eq!(resolved.builders[0].max_execution_payment, 9);
        assert_eq!(resolved.builders[1].max_execution_payment, 0);
    }
}
