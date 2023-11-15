use {
    crate::{boxed_error, SOLANA_ROOT},
    base64::{engine::general_purpose, Engine as _},
    k8s_openapi::{
        api::{
            apps::v1::{ReplicaSet, ReplicaSetSpec},
            core::v1::{
                Container, EnvVar, EnvVarSource, Namespace, ObjectFieldSelector,
                PodSecurityContext, PodSpec, PodTemplateSpec, Secret, SecretVolumeSource, Service,
                ServicePort, ServiceSpec, Volume, VolumeMount,
            },
        },
        apimachinery::pkg::apis::meta::v1::LabelSelector,
        ByteString,
    },
    kube::{
        api::{Api, ListParams, ObjectMeta, PostParams},
        Client,
    },
    log::*,
    solana_sdk::{hash::Hash, pubkey::Pubkey},
    std::{collections::BTreeMap, error::Error},
};

pub struct ValidatorConfig<'a> {
    pub tpu_enable_udp: bool,
    pub tpu_disable_quic: bool,
    pub gpu_mode: &'a str, // TODO: this is not implemented yet
    pub internal_node_sol: f64,
    pub internal_node_stake_sol: f64,
    pub wait_for_supermajority: Option<u64>,
    pub warp_slot: Option<u64>,
    pub shred_version: Option<u16>,
    pub bank_hash: Option<Hash>,
    pub max_ledger_size: Option<u64>,
    pub skip_poh_verify: bool,
    pub no_snapshot_fetch: bool,
    pub require_tower: bool,
    pub enable_full_rpc: bool,
}

impl<'a> std::fmt::Display for ValidatorConfig<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Runtime Config\n\
             tpu_enable_udp: {}\n\
             tpu_disable_quic: {}\n\
             gpu_mode: {}\n\
             internal_node_sol: {}\n\
             internal_node_stake_sol: {}\n\
             wait_for_supermajority: {:?}\n\
             warp_slot: {:?}\n\
             shred_version: {:?}\n\
             bank_hash: {:?}\n\
             max_ledger_size: {:?}\n\
             skip_poh_verify: {}\n\
             no_snapshot_fetch: {}\n\
             require_tower: {}\n\
             enable_full_rpc: {}",
            self.tpu_enable_udp,
            self.tpu_disable_quic,
            self.gpu_mode,
            self.internal_node_sol,
            self.internal_node_stake_sol,
            self.wait_for_supermajority,
            self.warp_slot,
            self.shred_version,
            self.bank_hash,
            self.max_ledger_size,
            self.skip_poh_verify,
            self.no_snapshot_fetch,
            self.require_tower,
            self.enable_full_rpc,
        )
    }
}

#[derive(Clone, Debug)]
pub struct ClientConfig {
    pub num_clients: i32,
    pub client_delay_start: u64,
    pub client_type: String,
    pub client_to_run: String,
    pub bench_tps_args: Vec<String>,
    pub target_node: Option<Pubkey>,
    pub duration: u64,
    pub num_nodes: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct Metrics {
    pub host: String,
    pub port: String,
    pub database: String,
    pub username: String,
    password: String,
}

impl Metrics {
    pub fn new(
        host: String,
        port: String,
        database: String,
        username: String,
        password: String,
    ) -> Self {
        Metrics {
            host,
            port,
            database,
            username,
            password,
        }
    }
    pub fn to_env_string(&self) -> String {
        format!(
            "host={}:{},db={},u={},p={}",
            self.host, self.port, self.database, self.username, self.password
        )
    }
}

pub struct Kubernetes<'a> {
    client: Client,
    namespace: &'a str,
    validator_config: &'a mut ValidatorConfig<'a>,
    client_config: ClientConfig,
    pub metrics: Option<Metrics>,
    num_faucets: i32,
}

impl<'a> Kubernetes<'a> {
    pub async fn new(
        namespace: &'a str,
        validator_config: &'a mut ValidatorConfig<'a>,
        client_config: ClientConfig,
        metrics: Option<Metrics>,
        num_faucets: i32,
    ) -> Kubernetes<'a> {
        Kubernetes {
            client: Client::try_default().await.unwrap(),
            namespace,
            validator_config,
            client_config,
            metrics,
            num_faucets
        }
    }

    pub fn set_shred_version(&mut self, shred_version: u16) {
        self.validator_config.shred_version = Some(shred_version);
    }

    pub fn set_bank_hash(&mut self, bank_hash: Hash) {
        self.validator_config.bank_hash = Some(bank_hash);
    }

    fn generate_command_flags(&mut self) -> Vec<String> {
        let mut flags = Vec::new();

        if self.validator_config.tpu_enable_udp {
            flags.push("--tpu-enable-udp".to_string());
        }
        if self.validator_config.tpu_disable_quic {
            flags.push("--tpu-disable-quic".to_string());
        }
        if self.validator_config.skip_poh_verify {
            flags.push("--skip-poh-verify".to_string());
        }
        if self.validator_config.no_snapshot_fetch {
            flags.push("--no-snapshot-fetch".to_string());
        }
        if self.validator_config.require_tower {
            flags.push("--require-tower".to_string());
        }
        if self.validator_config.enable_full_rpc {
            flags.push("--enable-rpc-transaction-history".to_string());
            flags.push("--enable-extended-tx-metadata-storage".to_string());
        }

        if let Some(limit_ledger_size) = self.validator_config.max_ledger_size {
            flags.push("--limit-ledger-size".to_string());
            flags.push(limit_ledger_size.to_string());
        }

        flags
    }

    fn generate_bootstrap_command_flags(&mut self) -> Vec<String> {
        let mut flags = self.generate_command_flags();
        if let Some(slot) = self.validator_config.wait_for_supermajority {
            flags.push("--wait-for-supermajority".to_string());
            flags.push(slot.to_string());
        }

        if let Some(bank_hash) = self.validator_config.bank_hash {
            flags.push("--expected-bank-hash".to_string());
            flags.push(bank_hash.to_string());
        }

        flags
    }

    fn generate_validator_command_flags(&mut self) -> Vec<String> {
        let mut flags = self.generate_command_flags();

        flags.push("--internal-node-stake-sol".to_string());
        flags.push(self.validator_config.internal_node_stake_sol.to_string());
        flags.push("--internal-node-sol".to_string());
        flags.push(self.validator_config.internal_node_sol.to_string());

        if let Some(shred_version) = self.validator_config.shred_version {
            flags.push("--expected-shred-version".to_string());
            flags.push(shred_version.to_string());
        }

        flags
    }

    fn generate_faucet_command_flags(&mut self) -> Vec<String> {
        let mut flags = self.generate_command_flags();
        if let Some(shred_version) = self.validator_config.shred_version {
            flags.push("--expected-shred-version".to_string());
            flags.push(shred_version.to_string());
        }

        flags
    }

    fn generate_client_command_flags(&self) -> Vec<String> {
        let mut flags = vec![];

        flags.push(self.client_config.client_to_run.clone()); //client to run
        let bench_tps_args = self.client_config.bench_tps_args.join(" ");
        flags.push(bench_tps_args);
        flags.push(self.client_config.client_type.clone());

        if let Some(target_node) = self.client_config.target_node {
            flags.push("--target-node".to_string());
            flags.push(target_node.to_string());
        }

        flags.push("--duration".to_string());
        flags.push(self.client_config.duration.to_string());
        info!("greg duration: {}", self.client_config.duration);

        if let Some(num_nodes) = self.client_config.num_nodes {
            flags.push("--num-nodes".to_string());
            flags.push(num_nodes.to_string());
            info!("greg num nodes: {}", num_nodes);
        }

        flags
    }

    pub async fn namespace_exists(&self) -> Result<bool, kube::Error> {
        let namespaces: Api<Namespace> = Api::all(self.client.clone());
        let namespace_list = namespaces.list(&ListParams::default()).await?;

        for namespace in namespace_list.items {
            if let Some(ns) = namespace.metadata.name {
                if ns == *self.namespace {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    pub fn create_selector(&mut self, key: &str, value: &str) -> BTreeMap<String, String> {
        let mut btree = BTreeMap::new();
        btree.insert(key.to_string(), value.to_string());
        btree
    }

    pub async fn create_bootstrap_validator_replicas_set(
        &mut self,
        container_name: &str,
        image_name: &str,
        num_bootstrap_validators: i32,
        secret_name: Option<String>,
        label_selector: &BTreeMap<String, String>,
    ) -> Result<ReplicaSet, Box<dyn Error>> {
        let mut env_var = vec![EnvVar {
            name: "MY_POD_IP".to_string(),
            value_from: Some(EnvVarSource {
                field_ref: Some(ObjectFieldSelector {
                    field_path: "status.podIP".to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];

        if self.metrics.is_some() {
            env_var.push(self.get_metrics_env_var_secret())
        }

        let accounts_volume = Some(vec![Volume {
            name: "bootstrap-accounts-volume".into(),
            secret: Some(SecretVolumeSource {
                secret_name,
                ..Default::default()
            }),
            ..Default::default()
        }]);

        let accounts_volume_mount = Some(vec![VolumeMount {
            name: "bootstrap-accounts-volume".to_string(),
            mount_path: "/home/solana/bootstrap-accounts".to_string(),
            ..Default::default()
        }]);

        let mut command =
            vec!["/home/solana/k8s-cluster-scripts/bootstrap-startup-script.sh".to_string()];
        command.extend(self.generate_bootstrap_command_flags());

        for c in command.iter() {
            debug!("bootstrap command: {}", c);
        }

        self.create_replicas_set(
            "bootstrap-validator",
            label_selector,
            container_name,
            image_name,
            num_bootstrap_validators,
            env_var,
            &command,
            accounts_volume,
            accounts_volume_mount,
        )
        .await
    }

    // mount genesis in bootstrap. validators will pull
    // genesis from bootstrap
    #[allow(clippy::too_many_arguments)]
    async fn create_replicas_set(
        &self,
        app_name: &str,
        label_selector: &BTreeMap<String, String>,
        container_name: &str,
        image_name: &str,
        num_validators: i32,
        env_vars: Vec<EnvVar>,
        command: &[String],
        volumes: Option<Vec<Volume>>,
        volume_mounts: Option<Vec<VolumeMount>>,
    ) -> Result<ReplicaSet, Box<dyn Error>> {
        // Define the pod spec
        let pod_spec = PodTemplateSpec {
            metadata: Some(ObjectMeta {
                labels: Some(label_selector.clone()),
                ..Default::default()
            }),
            spec: Some(PodSpec {
                containers: vec![Container {
                    name: container_name.to_string(),
                    image: Some(image_name.to_string()),
                    image_pull_policy: Some("Always".to_string()),
                    env: Some(env_vars),
                    command: Some(command.to_owned()),
                    volume_mounts,
                    ..Default::default()
                }],
                volumes,
                security_context: Some(PodSecurityContext {
                    run_as_user: Some(1000),
                    run_as_group: Some(1000),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        };

        let replicas_set_spec = ReplicaSetSpec {
            replicas: Some(num_validators),
            selector: LabelSelector {
                match_labels: Some(label_selector.clone()),
                ..Default::default()
            },
            template: Some(pod_spec),
            ..Default::default()
        };

        Ok(ReplicaSet {
            metadata: ObjectMeta {
                name: Some(format!("{}-replicaset", app_name)),
                namespace: Some(self.namespace.to_string()),
                ..Default::default()
            },
            spec: Some(replicas_set_spec),
            ..Default::default()
        })
    }

    pub async fn deploy_secret(&self, secret: &Secret) -> Result<Secret, kube::Error> {
        let secrets_api: Api<Secret> = Api::namespaced(self.client.clone(), self.namespace);
        secrets_api.create(&PostParams::default(), secret).await
    }

    pub fn create_metrics_secret(&self) -> Result<Secret, Box<dyn Error>> {
        let mut data = BTreeMap::new();
        if let Some(metrics) = &self.metrics {
            data.insert(
                "SOLANA_METRICS_CONFIG".to_string(),
                ByteString(metrics.to_env_string().into_bytes()),
            );
        } else {
            return Err(boxed_error!(format!(
                "Called create_metrics_secret() but metrics were not provided."
            )));
        }

        let secret = Secret {
            metadata: ObjectMeta {
                name: Some("solana-metrics-secret".to_string()),
                ..Default::default()
            },
            data: Some(data),
            ..Default::default()
        };

        Ok(secret)
    }

    pub fn get_metrics_env_var_secret(&self) -> EnvVar {
        EnvVar {
            name: "SOLANA_METRICS_CONFIG".to_string(),
            value_from: Some(k8s_openapi::api::core::v1::EnvVarSource {
                secret_key_ref: Some(k8s_openapi::api::core::v1::SecretKeySelector {
                    name: Some("solana-metrics-secret".to_string()),
                    key: "SOLANA_METRICS_CONFIG".to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    pub fn create_bootstrap_secret(&self, secret_name: &str) -> Result<Secret, Box<dyn Error>> {
        let faucet_key_path = SOLANA_ROOT.join("config-k8s");
        let faucet_keypair =
            std::fs::read(faucet_key_path.join("faucet.json")).unwrap_or_else(|_| {
                panic!("Failed to read faucet.json file! at: {:?}", faucet_key_path)
            });

        let key_path = SOLANA_ROOT.join("config-k8s/bootstrap-validator");

        let identity_keypair = std::fs::read(key_path.join("identity.json"))
            .unwrap_or_else(|_| panic!("Failed to read identity.json file! at: {:?}", key_path));
        let vote_keypair = std::fs::read(key_path.join("vote-account.json")).unwrap_or_else(|_| {
            panic!("Failed to read vote-account.json file! at: {:?}", key_path)
        });
        let stake_keypair =
            std::fs::read(key_path.join("stake-account.json")).unwrap_or_else(|_| {
                panic!("Failed to read stake-account.json file! at: {:?}", key_path)
            });

        let mut data = BTreeMap::new();
        data.insert(
            "identity.base64".to_string(),
            ByteString(
                general_purpose::STANDARD
                    .encode(identity_keypair)
                    .as_bytes()
                    .to_vec(),
            ),
        );
        data.insert(
            "vote.base64".to_string(),
            ByteString(
                general_purpose::STANDARD
                    .encode(vote_keypair)
                    .as_bytes()
                    .to_vec(),
            ),
        );
        data.insert(
            "stake.base64".to_string(),
            ByteString(
                general_purpose::STANDARD
                    .encode(stake_keypair)
                    .as_bytes()
                    .to_vec(),
            ),
        );
        data.insert(
            "faucet.base64".to_string(),
            ByteString(
                general_purpose::STANDARD
                    .encode(faucet_keypair)
                    .as_bytes()
                    .to_vec(),
            ),
        );

        let secret = Secret {
            metadata: ObjectMeta {
                name: Some(secret_name.to_string()),
                ..Default::default()
            },
            data: Some(data),
            ..Default::default()
        };

        Ok(secret)
    }

    pub fn create_validator_secret(&self, validator_index: i32) -> Result<Secret, Box<dyn Error>> {
        let secret_name = format!("validator-accounts-secret-{}", validator_index);
        let key_path = SOLANA_ROOT.join("config-k8s");

        let mut data: BTreeMap<String, ByteString> = BTreeMap::new();
        let accounts = vec!["identity", "vote", "stake"];
        for account in accounts {
            let file_name: String = if account == "identity" {
                format!("validator-{}-{}.json", account, validator_index)
            } else {
                format!("validator-{}-account-{}.json", account, validator_index)
            };
            let keypair = std::fs::read(key_path.join(file_name.clone())).unwrap_or_else(|_| {
                panic!("Failed to read {} file! at: {:?}", file_name, key_path)
            });
            data.insert(
                format!("{}.base64", account),
                ByteString(
                    general_purpose::STANDARD
                        .encode(keypair)
                        .as_bytes()
                        .to_vec(),
                ),
            );
        }
        let secret = Secret {
            metadata: ObjectMeta {
                name: Some(secret_name.to_string()),
                ..Default::default()
            },
            data: Some(data),
            ..Default::default()
        };

        Ok(secret)
    }

    pub fn create_client_secret(&self, client_index: i32) -> Result<Secret, Box<dyn Error>> {
        let secret_name = format!("client-accounts-secret-{}", client_index);
        let faucet_key_path = SOLANA_ROOT.join("config-k8s");
        let faucet_keypair =
            std::fs::read(faucet_key_path.join("faucet.json")).unwrap_or_else(|_| {
                panic!("Failed to read faucet.json file! at: {:?}", faucet_key_path)
            });

        let mut data = BTreeMap::new();
        data.insert(
            "faucet.base64".to_string(),
            ByteString(
                general_purpose::STANDARD
                    .encode(faucet_keypair)
                    .as_bytes()
                    .to_vec(),
            ),
        );

        let secret = Secret {
            metadata: ObjectMeta {
                name: Some(secret_name.to_string()),
                ..Default::default()
            },
            data: Some(data),
            ..Default::default()
        };

        Ok(secret)
    }

    pub fn create_faucet_secret(&self, faucet_index: i32) -> Result<Secret, Box<dyn Error>> {
        let secret_name = format!("faucet-accounts-secret-{}", faucet_index);
        let faucet_key_path = SOLANA_ROOT.join("config-k8s");
        let faucet_keypair =
            std::fs::read(faucet_key_path.join("faucet.json")).unwrap_or_else(|_| {
                panic!("Failed to read faucet.json file! at: {:?}", faucet_key_path)
            });

        let mut data = BTreeMap::new();
        let accounts = vec!["identity", "stake"];
        for account in accounts {
            let file_name: String = if account == "identity" {
                format!("non_voting-validator-{}-{}.json", account, faucet_index)
            } else {
                format!("non_voting-validator-{}-account-{}.json", account, faucet_index)
            };
            let keypair = std::fs::read(faucet_key_path.join(file_name.clone())).unwrap_or_else(|_| {
                panic!("Failed to read {} file! at: {:?}", file_name, faucet_key_path)
            });
            data.insert(
                format!("{}.base64", account),
                ByteString(
                    general_purpose::STANDARD
                        .encode(keypair)
                        .as_bytes()
                        .to_vec(),
                ),
            );
        }
        data.insert(
            "faucet.base64".to_string(),
            ByteString(
                general_purpose::STANDARD
                    .encode(faucet_keypair)
                    .as_bytes()
                    .to_vec(),
            ),
        );

        let secret = Secret {
            metadata: ObjectMeta {
                name: Some(secret_name.to_string()),
                ..Default::default()
            },
            data: Some(data),
            ..Default::default()
        };

        Ok(secret)
    }

    pub async fn deploy_replicas_set(
        &self,
        replica_set: &ReplicaSet,
    ) -> Result<ReplicaSet, kube::Error> {
        let api: Api<ReplicaSet> = Api::namespaced(self.client.clone(), self.namespace);
        let post_params = PostParams::default();
        info!("creating replica set!");
        // Apply the ReplicaSet
        api.create(&post_params, replica_set).await
    }

    fn create_service(
        &self,
        service_name: &str,
        label_selector: &BTreeMap<String, String>,
    ) -> Service {
        Service {
            metadata: ObjectMeta {
                name: Some(format!("{}-service", service_name).to_string()),
                namespace: Some(self.namespace.to_string()),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                selector: Some(label_selector.clone()),
                cluster_ip: Some("None".into()),
                // cluster_ips: None,
                ports: Some(vec![
                    ServicePort {
                        port: 8899, // RPC Port
                        name: Some("rpc-port".to_string()),
                        ..Default::default()
                    },
                    ServicePort {
                        port: 8001, //Gossip Port
                        name: Some("gossip-port".to_string()),
                        ..Default::default()
                    },
                    ServicePort {
                        port: 9900, //Faucet Port
                        name: Some("faucet-port".to_string()),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    pub async fn deploy_service(&self, service: &Service) -> Result<Service, kube::Error> {
        let post_params = PostParams::default();
        // Create an API instance for Services in the specified namespace
        let service_api: Api<Service> = Api::namespaced(self.client.clone(), self.namespace);

        // Create the Service object in the cluster
        service_api.create(&post_params, service).await
    }

    pub async fn check_replica_set_ready(
        &self,
        replica_set_name: &str,
    ) -> Result<bool, kube::Error> {
        let replica_sets: Api<ReplicaSet> = Api::namespaced(self.client.clone(), self.namespace);
        let replica_set = replica_sets.get(replica_set_name).await?;

        let desired_validators = replica_set.spec.as_ref().unwrap().replicas.unwrap_or(1);
        let available_validators = replica_set
            .status
            .as_ref()
            .unwrap()
            .available_replicas
            .unwrap_or(0);

        Ok(available_validators >= desired_validators)
    }

    fn set_non_bootstrap_environment_variables(&self) -> Vec<EnvVar> {
        vec![
            EnvVar {
                name: "NAMESPACE".to_string(),
                value_from: Some(EnvVarSource {
                    field_ref: Some(ObjectFieldSelector {
                        field_path: "metadata.namespace".to_string(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            EnvVar {
                name: "BOOTSTRAP_RPC_ADDRESS".to_string(),
                value: Some(
                    "bootstrap-validator-service.$(NAMESPACE).svc.cluster.local:8899".to_string(),
                ),
                ..Default::default()
            },
            EnvVar {
                name: "BOOTSTRAP_GOSSIP_ADDRESS".to_string(),
                value: Some(
                    "bootstrap-validator-service.$(NAMESPACE).svc.cluster.local:8001".to_string(),
                ),
                ..Default::default()
            },
            EnvVar {
                name: "BOOTSTRAP_FAUCET_ADDRESS".to_string(),
                value: Some(
                    "bootstrap-validator-service.$(NAMESPACE).svc.cluster.local:9900".to_string(),
                ),
                ..Default::default()
            },
        ]
    }

    fn set_environment_variables_to_find_faucet(&self) -> Vec<EnvVar> {
        vec![
            EnvVar {
                name: "FAUCET_RPC_ADDRESS".to_string(),
                value: Some(
                    "faucet-lb-service.$(NAMESPACE).svc.cluster.local:8899".to_string(),
                ),
                ..Default::default()
            },
            EnvVar {
                name: "FAUCET_FAUCET_ADDRESS".to_string(),
                value: Some(
                    "faucet-lb-service.$(NAMESPACE).svc.cluster.local:9900".to_string(),
                ),
                ..Default::default()
            },
        ]
    }

    pub async fn create_validator_replica_set(
        &mut self,
        container_name: &str,
        validator_index: i32,
        image_name: &str,
        num_validators: i32,
        secret_name: Option<String>,
        label_selector: &BTreeMap<String, String>,
    ) -> Result<ReplicaSet, Box<dyn Error>> {
        let mut env_vars = self.set_non_bootstrap_environment_variables();
        if self.metrics.is_some() {
            env_vars.push(self.get_metrics_env_var_secret())
        }
        if self.num_faucets > 0 {
            env_vars.append(&mut self.set_environment_variables_to_find_faucet());
        }

        let accounts_volume = Some(vec![Volume {
            name: format!("validator-accounts-volume-{}", validator_index),
            secret: Some(SecretVolumeSource {
                secret_name,
                ..Default::default()
            }),
            ..Default::default()
        }]);

        let accounts_volume_mount = Some(vec![VolumeMount {
            name: format!("validator-accounts-volume-{}", validator_index),
            mount_path: "/home/solana/validator-accounts".to_string(),
            ..Default::default()
        }]);

        let mut command =
            vec!["/home/solana/k8s-cluster-scripts/validator-startup-script.sh".to_string()];
        command.extend(self.generate_validator_command_flags());

        for c in command.iter() {
            debug!("validator command: {}", c);
        }

        self.create_replicas_set(
            format!("validator-{}", validator_index).as_str(),
            label_selector,
            container_name,
            image_name,
            num_validators,
            env_vars,
            &command,
            accounts_volume,
            accounts_volume_mount,
        )
        .await
    }

    pub async fn create_faucet_replica_set(
        &mut self,
        container_name: &str,
        faucet_index: i32,
        image_name: &str,
        num_validators: i32,
        secret_name: Option<String>,
        label_selector: &BTreeMap<String, String>,
    ) -> Result<ReplicaSet, Box<dyn Error>> {
        let mut env_vars = vec![EnvVar {
            name: "MY_POD_IP".to_string(),
            value_from: Some(EnvVarSource {
                field_ref: Some(ObjectFieldSelector {
                    field_path: "status.podIP".to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];
        env_vars.append(&mut self.set_non_bootstrap_environment_variables());

        if self.metrics.is_some() {
            env_vars.push(self.get_metrics_env_var_secret())
        }

        let accounts_volume = Some(vec![Volume {
            name: format!("faucet-accounts-volume-{}", faucet_index),
            secret: Some(SecretVolumeSource {
                secret_name,
                ..Default::default()
            }),
            ..Default::default()
        }]);

        let accounts_volume_mount = Some(vec![VolumeMount {
            name: format!("faucet-accounts-volume-{}", faucet_index),
            mount_path: "/home/solana/faucet-accounts".to_string(),
            ..Default::default()
        }]);

        let mut command =
        vec!["/home/solana/k8s-cluster-scripts/faucet-startup-script.sh".to_string()];
        command.extend(self.generate_faucet_command_flags());

    for c in command.iter() {
        debug!("validator command: {}", c);
    }

        self.create_replicas_set(
            format!("faucet-{}", faucet_index).as_str(),
            label_selector,
            container_name,
            image_name,
            num_validators,
            env_vars,
            &command,
            accounts_volume,
            accounts_volume_mount,
        )
        .await
    }

    pub async fn create_client_replica_set(
        &mut self,
        container_name: &str,
        client_index: i32,
        image_name: &str,
        num_clients: i32,
        secret_name: Option<String>,
        label_selector: &BTreeMap<String, String>,
    ) -> Result<ReplicaSet, Box<dyn Error>> {
        let mut env_vars = self.set_non_bootstrap_environment_variables();
        if self.metrics.is_some() {
            env_vars.push(self.get_metrics_env_var_secret())
        }
        if self.num_faucets > 0 {
            env_vars.append(&mut self.set_environment_variables_to_find_faucet());
        }

        let accounts_volume = Some(vec![Volume {
            name: format!("client-accounts-volume-{}", client_index),
            secret: Some(SecretVolumeSource {
                secret_name,
                ..Default::default()
            }),
            ..Default::default()
        }]);

        let accounts_volume_mount = Some(vec![VolumeMount {
            name: format!("client-accounts-volume-{}", client_index),
            mount_path: "/home/solana/client-accounts".to_string(),
            ..Default::default()
        }]);

        let mut command =
            vec!["/home/solana/k8s-cluster-scripts/client-startup-script.sh".to_string()];
        command.extend(self.generate_client_command_flags());

        for c in command.iter() {
            debug!("client command: {}", c);
        }

        self.create_replicas_set(
            format!("client-{}", client_index).as_str(),
            label_selector,
            container_name,
            image_name,
            num_clients,
            env_vars,
            &command,
            accounts_volume,
            accounts_volume_mount,
        )
        .await
    }

    pub fn create_validator_service(
        &self,
        service_name: &str,
        label_selector: &BTreeMap<String, String>,
    ) -> Service {
        self.create_service(service_name, label_selector)
    }

    pub fn create_faucet_load_balancer(
        &self,
        service_name: &str,
        label_selector: &BTreeMap<String, String>
    ) -> Service {
        Service {
            metadata: ObjectMeta {
                name: Some(service_name.to_string()),
                namespace: Some(self.namespace.to_string()),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                selector: Some(label_selector.clone()),
                type_: Some("LoadBalancer".to_string()),
                ports: Some(vec![
                    ServicePort {
                        port: 8899, // RPC Port
                        name: Some("rpc-port".to_string()),
                        ..Default::default()
                    },
                    ServicePort {
                        port: 9900, //Faucet Port
                        name: Some("faucet-port".to_string()),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    pub async fn check_service_matching_replica_set(
        &self,
        app_name: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Get the replica_set
        let replica_set_api: Api<ReplicaSet> = Api::namespaced(self.client.clone(), self.namespace);
        let replica_set = replica_set_api
            .get(format!("{}-replicaset", app_name).as_str())
            .await?;

        // Get the Service
        let service_api: Api<Service> = Api::namespaced(self.client.clone(), self.namespace);
        let service = service_api
            .get(format!("{}-service", app_name).as_str())
            .await?;

        let replica_set_labels = replica_set
            .spec
            .and_then(|spec| {
                Some(spec.selector).and_then(|selector| {
                    selector
                        .match_labels
                        .and_then(|val| val.get("app.kubernetes.io/name").cloned())
                })
            })
            .clone();

        let service_labels = service
            .spec
            .and_then(|spec| {
                spec.selector
                    .and_then(|val| val.get("app.kubernetes.io/name").cloned())
            })
            .clone();

        info!(
            "ReplicaSet, Service labels: {:?}, {:?}",
            replica_set_labels, service_labels
        );

        let are_equal = match (replica_set_labels, service_labels) {
            (Some(rs_label), Some(serv_label)) => rs_label == serv_label,
            _ => false,
        };

        if !are_equal {
            error!("ReplicaSet and Service labels are not the same!");
        }

        Ok(())
    }
}
