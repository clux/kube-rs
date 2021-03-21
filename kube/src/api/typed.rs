use either::Either;
use futures::Stream;
use serde::{de::DeserializeOwned, Serialize};
use std::{fmt::Debug, iter};
use tracing::instrument;

use crate::{
    api::{
        DeleteParams, ListParams, Meta, ObjectList, Patch, PatchParams, PostParams, RequestBuilder,
        WatchEvent,
    },
    client::{Client, Status},
    Result,
};

/// The generic Api abstraction
///
/// This abstracts over a request builder and a resource of type `K` to provide
/// automatic serialization/deserialization to and from `K` in api calls.
///
/// This abstraction takes a [`Client`] (cheap to clone) and awaits the result internally.
#[derive(Clone)]
pub struct Api<K> {
    /// The request creator object
    pub(crate) resource: RequestBuilder,
    /// The client to use (from this library)
    pub(crate) client: Client,
    /// Underlying Object unstored
    ///
    /// Note: Using `iter::Empty` over `PhantomData`, because we never actually keep any
    /// `K` objects, so `Empty` better models our constraints (in particular, `Empty<K>`
    /// is `Send`, even if `K` may not be).
    pub(crate) phantom: iter::Empty<K>,
}

/// Expose same interface as Api for controlling scope/group/versions/ns
impl<K> Api<K>
where
    K: Meta,
    <K as Meta>::DynamicType: Default,
{
    /// Cluster level resources, or resources viewed across all namespaces
    pub fn all(client: Client) -> Self {
        Self::all_with(client, &Default::default())
    }

    /// Namespaced resource within a given namespace
    pub fn namespaced(client: Client, ns: &str) -> Self {
        Self::namespaced_with(client, ns, &Default::default())
    }
}

/// Expose same interface as Api for controlling scope/group/versions/ns
impl<K> Api<K>
where
    K: Meta,
{
    /// Cluster level resources, or resources viewed across all namespaces
    ///
    /// This function accepts `K::DynamicType` so it can be used with dynamic resources.
    pub fn all_with(client: Client, dyntype: &K::DynamicType) -> Self {
        let resource = RequestBuilder::all_with::<K>(dyntype);
        Self {
            resource,
            client,
            phantom: iter::empty(),
        }
    }

    /// Namespaced resource within a given namespace
    ///
    /// This function accepts `K::DynamicType` so it can be used with dynamic resources.
    pub fn namespaced_with(client: Client, ns: &str, dyntype: &K::DynamicType) -> Self {
        let resource = RequestBuilder::namespaced_with::<K>(ns, dyntype);
        Self {
            resource,
            client,
            phantom: iter::empty(),
        }
    }

    /// Returns reference to the underlying `Resource` object.
    /// It can be used to make low-level requests or as a `DynamicType`
    /// for a `DynamicObject`.
    pub fn resource(&self) -> &RequestBuilder {
        &self.resource
    }

    /// Consume self and return the [`Client`]
    pub fn into_client(self) -> Client {
        self.into()
    }
}

/// PUSH/PUT/POST/GET abstractions
impl<K> Api<K>
where
    K: Clone + DeserializeOwned + Meta + Debug,
{
    /// Get a named resource
    ///
    /// ```no_run
    /// use kube::{Api, Client};
    /// use k8s_openapi::api::core::v1::Pod;
    /// #[tokio::main]
    /// async fn main() -> Result<(), kube::Error> {
    ///     let client = Client::try_default().await?;
    ///     let pods: Api<Pod> = Api::namespaced(client, "apps");
    ///     let p: Pod = pods.get("blog").await?;
    ///     Ok(())
    /// }
    /// ```
    #[instrument(skip(self), level = "trace")]
    pub async fn get(&self, name: &str) -> Result<K> {
        let req = self.resource.get(name)?;
        self.client.request::<K>(req).await
    }

    /// Get a list of resources
    ///
    /// You get use this to get everything, or a subset matching fields/labels, say:
    ///
    /// ```no_run
    /// use kube::{api::{Api, ListParams, Meta}, Client};
    /// use k8s_openapi::api::core::v1::Pod;
    /// #[tokio::main]
    /// async fn main() -> Result<(), kube::Error> {
    ///     let client = Client::try_default().await?;
    ///     let pods: Api<Pod> = Api::namespaced(client, "apps");
    ///     let lp = ListParams::default().labels("app=blog"); // for this app only
    ///     for p in pods.list(&lp).await? {
    ///         println!("Found Pod: {}", Meta::name(&p));
    ///     }
    ///     Ok(())
    /// }
    /// ```
    #[instrument(skip(self), level = "trace")]
    pub async fn list(&self, lp: &ListParams) -> Result<ObjectList<K>> {
        let req = self.resource.list(&lp)?;
        self.client.request::<ObjectList<K>>(req).await
    }

    /// Create a resource
    ///
    /// This function requires a type that Serializes to `K`, which can be:
    /// 1. Raw string YAML
    ///     - easy to port from existing files
    ///     - error prone (run-time errors on typos due to failed serialize attempts)
    ///     - very error prone (can write invalid YAML)
    /// 2. An instance of the struct itself
    ///     - easy to instantiate for CRDs (you define the struct)
    ///     - dense to instantiate for [`k8s_openapi`] types (due to many optionals)
    ///     - compile-time safety
    ///     - but still possible to write invalid native types (validation at apiserver)
    /// 3. [`serde_json::json!`] macro instantiated [`serde_json::Value`]
    ///     - Tradeoff between the two
    ///     - Easy partially filling of native [`k8s_openapi`] types (most fields optional)
    ///     - Partial safety against runtime errors (at least you must write valid JSON)
    #[instrument(skip(self), level = "trace")]
    pub async fn create(&self, pp: &PostParams, data: &K) -> Result<K>
    where
        K: Serialize,
    {
        let bytes = serde_json::to_vec(&data)?;
        let req = self.resource.create(&pp, bytes)?;
        self.client.request::<K>(req).await
    }

    /// Delete a named resource
    ///
    /// When you get a `K` via `Left`, your delete has started.
    /// When you get a `Status` via `Right`, this should be a a 2XX style
    /// confirmation that the object being gone.
    ///
    /// 4XX and 5XX status types are returned as an [`Err(kube::Error::Api)`](crate::Error::Api).
    ///
    /// ```no_run
    /// use kube::{api::{Api, DeleteParams}, Client};
    /// use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1beta1 as apiexts;
    /// use apiexts::CustomResourceDefinition;
    /// #[tokio::main]
    /// async fn main() -> Result<(), kube::Error> {
    ///     let client = Client::try_default().await?;
    ///     let crds: Api<CustomResourceDefinition> = Api::all(client);
    ///     crds.delete("foos.clux.dev", &DeleteParams::default()).await?
    ///         .map_left(|o| println!("Deleting CRD: {:?}", o.status))
    ///         .map_right(|s| println!("Deleted CRD: {:?}", s));
    ///     Ok(())
    /// }
    /// ```
    #[instrument(skip(self), level = "trace")]
    pub async fn delete(&self, name: &str, dp: &DeleteParams) -> Result<Either<K, Status>> {
        let req = self.resource.delete(name, &dp)?;
        self.client.request_status::<K>(req).await
    }

    /// Delete a collection of resources
    ///
    /// When you get an `ObjectList<K>` via `Left`, your delete has started.
    /// When you get a `Status` via `Right`, this should be a a 2XX style
    /// confirmation that the object being gone.
    ///
    /// 4XX and 5XX status types are returned as an [`Err(kube::Error::Api)`](crate::Error::Api).
    ///
    /// ```no_run
    /// use kube::{api::{Api, DeleteParams, ListParams, Meta}, Client};
    /// use k8s_openapi::api::core::v1::Pod;
    /// #[tokio::main]
    /// async fn main() -> Result<(), kube::Error> {
    ///     let client = Client::try_default().await?;
    ///     let pods: Api<Pod> = Api::namespaced(client, "apps");
    ///     match pods.delete_collection(&DeleteParams::default(), &ListParams::default()).await? {
    ///         either::Left(list) => {
    ///             let names: Vec<_> = list.iter().map(Meta::name).collect();
    ///             println!("Deleting collection of pods: {:?}", names);
    ///         },
    ///         either::Right(status) => {
    ///             println!("Deleted collection of pods: status={:?}", status);
    ///         }
    ///     }
    ///     Ok(())
    /// }
    /// ```
    #[instrument(skip(self), level = "trace")]
    pub async fn delete_collection(
        &self,
        dp: &DeleteParams,
        lp: &ListParams,
    ) -> Result<Either<ObjectList<K>, Status>> {
        let req = self.resource.delete_collection(&dp, &lp)?;
        self.client.request_status::<ObjectList<K>>(req).await
    }

    /// Patch a subset of a resource's properties
    ///
    /// Takes a [`Patch`] along with [`PatchParams`] for the call.
    ///
    /// ```no_run
    /// use kube::{api::{Api, PatchParams, Patch, Meta}, Client};
    /// use k8s_openapi::api::core::v1::Pod;
    /// #[tokio::main]
    /// async fn main() -> Result<(), kube::Error> {
    ///     let client = Client::try_default().await?;
    ///     let pods: Api<Pod> = Api::namespaced(client, "apps");
    ///     let patch = serde_json::json!({
    ///         "apiVersion": "v1",
    ///         "kind": "Pod",
    ///         "metadata": {
    ///             "name": "blog"
    ///         },
    ///         "spec": {
    ///             "activeDeadlineSeconds": 5
    ///         }
    ///     });
    ///     let params = PatchParams::apply("myapp");
    ///     let patch = Patch::Apply(&patch);
    ///     let o_patched = pods.patch("blog", &params, &patch).await?;
    ///     Ok(())
    /// }
    /// ```
    /// [`Patch`]: super::Patch
    /// [`PatchParams`]: super::PatchParams
    #[instrument(skip(self), level = "trace")]
    pub async fn patch<P: Serialize + Debug>(
        &self,
        name: &str,
        pp: &PatchParams,
        patch: &Patch<P>,
    ) -> Result<K> {
        let req = self.resource.patch(name, &pp, patch)?;
        self.client.request::<K>(req).await
    }

    /// Replace a resource entirely with a new one
    ///
    /// This is used just like [`Api::create`], but with one additional instruction:
    /// You must set `metadata.resourceVersion` in the provided data because k8s
    /// will not accept an update unless you actually knew what the last version was.
    ///
    /// Thus, to use this function, you need to do a `get` then a `replace` with its result.
    ///
    /// ```no_run
    /// use kube::{api::{Api, PostParams, Meta}, Client};
    /// use k8s_openapi::api::batch::v1::Job;
    /// #[tokio::main]
    /// async fn main() -> Result<(), kube::Error> {
    ///     let client = Client::try_default().await?;
    ///     let jobs: Api<Job> = Api::namespaced(client, "apps");
    ///     let j = jobs.get("baz").await?;
    ///     let j_new: Job = serde_json::from_value(serde_json::json!({
    ///         "apiVersion": "batch/v1",
    ///         "kind": "Job",
    ///         "metadata": {
    ///             "name": "baz",
    ///             "resourceVersion": Meta::resource_ver(&j),
    ///         },
    ///         "spec": {
    ///             "template": {
    ///                 "metadata": {
    ///                     "name": "empty-job-pod"
    ///                 },
    ///                 "spec": {
    ///                     "containers": [{
    ///                         "name": "empty",
    ///                         "image": "alpine:latest"
    ///                     }],
    ///                     "restartPolicy": "Never",
    ///                 }
    ///             }
    ///         }
    ///     }))?;
    ///     jobs.replace("baz", &PostParams::default(), &j_new).await?;
    ///     Ok(())
    /// }
    /// ```
    ///
    /// Consider mutating the result of `api.get` rather than recreating it.
    #[instrument(skip(self), level = "trace")]
    pub async fn replace(&self, name: &str, pp: &PostParams, data: &K) -> Result<K>
    where
        K: Serialize,
    {
        let bytes = serde_json::to_vec(&data)?;
        let req = self.resource.replace(name, &pp, bytes)?;
        self.client.request::<K>(req).await
    }

    /// Watch a list of resources
    ///
    /// This returns a future that awaits the initial response,
    /// then you can stream the remaining buffered `WatchEvent` objects.
    ///
    /// Note that a `watch` call can terminate for many reasons (even before the specified
    /// [`ListParams::timeout`] is triggered), and will have to be re-issued
    /// with the last seen resource version when or if it closes.
    ///
    /// Consider using a managed [`watcher`] to deal with automatic re-watches and error cases.
    ///
    /// ```no_run
    /// use kube::{api::{Api, ListParams, Meta, WatchEvent}, Client};
    /// use k8s_openapi::api::batch::v1::Job;
    /// use futures::{StreamExt, TryStreamExt};
    /// #[tokio::main]
    /// async fn main() -> Result<(), kube::Error> {
    ///     let client = Client::try_default().await?;
    ///     let jobs: Api<Job> = Api::namespaced(client, "apps");
    ///     let lp = ListParams::default()
    ///         .fields("metadata.name=my_job")
    ///         .timeout(20); // upper bound of how long we watch for
    ///     let mut stream = jobs.watch(&lp, "0").await?.boxed();
    ///     while let Some(status) = stream.try_next().await? {
    ///         match status {
    ///             WatchEvent::Added(s) => println!("Added {}", Meta::name(&s)),
    ///             WatchEvent::Modified(s) => println!("Modified: {}", Meta::name(&s)),
    ///             WatchEvent::Deleted(s) => println!("Deleted {}", Meta::name(&s)),
    ///             WatchEvent::Bookmark(s) => {},
    ///             WatchEvent::Error(s) => println!("{}", s),
    ///         }
    ///     }
    ///     Ok(())
    /// }
    /// ```
    /// [`ListParams::timeout`]: super::ListParams::timeout
    /// [`watcher`]: https://docs.rs/kube_runtime/*/kube_runtime/watcher/fn.watcher.html
    #[instrument(skip(self), level = "trace")]
    pub async fn watch(
        &self,
        lp: &ListParams,
        version: &str,
    ) -> Result<impl Stream<Item = Result<WatchEvent<K>>>> {
        let req = self.resource.watch(&lp, &version)?;
        self.client.request_events::<K>(req).await
    }
}

impl<K> From<Api<K>> for Client {
    fn from(api: Api<K>) -> Self {
        api.client
    }
}
