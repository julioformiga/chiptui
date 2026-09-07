//! OTA method registry.
//!
//! The single place that knows which drivers exist --- `backend::registry`'s
//! twin, one level down: everything else asks for a *method's* driver here,
//! so adding a mechanism means a driver module plus one entry, never a
//! `match` scattered through the app.

use super::mcumgr::McumgrDriver;
use super::{OtaMethod, OtaMethodDriver};

/// The drivers are unit structs, so the registry is a table of statics
/// rather than a `Vec<Box<..>>` --- there is nothing to own.
static DRIVERS: &[&dyn OtaMethodDriver] = &[&McumgrDriver];

/// Every registered driver, in registration order.
pub fn drivers() -> impl Iterator<Item = &'static dyn OtaMethodDriver> {
    DRIVERS.iter().copied()
}

/// The driver for `method` --- `None` only if a method was added without
/// its driver, which `every_method_registered_exactly_once` fails on.
pub fn driver_for(method: OtaMethod) -> Option<&'static dyn OtaMethodDriver> {
    drivers().find(|driver| driver.method() == method)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::ota::{OtaConfig, OtaContext, OtaStage};

    #[test]
    fn every_method_registered_exactly_once() {
        for method in OtaMethod::ALL {
            assert_eq!(
                drivers()
                    .filter(|driver| driver.method() == *method)
                    .count(),
                1,
                "{} must be registered exactly once",
                method.id()
            );
        }
        assert_eq!(drivers().count(), OtaMethod::ALL.len());
    }

    #[test]
    fn every_driver_refuses_a_stage_it_does_not_declare() {
        let target = OtaConfig {
            address: Some("192.168.1.42".to_string()),
            ..OtaConfig::default()
        };
        let ctx = OtaContext {
            target: &target,
            image: Path::new("build/zephyr/zephyr.signed.bin"),
            slot_hash: None,
            tool: "smpmgr",
        };
        for driver in drivers() {
            for stage in OtaStage::ALL {
                if !driver.stages().contains(stage) {
                    assert!(
                        driver.stage_command(*stage, &ctx).is_err(),
                        "{} improvises a '{}' stage it does not declare",
                        driver.method().id(),
                        stage.label()
                    );
                }
            }
        }
    }

    #[test]
    fn every_declared_stage_builds_or_refuses_but_never_panics() {
        // With every question answered and with none answered: both walks
        // must come back with a command or a named refusal, stage by stage.
        let answered = OtaConfig {
            address: Some("192.168.1.42".to_string()),
            ..OtaConfig::default()
        };
        let unanswered = OtaConfig::default();
        for driver in drivers() {
            for target in [&answered, &unanswered] {
                for slot_hash in [None, Some("AABBCCDD")] {
                    let ctx = OtaContext {
                        target,
                        image: Path::new("build/zephyr/zephyr.signed.bin"),
                        slot_hash,
                        tool: "smpmgr",
                    };
                    for stage in driver.stages() {
                        let _ = driver.stage_command(*stage, &ctx);
                    }
                }
            }
        }
    }
}
