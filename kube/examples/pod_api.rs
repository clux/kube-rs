#[macro_use] extern crate log;
use k8s_openapi::api::core::v1::Pod;
use serde_json::json;

use kube::{
    api::{Api, DeleteParams, ListParams, Meta, PatchParams, PostParams},
    client::APIClient,
    config,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    std::env::set_var("RUST_LOG", "info,kube=debug");
    env_logger::init();
    let config = config::load_kube_config().await?;
    let client = APIClient::new(config);
    let namespace = std::env::var("NAMESPACE").unwrap_or("default".into());

    // Manage pods
    let pods = Api::namespaced::<Pod>(client, &namespace);

    // Create Pod blog
    info!("Creating Pod instance blog");
    let p: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": { "name": "blog" },
        "spec": {
            "containers": [{
              "name": "blog",
              "image": "clux/blog:0.1.0"
            }],
        }
    }))?;

    let pp = PostParams::default();
    match pods.create::<Pod>(&pp, &p).await {
        Ok(o) => {
            let name = Meta::name(&o);
            assert_eq!(Meta::name(&p), name);
            info!("Created {}", name);
            // wait for it..
            std::thread::sleep(std::time::Duration::from_millis(5_000));
        }
        Err(kube::Error::Api(ae)) => assert_eq!(ae.code, 409), // if you skipped delete, for instance
        Err(e) => return Err(e.into()),                        // any other case is probably bad
    }

    // Verify we can get it
    info!("Get Pod blog");
    let p1cpy = pods.get::<Pod>("blog").await?;
    if let Some(spec) = &p1cpy.spec {
        info!("Got blog pod with containers: {:?}", spec.containers);
        assert_eq!(spec.containers[0].name, "blog");
    }

    // Replace its spec
    info!("Patch Pod blog");
    let patch = json!({
        "metadata": {
            "resourceVersion": Meta::resource_ver(&p1cpy),
        },
        "spec": {
            "activeDeadlineSeconds": 5
        }
    });
    let patch_params = PatchParams::default();
    let p_patched = pods
        .patch::<Pod>("blog", &patch_params, serde_json::to_vec(&patch)?)
        .await?;
    assert_eq!(p_patched.spec.unwrap().active_deadline_seconds, Some(5));

    let lp = ListParams::default().fields(&format!("metadata.name={}", "blog")); // only want results for our pod
    for p in pods.list::<Pod>(&lp).await? {
        info!("Found Pod: {}", Meta::name(&p));
    }

    // Delete it
    let dp = DeleteParams::default();
    pods.delete::<Pod>("blog", &dp).await?.map_left(|pdel| {
        assert_eq!(Meta::name(&pdel), "blog");
        info!("Deleting blog pod started");
    });

    Ok(())
}
