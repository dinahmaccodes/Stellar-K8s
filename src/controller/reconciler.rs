//! Main reconciler for StellarNode resources
//!
//! Implements the controller pattern using kube-rs runtime.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Service};
use kube::{
    api::{Api, Patch, PatchParams},
    client::Client,
    runtime::{
        controller::{Action, Controller},
        finalizer::{finalizer, Event},
        watcher::Config,
    },
    ResourceExt,
};
use tracing::{debug, error, info, instrument, warn};

use crate::crd::{NodeType, RolloutStrategy, StellarNode, StellarNodeStatus};
use crate::error::{Error, Result};

use super::finalizers::STELLAR_NODE_FINALIZER;
use super::health;
use super::resources;

/// Shared state for the controller
pub struct ControllerState {
    pub client: Client,
}

/// Main entry point to start the controller
pub async fn run_controller(state: Arc<ControllerState>) -> Result<()> {
    let client = state.client.clone();
    let stellar_nodes: Api<StellarNode> = Api::all(client.clone());

    info!("Starting StellarNode controller");

    // Verify CRD exists
    match stellar_nodes.list(&Default::default()).await {
        Ok(_) => info!("StellarNode CRD is available"),
        Err(e) => {
            error!(
                "StellarNode CRD not found. Please install the CRD first: {:?}",
                e
            );
            return Err(Error::ConfigError(
                "StellarNode CRD not installed".to_string(),
            ));
        }
    }

    Controller::new(stellar_nodes, Config::default())
        // Watch owned resources for changes
        .owns::<Deployment>(Api::all(client.clone()), Config::default())
        .owns::<StatefulSet>(Api::all(client.clone()), Config::default())
        .owns::<Service>(Api::all(client.clone()), Config::default())
        .owns::<PersistentVolumeClaim>(Api::all(client.clone()), Config::default())
        .shutdown_on_signal()
        .run(reconcile, error_policy, state)
        .for_each(|res| async move {
            match res {
                Ok(obj) => info!("Reconciled: {:?}", obj),
                Err(e) => error!("Reconcile error: {:?}", e),
            }
        })
        .await;

    Ok(())
}

/// The main reconciliation function
///
/// This function is called whenever:
/// - A StellarNode is created, updated, or deleted
/// - An owned resource (Deployment, Service, PVC) changes
/// - The requeue timer expires
#[instrument(skip(ctx), fields(name = %obj.name_any(), namespace = obj.namespace()))]
async fn reconcile(obj: Arc<StellarNode>, ctx: Arc<ControllerState>) -> Result<Action> {
    let client = ctx.client.clone();
    let namespace = obj.namespace().unwrap_or_else(|| "default".to_string());
    let api: Api<StellarNode> = Api::namespaced(client.clone(), &namespace);

    info!(
        "Reconciling StellarNode {}/{} (type: {:?})",
        namespace,
        obj.name_any(),
        obj.spec.node_type
    );

    // Use kube-rs built-in finalizer helper for clean lifecycle management
    finalizer(&api, STELLAR_NODE_FINALIZER, obj, |event| async {
        match event {
            Event::Apply(node) => apply_stellar_node(&client, &node).await,
            Event::Cleanup(node) => cleanup_stellar_node(&client, &node).await,
        }
    })
    .await
    .map_err(Error::from)
}

/// Apply/create/update the StellarNode resources
async fn apply_stellar_node(client: &Client, node: &StellarNode) -> Result<Action> {
    let namespace = node.namespace().unwrap_or_else(|| "default".to_string());
    let name = node.name_any();

    info!("Applying StellarNode: {}/{}", namespace, name);

    // Validate the spec
    if let Err(e) = node.spec.validate() {
        warn!("Validation failed for {}/{}: {}", namespace, name, e);
        update_status(client, node, "Failed", Some(&e), 0).await?;
        return Err(Error::ValidationError(e));
    }

    // Check if suspended
    if node.spec.suspended {
        info!("Node {}/{} is suspended, scaling to 0", namespace, name);
        update_status(client, node, "Suspended", Some("Node is suspended"), 0).await?;
        // Still create resources but with 0 replicas
    }

    // Update status to Creating
    update_status(client, node, "Creating", Some("Creating resources"), 0).await?;

    // 1. Create/update the PersistentVolumeClaim
    resources::ensure_pvc(client, node).await?;
    info!("PVC ensured for {}/{}", namespace, name);

    // 2. Create/update the ConfigMap for node configuration
    resources::ensure_config_map(client, node).await?;
    info!("ConfigMap ensured for {}/{}", namespace, name);

    // 3. Create/update the Deployment/StatefulSet based on node type
    match node.spec.node_type {
        NodeType::Validator => {
            // Validators use StatefulSet for stable identity
            resources::ensure_statefulset(client, node).await?;
            info!("StatefulSet ensured for validator {}/{}", namespace, name);
        }
        NodeType::Horizon | NodeType::SorobanRpc => {
            // Handle Canary Deployment
            if let RolloutStrategy::Canary(_) = &node.spec.strategy {
                // Determine if we are in a canary state
                let current_version = get_current_deployment_version(client, node).await?;
                if let Some(cv) = current_version {
                    if cv != node.spec.version {
                        // We have a version mismatch, ensure canary
                        info!("Canary version mismatch: spec={} current={}. Ensuring canary resources.", node.spec.version, cv);
                        
                        // We need a way to pass this state down without mutating the original node yet, 
                        // or we patch the status first.
                        // Let's assume the Reconciler will manage status in the next step.
                    }
                }
                
                resources::ensure_canary_deployment(client, node).await?;
                resources::ensure_canary_service(client, node).await?;
                
                // For canary, the main deployment should stay at the OLD version 
                // IF we are in the middle of a rollout.
                // This is tricky because ensure_deployment uses node.spec.version.
                
                if node.status.as_ref().and_then(|status| status.canary_version.as_ref()).is_some() {
                    let mut stable_node = node.clone();
                    // Recover the stable version from the existing deployment if possible
                    if let Some(cv) = get_current_deployment_version(client, node).await? {
                         stable_node.spec.version = cv;
                    }
                    resources::ensure_deployment(client, &stable_node).await?;
                } else {
                    resources::ensure_deployment(client, node).await?;
                }
            } else {
                // RPC nodes use Deployment for easy scaling
                resources::ensure_deployment(client, node).await?;
                info!("Deployment ensured for RPC node {}/{}", namespace, name);
                
                // Clean up canary resources if they exist
                resources::delete_canary_resources(client, node).await?;
            }
        }
    }

    // 4. Create/update the Service
    resources::ensure_service(client, node).await?;
    info!("Service ensured for {}/{}", namespace, name);


    // 5. Perform health check to determine if node is ready
    let health_result = health::check_node_health(client, node).await?;
    
    debug!(
        "Health check result for {}/{}: healthy={}, synced={}, message={}",
        namespace, name, health_result.healthy, health_result.synced, health_result.message
    );
    
    // Determine the phase based on health check
    let (phase, message) = if node.spec.suspended {
        ("Suspended", "Node is suspended".to_string())
    } else if !health_result.healthy {
        ("Creating", health_result.message.clone())
    } else if !health_result.synced {
        ("Syncing", health_result.message.clone())
    } else {
        ("Ready", "Node is healthy and synced".to_string())
    };
    
    // 6. Update status with health check results
    update_status_with_health(client, node, phase, Some(&message), &health_result).await?;
    
    info!(
        "Node {}/{} status updated to: {} - {}",
        namespace, name, phase, message
    );
    // 5. Create/update Ingress if configured
    resources::ensure_ingress(client, node).await?;
    info!("Ingress ensured for {}/{}", namespace, name);

    // 6. Create/update ServiceMonitor for Prometheus scraping (if autoscaling enabled)
    if node.spec.autoscaling.is_some() {
        resources::ensure_service_monitor(client, node).await?;
        info!("ServiceMonitor ensured for {}/{}", namespace, name);

        // 7. Create/update HPA for autoscaling
        resources::ensure_hpa(client, node).await?;
        info!("HPA ensured for {}/{}", namespace, name);
    }

    // 7. Create/update alerting rules
    resources::ensure_alerting(client, node).await?;
    info!("Alerting ensured for {}/{}", namespace, name);
    // 8. Fetch the ready replicas from Deployment/StatefulSet status
    let ready_replicas = get_ready_replicas(client, node).await.unwrap_or(0);
    let canary_ready_replicas = if node.status.as_ref().and_then(|status| status.canary_version.as_ref()).is_some() {
        get_canary_ready_replicas(client, node).await.unwrap_or(0)
    } else {
        0
    };

    // 9. Update status to Running with ready replica count
    let mut phase = if node.spec.suspended {
        "Suspended"
    } else if node.status.as_ref().and_then(|status| status.canary_version.as_ref()).is_some() {
        "Canary"
    } else {
        "Running"
    };

    // Automated Rollback logic
    let mut message = "Resources created successfully".to_string();
    if let RolloutStrategy::Canary(_) = &node.spec.strategy {
        if node.status.as_ref().and_then(|status| status.canary_version.as_ref()).is_some() {
            // Check canary health
            let canary_health = check_canary_health(client, node).await?;
            if !canary_health.healthy && health_result.healthy {
                // Canary is failing but stable is fine -> Rollback!
                warn!("Canary health check failed for {}/{}: {}. Initiating automated rollback.", namespace, name, canary_health.message);
                phase = "Rollback";
                message = format!("Canary failed: {}. Rolling back.", canary_health.message);
                
                // Reset canary version in status to trigger deletion of canary resources
                let mut updated_node = node.clone();
                if let Some(status) = &mut updated_node.status {
                    status.canary_version = None;
                }
                update_status_with_canary(
                    client,
                    &updated_node,
                    phase,
                    Some(&message),
                    ready_replicas,
                    0,
                    None
                ).await?;
                
                return Ok(Action::requeue(Duration::from_secs(5)));
            }
        }
    }

    update_status_with_canary(
        client,
        node,
        phase,
        Some(&message),
        ready_replicas,
        canary_ready_replicas,
        node.status.as_ref().and_then(|status| status.canary_version.clone()),
    )
    .await?;


    // Requeue based on current state
    let requeue_duration = if phase == "Ready" {
        // Check less frequently when ready
        Duration::from_secs(60)
    } else {
        // Check more frequently when syncing
        Duration::from_secs(15)
    };
    
    Ok(Action::requeue(requeue_duration))
}

/// Clean up resources when the StellarNode is deleted
async fn cleanup_stellar_node(client: &Client, node: &StellarNode) -> Result<Action> {
    let namespace = node.namespace().unwrap_or_else(|| "default".to_string());
    let name = node.name_any();

    info!("Cleaning up StellarNode: {}/{}", namespace, name);

    // Delete resources in reverse order of creation

    // 0. Delete Alerting
    if let Err(e) = resources::delete_alerting(client, node).await {
        warn!("Failed to delete alerting: {:?}", e);
    }

    // 1. Delete HPA (if autoscaling was configured)
    if let Err(e) = resources::delete_hpa(client, node).await {
        warn!("Failed to delete HPA: {:?}", e);
    }

    // 2. Delete ServiceMonitor (if autoscaling was configured)
    if let Err(e) = resources::delete_service_monitor(client, node).await {
        warn!("Failed to delete ServiceMonitor: {:?}", e);
    }

    // 3. Delete Ingress
    if let Err(e) = resources::delete_ingress(client, node).await {
        warn!("Failed to delete Ingress: {:?}", e);
    }

    // 4. Delete Service
    if let Err(e) = resources::delete_service(client, node).await {
        warn!("Failed to delete Service: {:?}", e);
    }

    // 5. Delete Deployment/StatefulSet
    if let Err(e) = resources::delete_workload(client, node).await {
        warn!("Failed to delete workload: {:?}", e);
    }

    // 6. Delete ConfigMap
    if let Err(e) = resources::delete_config_map(client, node).await {
        warn!("Failed to delete ConfigMap: {:?}", e);
    }

    // 7. Delete PVC based on retention policy
    if node.spec.should_delete_pvc() {
        info!(
            "Deleting PVC for node: {}/{} (retention policy: Delete)",
            namespace, name
        );
        if let Err(e) = resources::delete_pvc(client, node).await {
            warn!("Failed to delete PVC: {:?}", e);
        }
    } else {
        info!(
            "Retaining PVC for node: {}/{} (retention policy: Retain)",
            namespace, name
        );
    }

    info!("Cleanup complete for StellarNode: {}/{}", namespace, name);

    // Return await_change to signal finalizer completion
    Ok(Action::await_change())
}

/// Fetch the ready replicas from the Deployment or StatefulSet status
async fn get_ready_replicas(client: &Client, node: &StellarNode) -> Result<i32> {
    let namespace = node.namespace().unwrap_or_else(|| "default".to_string());
    let name = node.name_any();

    match node.spec.node_type {
        NodeType::Validator => {
            // Validators use StatefulSet
            let api: Api<StatefulSet> = Api::namespaced(client.clone(), &namespace);
            match api.get(&name).await {
                Ok(statefulset) => {
                    let ready_replicas = statefulset
                        .status
                        .as_ref()
                        .and_then(|s| s.ready_replicas)
                        .unwrap_or(0);
                    Ok(ready_replicas)
                }
                Err(e) => {
                    warn!("Failed to get StatefulSet {}/{}: {:?}", namespace, name, e);
                    Ok(0)
                }
            }
        }
        NodeType::Horizon | NodeType::SorobanRpc => {
            // RPC nodes use Deployment
            let api: Api<Deployment> = Api::namespaced(client.clone(), &namespace);
            match api.get(&name).await {
                Ok(deployment) => {
                    let ready_replicas = deployment
                        .status
                        .as_ref()
                        .and_then(|s| s.ready_replicas)
                        .unwrap_or(0);
                    Ok(ready_replicas)
                }
                Err(e) => {
                    warn!("Failed to get Deployment {}/{}: {:?}", namespace, name, e);
                    Ok(0)
                }
            }
        }
    }
}

/// Fetch the ready replicas for the canary deployment
async fn get_canary_ready_replicas(client: &Client, node: &StellarNode) -> Result<i32> {
    let namespace = node.namespace().unwrap_or_else(|| "default".to_string());
    let name = format!("{}-canary", node.name_any());

    let api: Api<Deployment> = Api::namespaced(client.clone(), &namespace);
    match api.get(&name).await {
        Ok(deployment) => {
            let ready_replicas = deployment
                .status
                .as_ref()
                .and_then(|s| s.ready_replicas)
                .unwrap_or(0);
            Ok(ready_replicas)
        }
        Err(_) => Ok(0),
    }
}

/// Get the current version of the stable deployment
async fn get_current_deployment_version(client: &Client, node: &StellarNode) -> Result<Option<String>> {
    let namespace = node.namespace().unwrap_or_else(|| "default".to_string());
    let name = node.name_any();

    let api: Api<Deployment> = Api::namespaced(client.clone(), &namespace);
    match api.get(&name).await {
        Ok(deployment) => {
            let version = deployment
                .spec
                .as_ref()
                .and_then(|s| s.template.spec.as_ref())
                .and_then(|ts| ts.containers.first())
                .and_then(|c| c.image.as_ref())
                .and_then(|img| img.split(':').last())
                .map(|v| v.to_string());
            Ok(version)
        }
        Err(_) => Ok(None),
    }
}

/// Check health of canary pods
async fn check_canary_health(client: &Client, node: &StellarNode) -> Result<health::HealthCheckResult> {
    let _namespace = node.namespace().unwrap_or_else(|| "default".to_string());
    let name = format!("{}-canary", node.name_any());
    
    // Create a temporary node with the canary name to use the existing health check logic
    let mut canary_node = node.clone();
    canary_node.metadata.name = Some(name);
    
    health::check_node_health(client, &canary_node).await
}

/// Update the status subresource of a StellarNode
async fn update_status(
    client: &Client,
    node: &StellarNode,
    phase: &str,
    message: Option<&str>,
    ready_replicas: i32,
) -> Result<()> {
    let namespace = node.namespace().unwrap_or_else(|| "default".to_string());
    let api: Api<StellarNode> = Api::namespaced(client.clone(), &namespace);

    let status = StellarNodeStatus {
        phase: phase.to_string(),
        message: message.map(String::from),
        observed_generation: node.metadata.generation,
        replicas: if node.spec.suspended {
            0
        } else {
            node.spec.replicas
        },
        ready_replicas,
        ..Default::default()
    };

    let patch = serde_json::json!({ "status": status });
    api.patch_status(
        &node.name_any(),
        &PatchParams::apply("stellar-operator"),
        &Patch::Merge(&patch),
    )
    .await
    .map_err(Error::KubeError)?;

    Ok(())
}

/// Update the status subresource with health check results
async fn update_status_with_health(
    client: &Client,
    node: &StellarNode,
    phase: &str,
    message: Option<&str>,
    health: &health::HealthCheckResult,
) -> Result<()> {
    let namespace = node.namespace().unwrap_or_else(|| "default".to_string());
    let api: Api<StellarNode> = Api::namespaced(client.clone(), &namespace);

    // Build conditions based on health check
    let mut conditions = Vec::new();
    
    // Ready condition
    let ready_condition = if health.synced {
        crate::crd::Condition::ready(
            true,
            "NodeSynced",
            "Node is fully synced and operational",
        )
    } else if health.healthy {
        crate::crd::Condition::ready(
            false,
            "NodeSyncing",
            &health.message,
        )
    } else {
        crate::crd::Condition::ready(
            false,
            "NodeNotHealthy",
            &health.message,
        )
    };
    conditions.push(ready_condition);
    
    // Progressing condition
    if !health.synced && health.healthy {
        conditions.push(crate::crd::Condition::progressing(
            "Syncing",
            &health.message,
        ));
    }

    let status = StellarNodeStatus {
        phase: phase.to_string(),
        message: message.map(String::from),
        observed_generation: node.metadata.generation,
        replicas: if node.spec.suspended { 0 } else { node.spec.replicas },
        ready_replicas: if health.synced && !node.spec.suspended {
            node.spec.replicas
        } else {
            0
        },
        ledger_sequence: health.ledger_sequence,
        conditions,
        ..Default::default()
    };

    let patch = serde_json::json!({ "status": status });
    api.patch_status(
        &node.name_any(),
        &PatchParams::apply("stellar-operator"),
        &Patch::Merge(&patch),
    )
    .await
    .map_err(Error::KubeError)?;

    Ok(())
}

/// Update the status subresource with canary information
async fn update_status_with_canary(
    client: &Client,
    node: &StellarNode,
    phase: &str,
    message: Option<&str>,
    ready_replicas: i32,
    canary_ready_replicas: i32,
    canary_version: Option<String>,
) -> Result<()> {
    let namespace = node.namespace().unwrap_or_else(|| "default".to_string());
    let api: Api<StellarNode> = Api::namespaced(client.clone(), &namespace);

    let status = StellarNodeStatus {
        phase: phase.to_string(),
        message: message.map(String::from),
        observed_generation: node.metadata.generation,
        replicas: if node.spec.suspended { 0 } else { node.spec.replicas },
        ready_replicas,
        canary_ready_replicas,
        canary_version,
        ..Default::default()
    };

    let patch = serde_json::json!({ "status": status });
    api.patch_status(
        &node.name_any(),
        &PatchParams::apply("stellar-operator"),
        &Patch::Merge(&patch),
    )
    .await
    .map_err(Error::KubeError)?;

    Ok(())
}

/// Error policy determines how to handle reconciliation errors
fn error_policy(node: Arc<StellarNode>, error: &Error, _ctx: Arc<ControllerState>) -> Action {
    error!("Reconciliation error for {}: {:?}", node.name_any(), error);

    // Use shorter retry for retriable errors
    let retry_duration = if error.is_retriable() {
        Duration::from_secs(15)
    } else {
        Duration::from_secs(60)
    };

    Action::requeue(retry_duration)
}
