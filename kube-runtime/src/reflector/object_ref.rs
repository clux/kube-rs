use derivative::Derivative;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::{DynamicObject, Meta};
use std::{
    fmt::{Debug, Display},
    hash::Hash,
};

#[derive(Derivative)]
#[derivative(Debug, PartialEq, Eq, Hash, Clone)]
/// A typed and namedspaced (if relevant) reference to a Kubernetes object
///
/// `K` may be either the object type or `DynamicObject`, in which case the
/// type is stored at runtime. Erased `ObjectRef`s pointing to different types
/// are still considered different.
///
/// ```
/// use kube_runtime::reflector::ObjectRef;
/// use k8s_openapi::api::core::v1::{ConfigMap, Secret};
/// assert_ne!(
///     ObjectRef::<ConfigMap>::new("a").erase(),
///     ObjectRef::<Secret>::new("a").erase(),
/// );
/// ```
pub struct ObjectRef<K: Meta>
where
    <K as Meta>::Family: Debug + Eq + Hash + Clone,
{
    family: K::Family,
    /// The name of the object
    pub name: String,
    /// The namespace of the object
    ///
    /// May only be `None` if the kind is cluster-scoped (not located in a namespace).
    /// Note that it *is* acceptable for an `ObjectRef` to a cluster-scoped resource to
    /// have a namespace. These are, however, not considered equal:
    ///
    /// ```
    /// # use kube_runtime::reflector::ObjectRef;
    /// # use k8s_openapi::api::core::v1::ConfigMap;
    /// assert_ne!(ObjectRef::<ConfigMap>::new("foo"), ObjectRef::new("foo").within("bar"));
    /// ```
    pub namespace: Option<String>,
}

impl<K: Meta> ObjectRef<K>
where
    <K as Meta>::Family: Debug + Eq + Hash + Clone + Default,
{
    #[must_use]
    pub fn new(name: &str) -> Self {
        Self::new_with(name, Default::default())
    }

    #[must_use]
    pub fn from_obj(obj: &K) -> Self
    where
        K: Meta,
    {
        Self::from_obj_with(obj, Default::default())
    }
}

impl<K: Meta> ObjectRef<K>
where
    <K as Meta>::Family: Debug + Eq + Hash + Clone,
{
    #[must_use]
    pub fn new_with(name: &str, family: K::Family) -> Self {
        Self {
            family,
            name: name.into(),
            namespace: None,
        }
    }

    #[must_use]
    pub fn within(mut self, namespace: &str) -> Self {
        self.namespace = Some(namespace.to_string());
        self
    }

    #[must_use]
    pub fn from_obj_with(obj: &K, f: K::Family) -> Self
    where
        K: Meta,
    {
        Self {
            family: f,
            name: obj.name(),
            namespace: obj.namespace(),
        }
    }

    #[must_use]
    /// Create an `ObjectRef` from an `OwnerReference`
    ///
    /// Returns `None` if the types do not match.
    pub fn from_owner_ref(
        namespace: Option<&str>,
        owner: &OwnerReference,
        family: K::Family,
    ) -> Option<Self> {
        if owner.api_version == K::api_version(&family) && owner.kind == K::kind(&family) {
            Some(Self {
                family,
                name: owner.name.clone(),
                namespace: namespace.map(String::from),
            })
        } else {
            None
        }
    }

    /// Convert into a reference to `K2`
    ///
    /// Note that no checking is done on whether this conversion makes sense. For example, every `Service`
    /// has a corresponding `Endpoints`, but it wouldn't make sense to convert a `Pod` into a `Deployment`.
    #[must_use]
    pub fn into_kind_unchecked<K2: Meta>(self, f2: K2::Family) -> ObjectRef<K2>
    where
        <K2 as Meta>::Family: Debug + Eq + Hash + Clone,
    {
        ObjectRef {
            family: f2,
            name: self.name,
            namespace: self.namespace,
        }
    }

    pub fn erase(self) -> ObjectRef<DynamicObject> {
        ObjectRef {
            family: kube::api::GroupVersionKind::from_dynamic_gvk(
                K::group(&self.family).as_ref(),
                K::version(&self.family).as_ref(),
                K::kind(&self.family).as_ref(),
                K::plural(&self.family).as_ref()
            ),
            name: self.name,
            namespace: self.namespace,
        }
    }
}

impl<K: Meta> Display for ObjectRef<K>
where
    <K as Meta>::Family: Debug + Eq + Hash + Clone,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}.{}.{}/{}",
            K::kind(&self.family),
            K::version(&self.family),
            K::group(&self.family),
            self.name
        )?;
        if let Some(namespace) = &self.namespace {
            write!(f, ".{}", namespace)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::ObjectRef;
    use k8s_openapi::api::{
        apps::v1::Deployment,
        core::v1::{Node, Pod},
    };

    #[test]
    fn display_should_follow_expected_format() {
        assert_eq!(
            format!("{}", ObjectRef::<Pod>::new("my-pod").within("my-namespace")),
            "Pod.v1./my-pod.my-namespace"
        );
        assert_eq!(
            format!(
                "{}",
                ObjectRef::<Deployment>::new("my-deploy").within("my-namespace")
            ),
            "Deployment.v1.apps/my-deploy.my-namespace"
        );
        assert_eq!(
            format!("{}", ObjectRef::<Node>::new("my-node")),
            "Node.v1./my-node"
        );
    }

    #[test]
    fn display_should_be_transparent_to_representation() {
        let pod_ref = ObjectRef::<Pod>::new("my-pod").within("my-namespace");
        assert_eq!(format!("{}", pod_ref), format!("{}", pod_ref.erase()));
        let deploy_ref = ObjectRef::<Deployment>::new("my-deploy").within("my-namespace");
        assert_eq!(format!("{}", deploy_ref), format!("{}", deploy_ref.erase()));
        let node_ref = ObjectRef::<Node>::new("my-node");
        assert_eq!(format!("{}", node_ref), format!("{}", node_ref.erase()));
    }
}
