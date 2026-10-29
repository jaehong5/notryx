use super::RuntimeError;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

#[derive(Default)]
pub(super) struct EventPermits {
    devices: Mutex<HashMap<Uuid, Uuid>>,
}

impl EventPermits {
    pub(super) fn acquire(
        self: &Arc<Self>,
        device: Uuid,
        principal: Uuid,
    ) -> Result<EventPermit, RuntimeError> {
        let mut devices = self.devices.lock().map_err(|_| RuntimeError::Worker)?;
        if devices.contains_key(&device)
            || devices.len() >= 20
            || devices.values().filter(|id| **id == principal).count() >= 5
        {
            return Err(RuntimeError::Busy);
        }
        devices.insert(device, principal);
        Ok(EventPermit {
            permits: Arc::clone(self),
            device,
        })
    }
}

pub(crate) struct EventPermit {
    permits: Arc<EventPermits>,
    device: Uuid,
}

impl Drop for EventPermit {
    fn drop(&mut self) {
        if let Ok(mut devices) = self.permits.devices.lock() {
            devices.remove(&self.device);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_device_principal_and_workspace_and_releases_permits() {
        let permits = Arc::new(EventPermits::default());
        let principal = Uuid::new_v4();
        let device = Uuid::new_v4();
        let first = permits.acquire(device, principal).unwrap();
        assert!(matches!(
            permits.acquire(device, principal),
            Err(RuntimeError::Busy)
        ));
        let mut held = vec![first];
        for _ in 0..4 {
            held.push(permits.acquire(Uuid::new_v4(), principal).unwrap());
        }
        assert!(matches!(
            permits.acquire(Uuid::new_v4(), principal),
            Err(RuntimeError::Busy)
        ));
        for _ in 0..3 {
            let other = Uuid::new_v4();
            for _ in 0..5 {
                held.push(permits.acquire(Uuid::new_v4(), other).unwrap());
            }
        }
        assert!(matches!(
            permits.acquire(Uuid::new_v4(), Uuid::new_v4()),
            Err(RuntimeError::Busy)
        ));
        drop(held.remove(0));
        assert!(permits.acquire(device, principal).is_ok());
    }
}
