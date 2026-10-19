//! Closed public Container control models from the fixed cf producer.

use open_compute_core::containers::ContainerResources;
use open_compute_core::{ErrorCode, PlatformError};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Configuration {
    pub image: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vcpu: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk: Option<Disk>,
}

impl Configuration {
    pub(crate) fn resources(
        &self,
        policy: ContainerResources,
    ) -> Result<ContainerResources, PlatformError> {
        let (cpu_nanos, memory_bytes, disk_bytes) = match (
            &self.instance_type,
            self.vcpu,
            self.memory_mib,
            self.disk.as_ref(),
        ) {
            (Some(name), None, None, None) => {
                let resources =
                    open_compute_core::containers::ContainerInstanceProfile::parse(name)?
                        .resources(policy)?;
                self.image_digest()?;
                return Ok(resources);
            }
            (None, Some(cpu), Some(memory), Some(disk)) => {
                let nanos = cpu * 1_000_000_000.0;
                if !nanos.is_finite()
                    || nanos <= 0.0
                    || nanos >= i64::MAX as f64
                    || nanos.fract() != 0.0
                {
                    return Err(invalid());
                }
                (
                    nanos as u64,
                    memory.checked_mul(1 << 20).ok_or_else(invalid)?,
                    disk.bytes()?,
                )
            }
            _ => return Err(invalid()),
        };
        let resources = ContainerResources {
            cpu_nanos,
            memory_bytes,
            disk_bytes,
            pids: policy.pids,
            open_files: policy.open_files,
        };
        resources.validate(policy)?;
        self.image_digest()?;
        Ok(resources)
    }

    pub(crate) fn image_digest(&self) -> Result<[u8; 32], PlatformError> {
        open_compute_storage::containers::registry_image_digest(&self.image).map_err(|_| invalid())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Disk {
    pub size_mb: f64,
}
impl Disk {
    fn bytes(&self) -> Result<u64, PlatformError> {
        let bytes = self.size_mb * 1_000_000.0;
        if !bytes.is_finite() || bytes <= 0.0 || bytes >= i64::MAX as f64 || bytes.fract() != 0.0 {
            return Err(invalid());
        }
        Ok(bytes as u64)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ApplicationCreate {
    pub name: String,
    pub scheduling_policy: open_compute_storage::containers::ContainerSchedulingPolicy,
    pub configuration: Option<Configuration>,
    pub max_instances: Option<u32>,
    #[serde(default)]
    pub instances: Option<u32>,
    pub durable_objects: DurableObjects,
    pub constraints: Option<Constraints>,
    pub observability: Option<Observability>,
    #[serde(default)]
    pub rollout_active_grace_period: Option<u32>,
}

impl ApplicationCreate {
    pub(crate) fn validate(
        &self,
        policy: ContainerResources,
        max_instances: u32,
    ) -> Result<ContainerResources, PlatformError> {
        use open_compute_storage::containers::ContainerSchedulingPolicy;
        if self.name.is_empty()
            || self.name.len() > 63
            || !self
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(invalid());
        }
        match self.scheduling_policy {
            ContainerSchedulingPolicy::Default => {
                if self.instances.is_some_and(|value| value != 0)
                    || self
                        .max_instances
                        .is_none_or(|value| value == 0 || value > max_instances)
                    || self
                        .rollout_active_grace_period
                        .is_some_and(|value| value != 0)
                    || self
                        .constraints
                        .as_ref()
                        .is_some_and(|value| value.tiers != [1, 2])
                {
                    return Err(invalid());
                }
                self.configuration
                    .as_ref()
                    .ok_or_else(invalid)?
                    .resources(policy)
            }
            ContainerSchedulingPolicy::DurableObject => {
                if self.configuration.is_some()
                    || self.max_instances.is_some()
                    || self.instances.is_some()
                    || self.constraints.is_some()
                    || self.rollout_active_grace_period.is_some()
                {
                    return Err(invalid());
                }
                Ok(policy)
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ApplicationPatch {
    pub configuration: Option<Configuration>,
    pub max_instances: Option<u32>,
    pub constraints: Option<Constraints>,
    pub scheduling_policy: Option<String>,
    pub rollout_active_grace_period: Option<u32>,
    pub observability: Option<Observability>,
}
impl ApplicationPatch {
    pub(crate) fn validate(
        &self,
        policy: ContainerResources,
        max_instances: u32,
    ) -> Result<Option<ContainerResources>, PlatformError> {
        if self
            .scheduling_policy
            .as_deref()
            .is_some_and(|value| value != "default")
            || self
                .max_instances
                .is_some_and(|value| value == 0 || value > max_instances)
            || self
                .rollout_active_grace_period
                .is_some_and(|value| value != 0)
            || self
                .constraints
                .as_ref()
                .is_some_and(|value| value.tiers != [1, 2])
        {
            return Err(invalid());
        }
        self.configuration
            .as_ref()
            .map(|configuration| configuration.resources(policy))
            .transpose()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DurableObjects {
    pub namespace_id: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Constraints {
    pub tiers: Vec<u32>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Observability {
    pub logs: Logs,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Logs {
    pub enabled: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RolloutCreate {
    pub description: Option<String>,
    pub strategy: String,
    pub target_configuration: Configuration,
    pub steps: Option<Vec<RolloutStep>>,
    pub step_percentage: Option<u32>,
    pub kind: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RolloutStep {
    pub step_size: RolloutStepSize,
    pub description: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RolloutStepSize {
    pub percentage: u32,
}

impl RolloutCreate {
    pub(crate) fn validate(&self) -> Result<Vec<RolloutStep>, PlatformError> {
        let steps = match (&self.steps, self.step_percentage) {
            (Some(steps), None) => steps.clone(),
            (None, Some(percentage @ (5 | 10 | 20 | 25 | 50 | 100))) => (1..=100 / percentage)
                .map(|step| RolloutStep {
                    step_size: RolloutStepSize {
                        percentage: step * percentage,
                    },
                    description: None,
                })
                .collect(),
            _ => return Err(invalid()),
        };
        if self.strategy != "rolling"
            || self.kind != "full_auto"
            || steps.is_empty()
            || steps.len() > 100
            || self
                .description
                .as_ref()
                .is_some_and(|description| description.len() > 1024)
            || steps
                .last()
                .is_none_or(|step| step.step_size.percentage != 100)
            || steps.iter().any(|step| {
                !(1..=100).contains(&step.step_size.percentage)
                    || step
                        .description
                        .as_ref()
                        .is_some_and(|description| description.len() > 1024)
            })
            || steps
                .windows(2)
                .any(|pair| pair[0].step_size.percentage >= pair[1].step_size.percentage)
        {
            return Err(invalid());
        }
        self.target_configuration.image_digest()?;
        Ok(steps)
    }
}

pub(super) fn invalid() -> PlatformError {
    PlatformError::new(
        ErrorCode::ConfigInvalid,
        "invalid Container application request",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn policy() -> ContainerResources {
        ContainerResources {
            cpu_nanos: 4_000_000_000,
            memory_bytes: 12 << 30,
            disk_bytes: 20_000_000_000,
            pids: 128,
            open_files: 1024,
        }
    }
    #[test]
    fn container_configuration_normalizes_exact_resources_and_rejects_unsafe_images() {
        let mut configuration = Configuration {
            image: format!("registry/app@sha256:{}", "a".repeat(64)),
            instance_type: Some("lite".into()),
            vcpu: None,
            memory_mib: None,
            disk: None,
        };
        let resources = configuration.resources(policy()).unwrap();
        assert_eq!(resources.cpu_nanos, 62_500_000);
        assert_eq!(resources.memory_bytes, 256 << 20);
        assert_eq!(resources.disk_bytes, 2_000_000_000);
        for name in [
            "basic",
            "standard-1",
            "standard-2",
            "standard-3",
            "standard-4",
        ] {
            configuration.instance_type = Some(name.into());
            configuration.resources(policy()).unwrap();
        }
        configuration.instance_type = None;
        configuration.vcpu = Some(0.0625);
        configuration.memory_mib = Some(256);
        configuration.disk = Some(Disk { size_mb: 2000.0 });
        assert_eq!(configuration.resources(policy()).unwrap(), resources);
        for cpu in [0.0, -1.0, f64::INFINITY, f64::NAN, 0.000_000_000_1] {
            configuration.vcpu = Some(cpu);
            assert!(configuration.resources(policy()).is_err());
        }
        configuration.vcpu = Some(0.0625);
        configuration.instance_type = Some("lite".into());
        assert!(configuration.resources(policy()).is_err());
        configuration.instance_type = None;
        for image in [
            "registry/app:latest".into(),
            format!("registry/app@sha256:{}", "A".repeat(64)),
            format!("bad image@sha256:{}", "a".repeat(64)),
            format!("registry/镜像@sha256:{}", "a".repeat(64)),
        ] {
            configuration.image = image;
            assert!(configuration.resources(policy()).is_err());
        }
    }
    #[test]
    fn container_application_patch_preserves_omitted_fields_and_validates_present_fields() {
        for value in [
            serde_json::json!({}),
            serde_json::json!({"max_instances": 2}),
            serde_json::json!({"observability":{"logs":{"enabled":false}}}),
        ] {
            let patch: ApplicationPatch = serde_json::from_value(value).unwrap();
            assert!(patch.validate(policy(), 4).unwrap().is_none());
        }
        let patch: ApplicationPatch = serde_json::from_value(serde_json::json!({"configuration":{"image":format!("registry/app@sha256:{}", "a".repeat(64)), "instance_type":"lite"},"max_instances":3})).unwrap();
        assert_eq!(
            patch.validate(policy(), 4).unwrap().unwrap().cpu_nanos,
            62_500_000
        );
        for value in [
            serde_json::json!({"max_instances":0}),
            serde_json::json!({"max_instances":5}),
            serde_json::json!({"scheduling_policy":"regional"}),
            serde_json::json!({"rollout_active_grace_period":1}),
            serde_json::json!({"constraints":{"tiers":[1]}}),
        ] {
            let patch: ApplicationPatch = serde_json::from_value(value).unwrap();
            assert!(patch.validate(policy(), 4).is_err());
        }
        assert!(
            serde_json::from_value::<ApplicationPatch>(
                serde_json::json!({"configuration":{"instance_type":"lite"}})
            )
            .is_err()
        );
    }
    #[test]
    fn container_application_and_rollout_accept_fixed_cf_wire_and_reject_unimplemented_placement() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../test/conformance/fixtures/containers-cf-wire.json"
        ))
        .unwrap();
        let create: ApplicationCreate =
            serde_json::from_value(fixture["create"][2]["body"].clone()).unwrap();
        create.validate(policy(), 4).unwrap();
        let mut invalid_create = create.clone();
        invalid_create.instances = Some(1);
        assert!(invalid_create.validate(policy(), 4).is_err());
        assert!(create.validate(policy(), 3).is_err());
        let rollout: RolloutCreate =
            serde_json::from_value(fixture["update"][3]["body"].clone()).unwrap();
        rollout.validate().unwrap();
        let mut invalid_rollout = rollout.clone();
        invalid_rollout.steps.as_mut().unwrap()[0]
            .step_size
            .percentage = 100;
        assert!(invalid_rollout.validate().is_err());
        invalid_rollout = rollout.clone();
        invalid_rollout.kind = "manual".into();
        assert!(invalid_rollout.validate().is_err());
        let mut value = fixture["create"][2]["body"].clone();
        value["constraints"]["regions"] = serde_json::json!(["WNAM"]);
        assert!(serde_json::from_value::<ApplicationCreate>(value).is_err());
    }
}
