#[macro_use] extern crate log;
use either::Either::{Left, Right};
use kube_derive::CustomResource;
use serde_derive::{Deserialize, Serialize};
use serde_json::json;

use apiexts::CustomResourceDefinition;
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1beta1 as apiexts;

use kube::{
    api::{Api, DeleteParams, ListParams, Meta, PatchParams, PostParams},
    client::APIClient,
    config,
};

// Own custom resource
#[derive(CustomResource, Deserialize, Serialize, Clone, Debug)]
#[kube(group = "clux.dev", version = "v1", namespaced)]
#[kube(apiextensions = "v1beta1")]
#[kube(status = "FooStatus")]
#[kube(scale = r#"{"specReplicasPath":".spec.replicas", "statusReplicasPath":".status.replicas"}"#)]
pub struct FooSpec {
    name: String,
    info: String,
    replicas: i32,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default)]
pub struct FooStatus {
    is_bad: bool,
    replicas: i32,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    std::env::set_var("RUST_LOG", "info,kube=debug");
    env_logger::init();
    let config = config::load_kube_config().await?;
    let client = APIClient::new(config);
    let namespace = std::env::var("NAMESPACE").unwrap_or("default".into());

    // Manage CRDs first
    let crds = Api::all::<CustomResourceDefinition>(client.clone());

    // Delete any old versions of it first:
    let dp = DeleteParams::default();
    // but ignore delete err if not exists
    let _ = crds
        .delete::<CustomResourceDefinition>("foos.clux.dev", &dp)
        .await
        .map(|res| {
            res.map_left(|o| {
                info!(
                    "Deleted {}: ({:?})",
                    Meta::name(&o),
                    o.status.unwrap().conditions.unwrap().last()
                );
                // NB: PropagationPolicy::Foreground doesn't cause us to block here
                // we have to watch for it explicitly.. but this is a demo:
                std::thread::sleep(std::time::Duration::from_millis(1000));
            })
            .map_right(|s| {
                // it's gone.
                info!("Deleted foos.clux.dev: ({:?})", s);
            })
        });

    // Create the CRD so we can create Foos in kube
    let foocrd = Foo::crd();
    info!("Creating Foo CRD: {}", serde_json::to_string_pretty(&foocrd)?);
    let pp = PostParams::default();
    let patch_params = PatchParams::default();
    match crds
        .create::<CustomResourceDefinition>(&pp, serde_json::to_vec(&foocrd)?)
        .await
    {
        Ok(o) => {
            info!("Created {} ({:?})", Meta::name(&o), o.status.unwrap());
            debug!("Created CRD: {:?}", o.spec);
        }
        Err(kube::Error::Api(ae)) => assert_eq!(ae.code, 409), // if you skipped delete, for instance
        Err(e) => return Err(e.into()),                        // any other case is probably bad
    }


    // Manage the Foo CR
    let foos = Api::namespaced::<Foo>(client.clone(), &namespace);

    // Create Foo baz
    info!("Creating Foo instance baz");
    let f1 = json!({
        "apiVersion": "clux.dev/v1",
        "kind": "Foo",
        "metadata": { "name": "baz" },
        "spec": { "name": "baz", "info": "old baz", "replicas": 1 },
    });
    let o = foos.create::<Foo>(&pp, serde_json::to_vec(&f1)?).await?;
    assert_eq!(f1["metadata"]["name"], Meta::name(&o));
    info!("Created {}", Meta::name(&o));

    // Verify we can get it
    info!("Get Foo baz");
    let f1cpy = foos.get::<Foo>("baz").await?;
    assert_eq!(f1cpy.spec.info, "old baz");

    // Replace its spec
    info!("Replace Foo baz");
    let foo_replace = json!({
        "apiVersion": "clux.dev/v1",
        "kind": "Foo",
        "metadata": {
            "name": "baz",
            // Updates need to provide our last observed version:
            "resourceVersion": Meta::resource_ver(&f1cpy),
        },
        "spec": { "name": "baz", "info": "new baz", "replicas": 1 },
    });
    let f1_replaced = foos
        .replace::<Foo>("baz", &pp, serde_json::to_vec(&foo_replace)?)
        .await?;
    assert_eq!(f1_replaced.spec.name, "baz");
    assert_eq!(f1_replaced.spec.info, "new baz");
    assert!(f1_replaced.status.is_none());

    // Delete it
    foos.delete::<Foo>("baz", &dp).await?.map_left(|f1del| {
        assert_eq!(f1del.spec.info, "old baz");
    });


    // Create Foo qux with status
    info!("Create Foo instance qux");
    let f2 = json!({
        "apiVersion": "clux.dev/v1",
        "kind": "Foo",
        "metadata": { "name": "qux" },
        "spec": FooSpec { name: "qux".into(), replicas: 0, info: "unpatched qux".into() },
        "status": FooStatus::default(),
    });
    let o = foos.create::<Foo>(&pp, serde_json::to_vec(&f2)?).await?;
    info!("Created {}", Meta::name(&o));

    // Update status on qux
    info!("Replace Status on Foo instance qux");
    let fs = json!({
        "apiVersion": "clux.dev/v1",
        "kind": "Foo",
        "metadata": {
            "name": "qux",
            // Updates need to provide our last observed version:
            "resourceVersion": Meta::resource_ver(&o),
        },
        "status": FooStatus { is_bad: true, replicas: 0 }
    });
    let o = foos
        .replace_status::<Foo>("qux", &pp, serde_json::to_vec(&fs)?)
        .await?;
    info!("Replaced status {:?} for {}", o.status, Meta::name(&o));
    assert!(o.status.unwrap().is_bad);

    info!("Patch Status on Foo instance qux");
    let fs = json!({
        "status": FooStatus { is_bad: false, replicas: 1 }
    });
    let o = foos
        .patch_status::<Foo>("qux", &patch_params, serde_json::to_vec(&fs)?)
        .await?;
    info!("Patched status {:?} for {}", o.status, Meta::name(&o));
    assert!(!o.status.unwrap().is_bad);

    info!("Get Status on Foo instance qux");
    let o = foos.get_status::<Foo>("qux").await?;
    info!("Got status {:?} for {}", o.status, Meta::name(&o));
    assert!(!o.status.unwrap().is_bad);

    // Check scale subresource:
    info!("Get Scale on Foo instance qux");
    let scale = foos.get_scale::<Foo>("qux").await?;
    info!("Got scale {:?} - {:?}", scale.spec, scale.status);
    assert_eq!(scale.status.unwrap().replicas, 1);

    // Scale up
    let fs = json!({
        "spec": { "replicas": 2 }
    });
    let o = foos
        .patch_scale::<Foo>("qux", &patch_params, serde_json::to_vec(&fs)?)
        .await?;
    info!("Patched scale {:?} for {}", o.spec, Meta::name(&o));
    assert_eq!(o.status.unwrap().replicas, 1);
    assert_eq!(o.spec.unwrap().replicas.unwrap(), 2); // we only asked for more


    // Modify a Foo qux with a Patch
    info!("Patch Foo instance qux");
    let patch = json!({
        "spec": { "info": "patched qux" }
    });
    let o = foos
        .patch::<Foo>("qux", &patch_params, serde_json::to_vec(&patch)?)
        .await?;
    info!("Patched {} with new name: {}", Meta::name(&o), o.spec.name);
    assert_eq!(o.spec.info, "patched qux");
    assert_eq!(o.spec.name, "qux"); // didn't blat existing params

    // Check we have 1 remaining instance
    let lp = ListParams::default();
    let res = foos.list::<Foo>(&lp).await?;
    assert_eq!(res.items.len(), 1);

    // Delete the last - expect a status back (instant delete)
    assert!(foos.delete::<Foo>("qux", &dp).await?.is_right());

    // Cleanup the full collection - expect a wait
    match foos.delete_collection::<Foo>(&lp).await? {
        Left(list) => {
            let deleted: Vec<_> = list.iter().map(Meta::name).collect();
            info!("Deleted collection of foos: {:?}", deleted);
        }
        Right(status) => {
            info!("Deleted collection of crds: status={:?}", status);
        }
    }
    Ok(())
}
