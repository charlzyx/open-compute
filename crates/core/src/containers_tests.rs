use super::*;

fn resources() -> ContainerResources {
    ContainerResources {
        cpu_nanos: 62_500_000,
        memory_bytes: 256 * 1024 * 1024,
        disk_bytes: 1024 * 1024 * 1024,
        pids: 64,
        open_files: 1024,
    }
}

#[test]
fn resource_limits_reject_zero_overflow_and_each_exceeded_dimension() {
    let ceiling = resources();
    ceiling.validate(ceiling).unwrap();
    for dimension in 0..5 {
        for zero in [true, false] {
            let mut candidate = ceiling;
            match dimension {
                0 => candidate.cpu_nanos = if zero { 0 } else { ceiling.cpu_nanos + 1 },
                1 => candidate.memory_bytes = if zero { 0 } else { ceiling.memory_bytes + 1 },
                2 => candidate.disk_bytes = if zero { 0 } else { ceiling.disk_bytes + 1 },
                3 => candidate.pids = if zero { 0 } else { ceiling.pids + 1 },
                _ => candidate.open_files = if zero { 0 } else { ceiling.open_files + 1 },
            }
            assert_eq!(
                candidate.validate(ceiling).unwrap_err().code(),
                ErrorCode::ConfigInvalid
            );
        }
    }
    for dimension in 0..3 {
        let mut candidate = ceiling;
        match dimension {
            0 => candidate.cpu_nanos = u64::MAX,
            1 => candidate.disk_bytes = u64::MAX,
            _ => candidate.memory_bytes = u64::MAX,
        }
        assert!(candidate.validate(candidate).is_err());
    }
    let json = serde_json::to_value(ceiling).unwrap();
    assert_eq!(
        serde_json::from_value::<ContainerResources>(json.clone()).unwrap(),
        ceiling
    );
    let mut invalid = json;
    invalid["privileged"] = true.into();
    assert!(serde_json::from_value::<ContainerResources>(invalid).is_err());
}

#[test]
fn lease_states_round_trip_and_unknown_state_fails_closed() {
    for state in [
        ContainerLeaseState::Admitting,
        ContainerLeaseState::Starting,
        ContainerLeaseState::Running,
        ContainerLeaseState::Stopping,
        ContainerLeaseState::Stopped,
        ContainerLeaseState::Lost,
    ] {
        assert_eq!(ContainerLeaseState::parse(state.as_str()).unwrap(), state);
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(
            serde_json::from_str::<ContainerLeaseState>(&json).unwrap(),
            state
        );
    }
    assert!(ContainerLeaseState::parse("exited").is_err());
}

#[test]
fn named_profiles_use_one_closed_resource_contract() {
    let ceiling = ContainerResources {
        cpu_nanos: 8_000_000_000,
        memory_bytes: 32 << 30,
        disk_bytes: 40_000_000_000,
        pids: 64,
        open_files: 1024,
    };
    for name in [
        "lite",
        "basic",
        "standard-1",
        "standard-2",
        "standard-3",
        "standard-4",
    ] {
        let profile = ContainerInstanceProfile::parse(name).unwrap();
        assert_eq!(profile.as_str(), name);
        let encoded = serde_json::to_string(&profile).unwrap();
        assert_eq!(encoded, format!("\"{name}\""));
        assert_eq!(
            serde_json::from_str::<ContainerInstanceProfile>(&encoded).unwrap(),
            profile
        );
        let resources = profile.resources(ceiling).unwrap();
        assert_eq!(resources.pids, 64);
        assert_eq!(resources.open_files, 1024);
        let mut small = resources;
        small.cpu_nanos -= 1;
        assert!(profile.resources(small).is_err());
    }
    assert!(ContainerInstanceProfile::parse("Standard-1").is_err());
    assert!(serde_json::from_str::<ContainerInstanceProfile>("\"unknown\"").is_err());
    let lite = ContainerInstanceProfile::Lite.resources(ceiling).unwrap();
    assert_eq!(
        (lite.cpu_nanos, lite.memory_bytes, lite.disk_bytes),
        (62_500_000, 256 << 20, 2_000_000_000)
    );
}
