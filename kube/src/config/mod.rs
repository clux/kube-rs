//! In cluster or out of cluster kubeconfig to be used by an api client
//!
//! You primarily want to interact with `Configuration`,
//! and its associated load functions.
//!
//! The full `Config` and child-objects are exposed here for convenience only.

mod apis;
mod exec;
mod incluster_config;
mod kube_config;
mod utils;

use crate::{config::kube_config::Der, Error, Result};
use reqwest::{header, Client, ClientBuilder};
use std::convert::TryInto;

use self::kube_config::ConfigLoader;

/// Configuration stores kubernetes path and client for requests.
#[derive(Clone)]
pub struct Configuration {
    pub base_path: String,
    pub client: Client,

    /// The current default namespace. This will be "default" while running outside of a cluster,
    /// and will be the namespace of the pod while running inside a cluster.
    pub default_ns: String,
}

impl Configuration {
    pub fn new(base_path: String, client: Client) -> Self {
        Self::with_default_ns(base_path, client, "default".to_string())
    }

    pub fn with_default_ns(base_path: String, client: Client, default_ns: String) -> Self {
        Configuration {
            base_path,
            client,
            default_ns,
        }
    }

    /// Infer the config type and return it
    ///
    /// Done by attempting to load in-cluster evars first,
    /// then if that fails, try the full local kube config.
    pub async fn infer() -> Result<Self> {
        let cfg = match incluster_config() {
            Err(e) => {
                trace!("No in-cluster config found: {}", e);
                trace!("Falling back to local kube config");
                load_kube_config().await?
            }
            Ok(o) => o,
        };
        Ok(cfg)
    }
}

/// Returns a config includes authentication and cluster infomation from kubeconfig file.
pub async fn load_kube_config() -> Result<Configuration> {
    load_kube_config_with(Default::default()).await
}

/// ConfigOptions stores options used when loading kubeconfig file.
#[derive(Default)]
pub struct ConfigOptions {
    pub context: Option<String>,
    pub cluster: Option<String>,
    pub user: Option<String>,
}

/// Returns a config which includes authentication and cluster information from kubeconfig file.
pub async fn load_kube_config_with(options: ConfigOptions) -> Result<Configuration> {
    let result = create_client_builder(options).await?;
    Ok(Configuration::new(
        result.1.cluster.server,
        result
            .0
            .build()
            .map_err(|e| Error::KubeConfig(format!("Unable to build client: {}", e)))?,
    ))
}

// temporary catalina hack for openssl only
#[cfg(all(target_os = "macos", feature = "native-tls"))]
fn hacky_cert_lifetime_for_macos(client_builder: ClientBuilder, ca_: &Der) -> ClientBuilder {
    use openssl::x509::X509;
    let ca = X509::from_der(&ca_.0).expect("valid der is a der");
    if ca
        .not_before()
        .diff(ca.not_after())
        .map(|d| d.days.abs() > 824)
        .unwrap_or(false)
    {
        client_builder.danger_accept_invalid_certs(true)
    } else {
        client_builder
    }
}

#[cfg(any(not(target_os = "macos"), not(feature = "native-tls")))]
fn hacky_cert_lifetime_for_macos(client_builder: ClientBuilder, _: &Der) -> ClientBuilder {
    client_builder
}

/// Returns a client builder and config loader, based on the cluster information from the kubeconfig file.
///
/// This allows to create your custom reqwest client for using with the cluster API.
pub async fn create_client_builder(options: ConfigOptions) -> Result<(ClientBuilder, ConfigLoader)> {
    let kubeconfig =
        utils::find_kubeconfig().map_err(|e| Error::KubeConfig(format!("Unable to load file: {}", e)))?;

    let mut loader = ConfigLoader::load(kubeconfig, options.context, options.cluster, options.user).await?;


    let (token, client_certificate_data, client_key_data) = match (&loader.user.token, &loader.user.client_certificate_data, &loader.user.client_certificate_data) {
        (Some(token), _, _) => (Some(token.clone()), None, None),
        (_, Some(client_certificate_data), Some(client_key_data)) => (None, Some(client_certificate_data.clone()), Some(client_key_data.clone())),
        (_, _, _) => {
            if let Some(exec) = &loader.user.exec {
                let creds = exec::auth_exec(exec)?;
                let status = creds.status.ok_or_else(|| {
                    Error::KubeConfig("exec-plugin response did not contain a status".into())
                })?;
                (status.token, status.client_certificate_data, status.client_key_data)
            } else {
                (None, None, None)
            }
        }
    };
    loader.user.token = token;
    loader.user.client_key_data = client_key_data;
    loader.user.client_certificate_data = client_certificate_data;

    let mut client_builder = Client::builder()
        // hard disallow more than 5 minute polls due to kubernetes limitations
        .timeout(std::time::Duration::new(295, 0));


    if let Some(ca_bundle) = loader.ca_bundle()? {
        for ca in ca_bundle {
            client_builder = hacky_cert_lifetime_for_macos(client_builder, &ca);
            client_builder = client_builder.add_root_certificate(ca.try_into()?);
        }
    }

    match loader.identity(" ") {
        Ok(id) => {
            client_builder = client_builder.identity(id);
        }
        Err(e) => {
            debug!("failed to load client identity from kube config: {}", e);
            // last resort only if configs ask for it, and no client certs
            if let Some(true) = loader.cluster.insecure_skip_tls_verify {
                client_builder = client_builder.danger_accept_invalid_certs(true);
            }
        }
    }

    let mut headers = header::HeaderMap::new();

    match (
        utils::data_or_file(&token, &loader.user.token_file),
        (&loader.user.username, &loader.user.password),
    ) {
        (Ok(token), _) => {
            headers.insert(
                header::AUTHORIZATION,
                header::HeaderValue::from_str(&format!("Bearer {}", token))
                    .map_err(|e| Error::KubeConfig(format!("Invalid bearer token: {}", e)))?,
            );
        }
        (_, (Some(u), Some(p))) => {
            let encoded = base64::encode(&format!("{}:{}", u, p));
            headers.insert(
                header::AUTHORIZATION,
                header::HeaderValue::from_str(&format!("Basic {}", encoded))
                    .map_err(|e| Error::KubeConfig(format!("Invalid bearer token: {}", e)))?,
            );
        }
        _ => {}
    }

    Ok((client_builder.default_headers(headers), loader))
}

/// Returns a config which is used by clients within pods on kubernetes.
///
/// It will return an error if called from out of kubernetes cluster.
pub fn incluster_config() -> Result<Configuration> {
    let server = incluster_config::kube_server().ok_or_else(|| {
        Error::KubeConfig(format!(
            "Unable to load incluster config, {} and {} must be defined",
            incluster_config::SERVICE_HOSTENV,
            incluster_config::SERVICE_PORTENV
        ))
    })?;

    let cert = incluster_config::load_cert()?;

    let token = incluster_config::load_token()
        .map_err(|e| Error::KubeConfig(format!("Unable to load in cluster token: {}", e)))?;

    let default_ns = incluster_config::load_default_ns()
        .map_err(|e| Error::KubeConfig(format!("Unable to load incluster default namespace: {}", e)))?;

    let mut headers = header::HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        header::HeaderValue::from_str(&format!("Bearer {}", token))
            .map_err(|e| Error::KubeConfig(format!("Invalid bearer token: {}", e)))?,
    );

    let client_builder = Client::builder()
        .add_root_certificate(cert)
        .default_headers(headers);

    Ok(Configuration::with_default_ns(
        server,
        client_builder
            .build()
            .map_err(|e| Error::KubeConfig(format!("Unable to build client: {}", e)))?,
        default_ns,
    ))
}


// Expose raw config structs
pub use apis::{
    AuthInfo, AuthProviderConfig, Cluster, Config, Context, ExecConfig, NamedCluster, NamedContext,
    NamedExtension, Preferences,
};
