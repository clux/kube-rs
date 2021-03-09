use bytes::Bytes;
use futures::Stream;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Status;
use serde::de::DeserializeOwned;

use crate::{
    api::{Api, DeleteParams, Patch, PatchParams, PostParams, Resource},
    Error, Result,
};

pub use k8s_openapi::api::autoscaling::v1::{Scale, ScaleSpec, ScaleStatus};

#[cfg(feature = "ws")] use crate::api::remote_command::AttachedProcess;

/// Methods for [scale subresource](https://kubernetes.io/docs/tasks/access-kubernetes-api/custom-resources/custom-resource-definitions/#scale-subresource).
impl<K> Api<K>
where
    K: Clone + DeserializeOwned,
{
    /// Fetch the scale subresource
    pub async fn get_scale(&self, name: &str) -> Result<Scale> {
        let req = self.resource.get_scale(name)?;
        self.client.request::<Scale>(req).await
    }

    /// Update the scale subresource
    pub async fn patch_scale<P: serde::Serialize>(
        &self,
        name: &str,
        pp: &PatchParams,
        patch: &Patch<P>,
    ) -> Result<Scale> {
        let req = self.resource.patch_scale(name, &pp, patch)?;
        self.client.request::<Scale>(req).await
    }

    /// Replace the scale subresource
    pub async fn replace_scale(&self, name: &str, pp: &PostParams, data: Vec<u8>) -> Result<Scale> {
        let req = self.resource.replace_scale(name, &pp, data)?;
        self.client.request::<Scale>(req).await
    }
}

// ----------------------------------------------------------------------------

// TODO: Replace examples with owned custom resources. Bad practice to write to owned objects
// These examples work, but the job controller will totally overwrite what we do.
/// Methods for [status subresource](https://kubernetes.io/docs/tasks/access-kubernetes-api/custom-resources/custom-resource-definitions/#status-subresource).
impl<K> Api<K>
where
    K: DeserializeOwned,
{
    /// Get the named resource with a status subresource
    ///
    /// This actually returns the whole K, with metadata, and spec.
    pub async fn get_status(&self, name: &str) -> Result<K> {
        let req = self.resource.get_status(name)?;
        self.client.request::<K>(req).await
    }

    /// Patch fields on the status object
    ///
    /// NB: Requires that the resource has a status subresource.
    ///
    /// ```no_run
    /// use kube::{api::{Api, PatchParams, Patch}, Client};
    /// use k8s_openapi::api::batch::v1::Job;
    /// #[tokio::main]
    /// async fn main() -> Result<(), kube::Error> {
    ///     let client = Client::try_default().await?;
    ///     let jobs: Api<Job> = Api::namespaced(client, "apps");
    ///     let mut j = jobs.get("baz").await?;
    ///     let pp = PatchParams::default(); // json merge patch
    ///     let data = serde_json::json!({
    ///         "status": {
    ///             "succeeded": 2
    ///         }
    ///     });
    ///     let o = jobs.patch_status("baz", &pp, &Patch::Merge(data)).await?;
    ///     assert_eq!(o.status.unwrap().succeeded, Some(2));
    ///     Ok(())
    /// }
    /// ```
    pub async fn patch_status<P: serde::Serialize>(
        &self,
        name: &str,
        pp: &PatchParams,
        patch: &Patch<P>,
    ) -> Result<K> {
        let req = self.resource.patch_status(name, &pp, patch)?;
        self.client.request::<K>(req).await
    }

    /// Replace every field on the status object
    ///
    /// This works similarly to the [`Api::replace`] method, but `.spec` is ignored.
    /// You can leave out the `.spec` entirely from the serialized output.
    ///
    /// ```no_run
    /// use kube::{api::{Api, PostParams}, Client};
    /// use k8s_openapi::api::batch::v1::{Job, JobStatus};
    /// #[tokio::main]
    /// async fn main() -> Result<(), kube::Error> {
    ///     let client = Client::try_default().await?;
    ///     let jobs: Api<Job> = Api::namespaced(client, "apps");
    ///     let mut o = jobs.get_status("baz").await?; // retrieve partial object
    ///     o.status = Some(JobStatus::default()); // update the job part
    ///     let pp = PostParams::default();
    ///     let o = jobs.replace_status("baz", &pp, serde_json::to_vec(&o)?).await?;
    ///     Ok(())
    /// }
    /// ```
    pub async fn replace_status(&self, name: &str, pp: &PostParams, data: Vec<u8>) -> Result<K> {
        let req = self.resource.replace_status(name, &pp, data)?;
        self.client.request::<K>(req).await
    }
}

// ----------------------------------------------------------------------------
// Log subresource
// ----------------------------------------------------------------------------

/// Params for logging
#[derive(Default, Clone, Debug)]
pub struct LogParams {
    /// The container for which to stream logs. Defaults to only container if there is one container in the pod.
    pub container: Option<String>,
    /// Follow the log stream of the pod. Defaults to `false`.
    pub follow: bool,
    /// If set, the number of bytes to read from the server before terminating the log output.
    /// This may not display a complete final line of logging, and may return slightly more or slightly less than the specified limit.
    pub limit_bytes: Option<i64>,
    /// If `true`, then the output is pretty printed.
    pub pretty: bool,
    /// Return previous terminated container logs. Defaults to `false`.
    pub previous: bool,
    /// A relative time in seconds before the current time from which to show logs.
    /// If this value precedes the time a pod was started, only logs since the pod start will be returned.
    /// If this value is in the future, no logs will be returned. Only one of sinceSeconds or sinceTime may be specified.
    pub since_seconds: Option<i64>,
    /// If set, the number of lines from the end of the logs to show.
    /// If not specified, logs are shown from the creation of the container or sinceSeconds or sinceTime
    pub tail_lines: Option<i64>,
    /// If `true`, add an RFC3339 or RFC3339Nano timestamp at the beginning of every line of log output. Defaults to `false`.
    pub timestamps: bool,
}

impl Resource {
    /// Get a pod logs
    pub fn logs(&self, name: &str, lp: &LogParams) -> Result<http::Request<Vec<u8>>> {
        let base_url = self.make_url() + "/" + name + "/" + "log?";
        let mut qp = url::form_urlencoded::Serializer::new(base_url);

        if let Some(container) = &lp.container {
            qp.append_pair("container", &container);
        }

        if lp.follow {
            qp.append_pair("follow", "true");
        }

        if let Some(lb) = &lp.limit_bytes {
            qp.append_pair("limitBytes", &lb.to_string());
        }

        if lp.pretty {
            qp.append_pair("pretty", "true");
        }

        if lp.previous {
            qp.append_pair("previous", "true");
        }

        if let Some(ss) = &lp.since_seconds {
            qp.append_pair("sinceSeconds", &ss.to_string());
        }

        if let Some(tl) = &lp.tail_lines {
            qp.append_pair("tailLines", &tl.to_string());
        }

        if lp.timestamps {
            qp.append_pair("timestamps", "true");
        }

        let urlstr = qp.finish();
        let req = http::Request::get(urlstr);
        req.body(vec![]).map_err(Error::HttpError)
    }
}

#[test]
fn log_path() {
    use crate::api::Resource;
    use k8s_openapi::api::core::v1 as corev1;
    let r = Resource::namespaced::<corev1::Pod>("ns");
    let lp = LogParams {
        container: Some("blah".into()),
        ..LogParams::default()
    };
    let req = r.logs("foo", &lp).unwrap();
    assert_eq!(req.uri(), "/api/v1/namespaces/ns/pods/foo/log?&container=blah");
}

/// Marker trait for objects that has logs
pub trait Loggable {}

impl Loggable for k8s_openapi::api::core::v1::Pod {}

impl<K> Api<K>
where
    K: DeserializeOwned + Loggable,
{
    /// Fetch logs as a string
    pub async fn logs(&self, name: &str, lp: &LogParams) -> Result<String> {
        let req = self.resource.logs(name, lp)?;
        Ok(self.client.request_text(req).await?)
    }

    /// Fetch logs as a stream of bytes
    pub async fn log_stream(&self, name: &str, lp: &LogParams) -> Result<impl Stream<Item = Result<Bytes>>> {
        let req = self.resource.logs(name, lp)?;
        Ok(self.client.request_text_stream(req).await?)
    }
}

// ----------------------------------------------------------------------------
// Eviction subresource
// ----------------------------------------------------------------------------

/// Params for evictable objects
#[derive(Default, Clone)]
pub struct EvictParams {
    /// How the eviction should occur
    pub delete_options: Option<DeleteParams>,
    /// How the http post should occur
    pub post_options: PostParams,
}

impl Resource {
    /// Create an eviction
    pub fn evict(&self, name: &str, ep: &EvictParams) -> Result<http::Request<Vec<u8>>> {
        let base_url = self.make_url() + "/" + name + "/" + "eviction?";
        // This is technically identical to Resource::create, but different url
        let pp = &ep.post_options;
        pp.validate()?;
        let mut qp = url::form_urlencoded::Serializer::new(base_url);
        if pp.dry_run {
            qp.append_pair("dryRun", "All");
        }
        let urlstr = qp.finish();
        // eviction body parameters are awkward, need metadata with name
        let data = serde_json::to_vec(&serde_json::json!({
            "delete_options": ep.delete_options,
            "metadata": { "name": name }
        }))?;
        let req = http::Request::post(urlstr);
        req.body(data).map_err(Error::HttpError)
    }
}

#[test]
fn evict_path() {
    use crate::api::Resource;
    use k8s_openapi::api::core::v1 as corev1;
    let r = Resource::namespaced::<corev1::Pod>("ns");
    let ep = EvictParams::default();
    let req = r.evict("foo", &ep).unwrap();
    assert_eq!(req.uri(), "/api/v1/namespaces/ns/pods/foo/eviction?");
}

/// Marker trait for objects that can be evicted
pub trait Evictable {}

impl Evictable for k8s_openapi::api::core::v1::Pod {}

impl<K> Api<K>
where
    K: DeserializeOwned + Evictable,
{
    /// Create an eviction
    pub async fn evict(&self, name: &str, ep: &EvictParams) -> Result<Status> {
        let req = self.resource.evict(name, ep)?;
        self.client.request::<Status>(req).await
    }
}

// ----------------------------------------------------------------------------
// Attach subresource
// ----------------------------------------------------------------------------
/// Parameters for attaching to a container in a Pod.
///
/// - One of `stdin`, `stdout`, or `stderr` must be `true`.
/// - `stderr` and `tty` cannot both be `true` because multiplexing is not supported with TTY.
#[cfg(feature = "ws")]
#[cfg_attr(docsrs, doc(cfg(feature = "ws")))]
pub struct AttachParams {
    /// The name of the container to attach.
    /// Defaults to the only container if there is only one container in the pod.
    pub container: Option<String>,
    /// Attach to the container's standard input. Defaults to `false`.
    ///
    /// Call [`AttachedProcess::stdin`] to obtain a writer.
    pub stdin: bool,
    /// Attach to the container's standard output. Defaults to `true`.
    ///
    /// Call [`AttachedProcess::stdout`] to obtain a reader.
    pub stdout: bool,
    /// Attach to the container's standard error. Defaults to `true`.
    ///
    /// Call [`AttachedProcess::stderr`] to obtain a reader.
    pub stderr: bool,
    /// Allocate TTY. Defaults to `false`.
    ///
    /// NOTE: Terminal resizing is not implemented yet.
    pub tty: bool,

    /// The maximum amount of bytes that can be written to the internal `stdin`
    /// pipe before the write returns `Poll::Pending`.
    /// Defaults to 1024.
    ///
    /// This is not sent to the server.
    pub max_stdin_buf_size: Option<usize>,
    /// The maximum amount of bytes that can be written to the internal `stdout`
    /// pipe before the write returns `Poll::Pending`.
    /// Defaults to 1024.
    ///
    /// This is not sent to the server.
    pub max_stdout_buf_size: Option<usize>,
    /// The maximum amount of bytes that can be written to the internal `stderr`
    /// pipe before the write returns `Poll::Pending`.
    /// Defaults to 1024.
    ///
    /// This is not sent to the server.
    pub max_stderr_buf_size: Option<usize>,
}

#[cfg(feature = "ws")]
impl Default for AttachParams {
    // Default matching the server's defaults.
    fn default() -> Self {
        Self {
            container: None,
            stdin: false,
            stdout: true,
            stderr: true,
            tty: false,
            max_stdin_buf_size: None,
            max_stdout_buf_size: None,
            max_stderr_buf_size: None,
        }
    }
}

#[cfg(feature = "ws")]
impl AttachParams {
    /// Default parameters for an tty exec with stdin and stdout
    pub fn interactive_tty() -> Self {
        Self {
            stdin: true,
            stdout: true,
            stderr: false,
            tty: true,
            ..Default::default()
        }
    }

    /// Specify the container to execute in.
    pub fn container<T: Into<String>>(mut self, container: T) -> Self {
        self.container = Some(container.into());
        self
    }

    /// Set `stdin` field.
    pub fn stdin(mut self, enable: bool) -> Self {
        self.stdin = enable;
        self
    }

    /// Set `stdout` field.
    pub fn stdout(mut self, enable: bool) -> Self {
        self.stdout = enable;
        self
    }

    /// Set `stderr` field.
    pub fn stderr(mut self, enable: bool) -> Self {
        self.stderr = enable;
        self
    }

    /// Set `tty` field.
    pub fn tty(mut self, enable: bool) -> Self {
        self.tty = enable;
        self
    }

    /// Set `max_stdin_buf_size` field.
    pub fn max_stdin_buf_size(mut self, size: usize) -> Self {
        self.max_stdin_buf_size = Some(size);
        self
    }

    /// Set `max_stdout_buf_size` field.
    pub fn max_stdout_buf_size(mut self, size: usize) -> Self {
        self.max_stdout_buf_size = Some(size);
        self
    }

    /// Set `max_stderr_buf_size` field.
    pub fn max_stderr_buf_size(mut self, size: usize) -> Self {
        self.max_stderr_buf_size = Some(size);
        self
    }

    fn validate(&self) -> Result<()> {
        if !self.stdin && !self.stdout && !self.stderr {
            return Err(Error::RequestValidation(
                "AttachParams: one of stdin, stdout, or stderr must be true".into(),
            ));
        }

        if self.stderr && self.tty {
            // Multiplexing is not supported with TTY
            return Err(Error::RequestValidation(
                "AttachParams: tty and stderr cannot both be true".into(),
            ));
        }

        Ok(())
    }

    fn append_to_url_serializer(&self, qp: &mut url::form_urlencoded::Serializer<String>) {
        if self.stdin {
            qp.append_pair("stdin", "true");
        }
        if self.stdout {
            qp.append_pair("stdout", "true");
        }
        if self.stderr {
            qp.append_pair("stderr", "true");
        }
        if self.tty {
            qp.append_pair("tty", "true");
        }
        if let Some(container) = &self.container {
            qp.append_pair("container", &container);
        }
    }
}

#[cfg(feature = "ws")]
#[cfg_attr(docsrs, doc(cfg(feature = "ws")))]
impl Resource {
    /// Attach to a pod
    pub fn attach(&self, name: &str, ap: &AttachParams) -> Result<http::Request<Vec<u8>>> {
        ap.validate()?;

        let base_url = self.make_url() + "/" + name + "/" + "attach?";
        let mut qp = url::form_urlencoded::Serializer::new(base_url);
        ap.append_to_url_serializer(&mut qp);

        let req = http::Request::get(qp.finish());
        req.body(vec![]).map_err(Error::HttpError)
    }
}

#[cfg(feature = "ws")]
#[test]
fn attach_path() {
    use crate::api::Resource;
    use k8s_openapi::api::core::v1 as corev1;
    let r = Resource::namespaced::<corev1::Pod>("ns");
    let ap = AttachParams {
        container: Some("blah".into()),
        ..AttachParams::default()
    };
    let req = r.attach("foo", &ap).unwrap();
    assert_eq!(
        req.uri(),
        "/api/v1/namespaces/ns/pods/foo/attach?&stdout=true&stderr=true&container=blah"
    );
}

/// Marker trait for objects that has attach
#[cfg(feature = "ws")]
#[cfg_attr(docsrs, doc(cfg(feature = "ws")))]
pub trait Attachable {}

#[cfg(feature = "ws")]
#[cfg_attr(docsrs, doc(cfg(feature = "ws")))]
impl Attachable for k8s_openapi::api::core::v1::Pod {}

#[cfg(feature = "ws")]
#[cfg_attr(docsrs, doc(cfg(feature = "ws")))]
impl<K> Api<K>
where
    K: Clone + DeserializeOwned + Attachable,
{
    /// Attach to pod
    pub async fn attach(&self, name: &str, ap: &AttachParams) -> Result<AttachedProcess> {
        let req = self.resource.attach(name, ap)?;
        let stream = self.client.connect(req).await?;
        Ok(AttachedProcess::new(stream, ap))
    }
}

// ----------------------------------------------------------------------------
// Exec subresource
// ----------------------------------------------------------------------------
#[cfg(feature = "ws")]
#[cfg_attr(docsrs, doc(cfg(feature = "ws")))]
impl Resource {
    /// Execute command in a pod
    pub fn exec<I, T>(&self, name: &str, command: I, ap: &AttachParams) -> Result<http::Request<Vec<u8>>>
    where
        I: IntoIterator<Item = T>,
        T: Into<String>,
    {
        ap.validate()?;

        let base_url = self.make_url() + "/" + name + "/" + "exec?";
        let mut qp = url::form_urlencoded::Serializer::new(base_url);
        ap.append_to_url_serializer(&mut qp);

        for c in command.into_iter() {
            qp.append_pair("command", &c.into());
        }

        let req = http::Request::get(qp.finish());
        req.body(vec![]).map_err(Error::HttpError)
    }
}

#[cfg(feature = "ws")]
#[test]
fn exec_path() {
    use crate::api::Resource;
    use k8s_openapi::api::core::v1 as corev1;
    let r = Resource::namespaced::<corev1::Pod>("ns");
    let ap = AttachParams {
        container: Some("blah".into()),
        ..AttachParams::default()
    };
    let req = r.exec("foo", vec!["echo", "foo", "bar"], &ap).unwrap();
    assert_eq!(
        req.uri(),
        "/api/v1/namespaces/ns/pods/foo/exec?&stdout=true&stderr=true&container=blah&command=echo&command=foo&command=bar"
    );
}

/// Marker trait for objects that has exec
#[cfg(feature = "ws")]
#[cfg_attr(docsrs, doc(cfg(feature = "ws")))]
pub trait Executable {}

#[cfg(feature = "ws")]
#[cfg_attr(docsrs, doc(cfg(feature = "ws")))]
impl Executable for k8s_openapi::api::core::v1::Pod {}

#[cfg(feature = "ws")]
#[cfg_attr(docsrs, doc(cfg(feature = "ws")))]
impl<K> Api<K>
where
    K: Clone + DeserializeOwned + Executable,
{
    /// Execute a command in a pod
    pub async fn exec<I, T>(&self, name: &str, command: I, ap: &AttachParams) -> Result<AttachedProcess>
    where
        I: IntoIterator<Item = T>,
        T: Into<String>,
    {
        let req = self.resource.exec(name, command, ap)?;
        let stream = self.client.connect(req).await?;
        Ok(AttachedProcess::new(stream, ap))
    }
}
