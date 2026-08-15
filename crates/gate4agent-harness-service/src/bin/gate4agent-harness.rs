use gate4agent_harness_service::{
    c2::HarnessC2Adapter,
    dispatch::{
        compile_delivery_catalog_from_json_arguments, HarnessLaunchCatalog,
    },
    runtime::{
        start_harness_host_with_operator_and_catalogs, HarnessRuntimeCatalogs,
    },
    HarnessService,
};
use gate4agent_harness_api::HarnessOperatorCredential;
use gate4agent_observation_service::ObservationService;
use std::{
    env,
    net::SocketAddr,
    path::{Path, PathBuf},
};

const C2_TOKEN_ENV: &str = "GATE4AGENT_C2_TOKEN";
const OPERATOR_TOKEN_ENV: &str = "GATE4AGENT_HARNESS_OPERATOR_TOKEN";

#[tokio::main]
async fn main() {
    if run().await.is_err() {
        eprintln!("gate4agent harness failed");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), ()> {
    let config = Config::parse(env::args().skip(1))?;
    if database_paths_alias(&config.harness_db, &config.observation_db) {
        return Err(());
    }
    let operator_token = take_required_env(OPERATOR_TOKEN_ENV)?;
    let operator_credential = HarnessOperatorCredential::parse(operator_token).map_err(|_| ())?;
    let token = take_required_env(C2_TOKEN_ENV)?;
    let launch = HarnessLaunchCatalog::from_json_arguments(config.launch_plan_json)
        .map_err(|_| ())?;
    let delivery = compile_delivery_catalog_from_json_arguments(config.delivery_bundle_json)
        .map_err(|_| ())?;
    let catalogs = HarnessRuntimeCatalogs::new(launch, delivery).map_err(|_| ())?;
    let harness = HarnessService::open(&config.harness_db).map_err(|_| ())?;
    let observation = ObservationService::open(&config.observation_db).map_err(|_| ())?;
    let (adapter, events) = HarnessC2Adapter::connect(&config.c2_endpoint, &token)
        .await.map_err(|_| ())?;
    drop(token);
    let (host, task) = start_harness_host_with_operator_and_catalogs(
        harness,
        observation,
        adapter,
        events,
        config.read_bind,
        Some(operator_credential),
        catalogs,
    ).await.map_err(|_| ())?;
    let endpoint = host.endpoint().socket_addr();
    println!("HARNESS_READ_ENDPOINT={endpoint}");
    println!("GATE4AGENT_HARNESS_OPERATOR_ENDPOINT={endpoint}");
    tokio::signal::ctrl_c().await.map_err(|_| ())?;
    host.shutdown().await.map_err(|_| ())?;
    task.await.map_err(|_| ())?.map_err(|_| ())
}

fn take_required_env(name: &str) -> Result<String, ()> {
    let value = env::var(name).map_err(|_| ())?;
    env::remove_var(name);
    Ok(value)
}

struct Config {
    harness_db: PathBuf,
    observation_db: PathBuf,
    c2_endpoint: String,
    read_bind: SocketAddr,
    launch_plan_json: Vec<String>,
    delivery_bundle_json: Vec<String>,
}

impl Config {
    fn parse(arguments: impl Iterator<Item = String>) -> Result<Self, ()> {
        let mut harness_db = None;
        let mut observation_db = None;
        let mut c2_endpoint = None;
        let mut read_bind = None;
        let mut launch_plan_json = Vec::new();
        let mut delivery_bundle_json = Vec::new();
        let mut arguments = arguments;
        while let Some(flag) = arguments.next() {
            let value = arguments.next().ok_or(())?;
            match flag.as_str() {
                "--harness-db" if harness_db.is_none() => harness_db = Some(PathBuf::from(value)),
                "--observation-db" if observation_db.is_none() => {
                    observation_db = Some(PathBuf::from(value));
                }
                "--c2-endpoint" if c2_endpoint.is_none() => c2_endpoint = Some(value),
                "--read-bind" if read_bind.is_none() => {
                    let address: SocketAddr = value.parse().map_err(|_| ())?;
                    if address.ip() != std::net::Ipv4Addr::LOCALHOST {
                        return Err(());
                    }
                    read_bind = Some(address);
                }
                "--launch-plan-json" => launch_plan_json.push(value),
                "--delivery-bundle-json" => delivery_bundle_json.push(value),
                _ => return Err(()),
            }
        }
        Ok(Self {
            harness_db: harness_db.ok_or(())?,
            observation_db: observation_db.ok_or(())?,
            c2_endpoint: c2_endpoint.ok_or(())?,
            read_bind: read_bind.ok_or(())?,
            launch_plan_json,
            delivery_bundle_json,
        })
    }
}

fn database_paths_alias(left: &Path, right: &Path) -> bool {
    match (canonical_target(left), canonical_target(right)) {
        (Some(left), Some(right)) => sqlite_families_overlap(&left, &right),
        _ => false,
    }
}

fn sqlite_families_overlap(left: &Path, right: &Path) -> bool {
    let left = left.to_string_lossy();
    let right = right.to_string_lossy();
    let left_family = [
        left.to_string(),
        format!("{left}-wal"),
        format!("{left}-shm"),
    ];
    let right_family = [
        right.to_string(),
        format!("{right}-wal"),
        format!("{right}-shm"),
    ];
    left_family.iter().any(|left| {
        right_family.iter().any(|right| left.eq_ignore_ascii_case(right))
    })
}

fn canonical_target(path: &Path) -> Option<PathBuf> {
    if path.exists() {
        return std::fs::canonicalize(path).ok();
    }
    let file_name = path.file_name()?;
    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Some(std::fs::canonicalize(parent).ok()?.join(file_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn required_cli_arguments() -> Vec<String> {
        [
            "--harness-db",
            "harness.sqlite",
            "--observation-db",
            "observation.sqlite",
            "--c2-endpoint",
            r"\\.\pipe\gate4agent-c2",
            "--read-bind",
            "127.0.0.1:0",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }

    #[test]
    fn cli_catalog_json_is_repeatable_exact_and_credentials_remain_env_only() {
        let first_plan = r#" {"plan_id":"plan-a"} "#.to_owned();
        let second_plan = r#"{"plan_id":"plan-a"}"#.to_owned();
        let bundle = r#" {"bundle_id":"bundle-a"} "#.to_owned();
        let mut arguments = required_cli_arguments();
        arguments.extend([
            "--launch-plan-json".to_owned(),
            first_plan.clone(),
            "--delivery-bundle-json".to_owned(),
            bundle.clone(),
            "--launch-plan-json".to_owned(),
            second_plan.clone(),
        ]);
        let config = Config::parse(arguments.into_iter()).unwrap();
        assert_eq!(config.launch_plan_json, [first_plan, second_plan]);
        assert_eq!(config.delivery_bundle_json, [bundle]);

        let defaults = Config::parse(required_cli_arguments().into_iter()).unwrap();
        assert!(defaults.launch_plan_json.is_empty());
        assert!(defaults.delivery_bundle_json.is_empty());

        for credential_flag in ["--operator-token", "--c2-token"] {
            let mut credential_argument = required_cli_arguments();
            credential_argument.extend([
                credential_flag.to_owned(),
                "must-not-enter-argv".to_owned(),
            ]);
            assert!(Config::parse(credential_argument.into_iter()).is_err());
        }

        let mut missing_value = required_cli_arguments();
        missing_value.push("--launch-plan-json".to_owned());
        assert!(Config::parse(missing_value.into_iter()).is_err());
    }

    #[test]
    fn sqlite_database_sidecars_are_rejected_as_path_aliases_in_both_orders() {
        let root = std::env::temp_dir();
        let main = root.join("g4a-harness-alias.sqlite");
        let wal = root.join("g4a-harness-alias.sqlite-wal");
        let uppercase_wal = root.join("g4a-harness-alias.sqlite-WAL");
        let nested_wal = root.join("g4a-harness-alias.sqlite-wal-wal");
        let nested_shm_wal = root.join("g4a-harness-alias.sqlite-shm-wal");
        assert!(database_paths_alias(&main, &wal));
        assert!(database_paths_alias(&wal, &main));
        assert!(database_paths_alias(&main, &uppercase_wal));
        assert!(database_paths_alias(&uppercase_wal, &main));
        assert!(database_paths_alias(&wal, &nested_wal));
        assert!(database_paths_alias(&nested_wal, &wal));
        assert!(database_paths_alias(
            &root.join("g4a-harness-alias.sqlite-shm"),
            &nested_shm_wal,
        ));
        assert!(database_paths_alias(
            &nested_shm_wal,
            &root.join("g4a-harness-alias.sqlite-SHM"),
        ));
        assert!(!database_paths_alias(
            &main,
            &root.join("g4a-observation-distinct.sqlite"),
        ));
    }

    #[test]
    fn operator_environment_is_removed_and_endpoint_output_contains_no_credential() {
        let name = format!("GATE4AGENT_HARNESS_OPERATOR_TOKEN_TEST_{}", std::process::id());
        let credential = format!("g4aho_{}", "a".repeat(64));
        env::set_var(&name, &credential);
        let removed = take_required_env(&name).unwrap();
        assert_eq!(removed, credential);
        assert!(env::var_os(&name).is_none());
        let endpoint: SocketAddr = "127.0.0.1:31245".parse().unwrap();
        let output = format!(
            "HARNESS_READ_ENDPOINT={endpoint}\nGATE4AGENT_HARNESS_OPERATOR_ENDPOINT={endpoint}"
        );
        assert!(!output.contains(&removed));
        assert_eq!(output.matches(endpoint.to_string().as_str()).count(), 2);
    }
}
