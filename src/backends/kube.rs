//! The Kubernetes seam, realised with the `kube` crate.
//!
//! The GPU platform's host tenants are workloads whose pod template requests a
//! GPU resource. Taking one off the card is always two acts: tell Argo CD not
//! to heal the change, then scale the workload to zero. Returning it reverses
//! both in the same order. Workloads are read as dynamic objects so the driver
//! carries no generated Kubernetes types and no cluster-specific import.

use kube::Client;
use kube::api::{
    Api, ApiResource, DynamicObject, GroupVersionKind, ListParams, Patch, PatchParams,
};
use serde_json::{Value, json};

use crate::error::{ClnxError, Result};

/// The workload kinds that can own a GPU pod.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadKind {
    /// A Deployment.
    Deployment,
    /// A StatefulSet.
    StatefulSet,
}

impl WorkloadKind {
    /// The `apps/v1` group-version-kind of this workload kind.
    #[must_use]
    pub fn gvk(self) -> GroupVersionKind {
        let kind = match self {
            Self::Deployment => "Deployment",
            Self::StatefulSet => "StatefulSet",
        };
        GroupVersionKind::gvk("apps", "v1", kind)
    }
}

/// One workload that requests a GPU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workload {
    /// The namespace it lives in.
    pub namespace: String,
    /// Its kind.
    pub kind: WorkloadKind,
    /// Its name.
    pub name: String,
    /// The replicas it asks for now.
    pub replicas: u32,
}

/// The Kubernetes operations the host GPU platform needs.
pub trait Kube {
    /// Every workload that requests a GPU resource, across all namespaces.
    fn gpu_workloads(&self) -> Result<Vec<Workload>>;

    /// Set a workload's replica count.
    fn set_replicas(&self, workload: &Workload, replicas: u32) -> Result<()>;

    /// Set or clear Argo CD's skip-reconcile annotation on the Application
    /// that deploys a namespace. A cluster without that Application is not an
    /// error: there is nothing to pause.
    fn set_skip_reconcile(&self, namespace: &str, skip: bool) -> Result<()>;
}

/// The real Kubernetes client.
pub struct KubeClient {
    runtime: tokio::runtime::Runtime,
    client: Client,
    argocd_namespace: String,
}

impl std::fmt::Debug for KubeClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KubeClient")
            .field("argocd_namespace", &self.argocd_namespace)
            .finish_non_exhaustive()
    }
}

impl KubeClient {
    /// Connect with the in-cluster or kubeconfig configuration and Argo CD in
    /// its conventional `argocd` namespace.
    ///
    /// # Errors
    ///
    /// Fails when no configuration is found or the runtime cannot be created.
    pub fn connect() -> Result<Self> {
        Self::with_argocd_namespace("argocd")
    }

    /// Connect with an explicit Argo CD namespace.
    ///
    /// # Errors
    ///
    /// Fails when no configuration is found or the runtime cannot be created.
    pub fn with_argocd_namespace(argocd_namespace: impl Into<String>) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|error| ClnxError::backend("tokio", error.to_string()))?;
        let client = runtime
            .block_on(Client::try_default())
            .map_err(kube_error)?;
        Ok(Self {
            runtime,
            client,
            argocd_namespace: argocd_namespace.into(),
        })
    }

    fn block_on<F: std::future::Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }
}

impl Kube for KubeClient {
    fn gpu_workloads(&self) -> Result<Vec<Workload>> {
        let mut workloads =
            self.block_on(list_gpu_workloads(&self.client, WorkloadKind::Deployment))?;
        workloads
            .extend(self.block_on(list_gpu_workloads(&self.client, WorkloadKind::StatefulSet))?);
        workloads.sort_by(|a, b| {
            (&a.namespace, a.kind.gvk().kind, &a.name).cmp(&(
                &b.namespace,
                b.kind.gvk().kind,
                &b.name,
            ))
        });
        Ok(workloads)
    }

    fn set_replicas(&self, workload: &Workload, replicas: u32) -> Result<()> {
        self.block_on(set_replicas(&self.client, workload, replicas))
    }

    fn set_skip_reconcile(&self, namespace: &str, skip: bool) -> Result<()> {
        self.block_on(set_skip_reconcile(
            &self.client,
            &self.argocd_namespace,
            namespace,
            skip,
        ))
    }
}

async fn list_gpu_workloads(client: &Client, kind: WorkloadKind) -> Result<Vec<Workload>> {
    let resource = ApiResource::from_gvk(&kind.gvk());
    let api: Api<DynamicObject> = Api::all_with(client.clone(), &resource);
    let list = api.list(&ListParams::default()).await.map_err(kube_error)?;
    let mut workloads = Vec::new();
    for object in list.items {
        if !requests_gpu(&object.data) {
            continue;
        }
        let namespace = object.metadata.namespace.unwrap_or_default();
        let name = object.metadata.name.unwrap_or_default();
        if namespace.is_empty() || name.is_empty() {
            continue;
        }
        let replicas = object
            .data
            .pointer("/spec/replicas")
            .and_then(Value::as_u64)
            .unwrap_or(1);
        workloads.push(Workload {
            namespace,
            kind,
            name,
            replicas: u32::try_from(replicas).unwrap_or(u32::MAX),
        });
    }
    Ok(workloads)
}

async fn set_replicas(client: &Client, workload: &Workload, replicas: u32) -> Result<()> {
    let resource = ApiResource::from_gvk(&workload.kind.gvk());
    let api: Api<DynamicObject> =
        Api::namespaced_with(client.clone(), &workload.namespace, &resource);
    let patch = json!({ "spec": { "replicas": replicas } });
    api.patch(
        &workload.name,
        &PatchParams::default(),
        &Patch::Merge(&patch),
    )
    .await
    .map(|_object| ())
    .map_err(kube_error)
}

async fn set_skip_reconcile(
    client: &Client,
    argocd_namespace: &str,
    namespace: &str,
    skip: bool,
) -> Result<()> {
    let resource = ApiResource::from_gvk_with_plural(
        &GroupVersionKind::gvk("argoproj.io", "v1alpha1", "Application"),
        "applications",
    );
    let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), argocd_namespace, &resource);
    let value = if skip { json!("true") } else { Value::Null };
    let patch = json!({
        "metadata": { "annotations": { "argocd.argoproj.io/skip-reconcile": value } }
    });
    match api
        .patch(namespace, &PatchParams::default(), &Patch::Merge(&patch))
        .await
    {
        Ok(_object) => Ok(()),
        Err(kube::Error::Api(response)) if response.code == 404 => Ok(()),
        Err(error) => Err(kube_error(error)),
    }
}

/// Whether a workload's pod template requests a GPU resource.
///
/// Mirrors the proven GPU tool: a `devic.es/*` resource other than `kvm`, or
/// any resource key that names a GPU, in limits or requests of any container
/// or init container.
fn requests_gpu(data: &Value) -> bool {
    let containers = data
        .pointer("/spec/template/spec/containers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    let init_containers = data
        .pointer("/spec/template/spec/initContainers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    containers.chain(init_containers).any(|container| {
        ["limits", "requests"].iter().any(|section| {
            container
                .pointer(&format!("/resources/{section}"))
                .and_then(Value::as_object)
                .is_some_and(|resources| {
                    resources.keys().any(|key| {
                        (key.starts_with("devic.es/") && key != "devic.es/kvm")
                            || key.to_ascii_lowercase().contains("gpu")
                    })
                })
        })
    })
}

fn kube_error(error: kube::Error) -> ClnxError {
    ClnxError::backend("kubernetes", error.to_string())
}
